//! radio.rs — chip-agnostic BLE runtime: a custom `block_on` executor parked on
//! a Zephyr semaphore, an embassy-time driver backed by the Zephyr clock, a
//! **user-defined GATT** built at load time from the C config (or a built-in
//! Nordic-UART service), and the advertise/connection loop.
//!
//! The chip-specific radio bring-up (MPSL/SDC peripherals + interrupt wiring)
//! lives in `chip/<soc>.rs`; each builds the `SoftdeviceController` + trouble
//! `Stack` and calls [`serve_session`].

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::future::Future;
use core::pin::pin;
use core::sync::atomic::Ordering;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use embassy_futures::join::join;
use embassy_futures::select::select;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_time::{Duration, Timer};
use embassy_time_driver::Driver;
use embassy_time_queue_utils::Queue;
use nrf_sdc::mpsl::MultiprotocolServiceLayer;
use trouble_host::attribute::{AttributeTable, Characteristic, CharacteristicProps, Service};
use trouble_host::prelude::*;

use crate::{
    RuntimeBleCharDef, RuntimeBleServiceDef, RuntimeCfg, NUS_TX_CHR, SEND_BUF, SEND_BUF_CAP,
    SEND_CHR, SEND_LEN, SEND_REQ, UNLOAD_REQ,
};

// Per-chip bring-up. Exactly one chip feature is enabled; `chip::run` is the
// entry called from lib.rs.
#[cfg(any(
    feature = "nrf54l15",
    feature = "nrf54l10",
    feature = "nrf54l05",
    feature = "nrf54lm20"
))]
#[path = "chip/nrf54l.rs"]
mod chip;
#[cfg(any(feature = "nrf52840", feature = "nrf52833", feature = "nrf52832"))]
#[path = "chip/nrf52.rs"]
mod chip;

pub(crate) use chip::run;

// ---------------------------------------------------------------------------
// Zephyr glue externs (provided by glue/glue.c).
// ---------------------------------------------------------------------------
extern "C" {
    fn runtime_uptime_ms() -> i64;
    fn runtime_alarm_set(at_ms: u64);
    fn runtime_ble_wait();
    fn runtime_ble_wake();
    fn runtime_ble_addr(out: *mut u8);
}

// ---------------------------------------------------------------------------
// Custom single-future executor: poll, then park on a Zephyr semaphore.
// ---------------------------------------------------------------------------
unsafe fn rw_clone(_: *const ()) -> RawWaker {
    RawWaker::new(core::ptr::null(), &VTABLE)
}
unsafe fn rw_wake(_: *const ()) {
    runtime_ble_wake();
}
unsafe fn rw_drop(_: *const ()) {}
static VTABLE: RawWakerVTable = RawWakerVTable::new(rw_clone, rw_wake, rw_wake, rw_drop);

fn block_on<F: Future>(f: F) -> F::Output {
    let waker = unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) };
    let mut cx = Context::from_waker(&waker);
    let mut f = pin!(f);
    loop {
        if let Poll::Ready(out) = f.as_mut().poll(&mut cx) {
            return out;
        }
        unsafe { runtime_ble_wait() };
    }
}

// ---------------------------------------------------------------------------
// embassy-time driver backed by the Zephyr clock (ms resolution).
// ---------------------------------------------------------------------------
struct ZephyrDriver {
    queue: critical_section::Mutex<RefCell<Queue>>,
}
embassy_time_driver::time_driver_impl!(static DRIVER: ZephyrDriver = ZephyrDriver {
    queue: critical_section::Mutex::new(RefCell::new(Queue::new())),
});
impl Driver for ZephyrDriver {
    fn now(&self) -> u64 {
        unsafe { runtime_uptime_ms() as u64 }
    }
    fn schedule_wake(&self, at: u64, waker: &Waker) {
        critical_section::with(|cs| {
            let mut q = self.queue.borrow(cs).borrow_mut();
            if q.schedule_wake(at, waker) {
                let next = q.next_expiration(self.now());
                unsafe { runtime_alarm_set(next) };
            }
        });
    }
}
#[no_mangle]
pub extern "C" fn runtime_alarm_fired() {
    let now = DRIVER.now();
    critical_section::with(|cs| {
        let mut q = DRIVER.queue.borrow(cs).borrow_mut();
        let next = q.next_expiration(now);
        unsafe { runtime_alarm_set(next) };
    });
}

// ---------------------------------------------------------------------------
// GATT: a runtime-built attribute table. Every characteristic is backed by a
// fixed VALUE_LEN byte buffer (heap), so the layout is decided at load time from
// the C config rather than by a compile-time macro.
// ---------------------------------------------------------------------------
pub(crate) const VALUE_LEN: usize = 244;
const ATT_MAX: usize = 64;
const CONNECTIONS_MAX: usize = 1;
const L2CAP_CHANNELS_MAX: usize = 2;

pub(crate) type Resources = HostResources<
    nrf_sdc::SoftdeviceController<'static>,
    DefaultPacketPool,
    CONNECTIONS_MAX,
    L2CAP_CHANNELS_MAX,
>;

type Srv = AttributeServer<'static, NoopRawMutex, DefaultPacketPool, ATT_MAX, CONNECTIONS_MAX>;

// Built-in Nordic UART Service UUIDs (little-endian 128-bit).
const NUS_SVC: [u8; 16] = [
    0x9e, 0xca, 0xdc, 0x24, 0x0e, 0xe5, 0xa9, 0xe0, 0x93, 0xf3, 0xa3, 0xb5, 0x01, 0x00, 0x40, 0x6e,
];
const NUS_RX: [u8; 16] = [
    0x9e, 0xca, 0xdc, 0x24, 0x0e, 0xe5, 0xa9, 0xe0, 0x93, 0xf3, 0xa3, 0xb5, 0x02, 0x00, 0x40, 0x6e,
];
const NUS_TX: [u8; 16] = [
    0x9e, 0xca, 0xdc, 0x24, 0x0e, 0xe5, 0xa9, 0xe0, 0x93, 0xf3, 0xa3, 0xb5, 0x03, 0x00, 0x40, 0x6e,
];

// C property bits (runtime_ble.h) -> BLE CharacteristicProp bits.
const C_PROP_READ: u16 = 1 << 0;
const C_PROP_WRITE: u16 = 1 << 1;
const C_PROP_WRITE_NR: u16 = 1 << 2;
const C_PROP_NOTIFY: u16 = 1 << 3;
const C_PROP_INDICATE: u16 = 1 << 4;

fn map_props(props: u16) -> CharacteristicProps {
    let mut b: u8 = 0;
    if props & C_PROP_READ != 0 {
        b |= 0x02; // Read
    }
    if props & C_PROP_WRITE_NR != 0 {
        b |= 0x04; // WriteWithoutResponse
    }
    if props & C_PROP_WRITE != 0 {
        b |= 0x08; // Write
    }
    if props & C_PROP_NOTIFY != 0 {
        b |= 0x10; // Notify
    }
    if props & C_PROP_INDICATE != 0 {
        b |= 0x20; // Indicate
    }
    CharacteristicProps::from(b)
}

unsafe fn uuid_from(ptr: *const u8, len: u8) -> Uuid {
    if len == 2 && !ptr.is_null() {
        Uuid::new_short(u16::from_le_bytes([*ptr, *ptr.add(1)]))
    } else if len == 16 && !ptr.is_null() {
        let mut b = [0u8; 16];
        core::ptr::copy_nonoverlapping(ptr, b.as_mut_ptr(), 16);
        Uuid::new_long(b)
    } else {
        Uuid::new_short(0)
    }
}

fn leak_store() -> &'static mut [u8] {
    Box::leak(Box::new([0u8; VALUE_LEN]))
}

/// The result of building the GATT: the server, every characteristic by flat
/// index (declaration order), and the NUS RX/TX indices (usize::MAX if the
/// user provided their own services).
struct Gatt {
    server: Box<Srv>,
    chars: Vec<Characteristic<[u8; VALUE_LEN]>>,
    nus_rx: usize,
    nus_tx: usize,
}

fn build_gatt(cfg: &RuntimeCfg, name: &'static str) -> Option<Gatt> {
    let mut table: AttributeTable<'static, NoopRawMutex, ATT_MAX> = AttributeTable::new();
    GapConfig::Peripheral(PeripheralConfig {
        name,
        appearance: &appearance::power_device::GENERIC_POWER_DEVICE,
    })
    .build(&mut table)
    .ok()?;

    let mut chars: Vec<Characteristic<[u8; VALUE_LEN]>> = Vec::new();
    let (mut nus_rx, mut nus_tx) = (usize::MAX, usize::MAX);

    if cfg.services.is_null() || cfg.num_services == 0 {
        // Built-in Nordic UART Service.
        let mut svc = table.add_service(Service::new(Uuid::new_long(NUS_SVC)));
        nus_rx = chars.len();
        chars.push(
            svc.add_characteristic(
                Uuid::new_long(NUS_RX),
                CharacteristicProps::from(0x04 | 0x08), // write_nr + write
                [0u8; VALUE_LEN],
                leak_store(),
            )
            .build(),
        );
        nus_tx = chars.len();
        chars.push(
            svc.add_characteristic(
                Uuid::new_long(NUS_TX),
                CharacteristicProps::from(0x10), // notify
                [0u8; VALUE_LEN],
                leak_store(),
            )
            .build(),
        );
    } else {
        let services =
            unsafe { core::slice::from_raw_parts(cfg.services, cfg.num_services as usize) };
        for s in services {
            let suuid = unsafe { uuid_from(s.uuid, s.uuid_len) };
            let mut svc = table.add_service(Service::new(suuid));
            let cdefs: &[RuntimeBleCharDef] =
                unsafe { core::slice::from_raw_parts(s.chars, s.num_chars as usize) };
            for c in cdefs {
                let cuuid = unsafe { uuid_from(c.uuid, c.uuid_len) };
                chars.push(
                    svc.add_characteristic(cuuid, map_props(c.props), [0u8; VALUE_LEN], leak_store())
                        .build(),
                );
            }
        }
    }

    Some(Gatt {
        server: Box::new(AttributeServer::new(table)),
        chars,
        nus_rx,
        nus_tx,
    })
}

pub(crate) fn log(cfg: &RuntimeCfg, msg: &core::ffi::CStr) {
    if let Some(cb) = cfg.callbacks.on_log {
        cb(msg.as_ptr(), cfg.user);
    }
}

fn log_str(cfg: &RuntimeCfg, s: &str) {
    if let Some(cb) = cfg.callbacks.on_log {
        cb(s.as_ptr() as *const core::ffi::c_char, cfg.user);
    }
}

pub(crate) unsafe fn cstr_or(p: *const core::ffi::c_char, default: &'static str) -> &'static str {
    if p.is_null() {
        return default;
    }
    core::ffi::CStr::from_ptr(p).to_str().unwrap_or(default)
}

/// The session's BLE address: a custom 6-byte address if configured, else the
/// per-device value from the glue (chip factory / hwinfo).
pub(crate) fn device_address(cfg: &RuntimeCfg) -> Address {
    let mut addr = [0u8; 6];
    if !cfg.address.is_null() {
        unsafe { core::ptr::copy_nonoverlapping(cfg.address, addr.as_mut_ptr(), 6) };
    } else {
        unsafe { runtime_ble_addr(addr.as_mut_ptr()) };
    }
    Address::random(addr)
}

// ---------------------------------------------------------------------------
// Session: build the GATT, run the host + advertise/connection loop until unload.
// ---------------------------------------------------------------------------
pub(crate) fn serve_session(
    mpsl: &MultiprotocolServiceLayer,
    stack: &Stack<'_, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    cfg: &RuntimeCfg,
) {
    let name = unsafe { cstr_or(cfg.device_name, "RUNTIME-BLE") };
    let gatt = match build_gatt(cfg, name) {
        Some(g) => g,
        None => return,
    };
    let server: &Srv = &gatt.server;

    let mut peripheral = stack.peripheral();
    let runner = stack.runner();
    let ble_main = async {
        let work = join(run_runner(runner), serve(&mut peripheral, &gatt, server, stack, cfg));
        select(work, wait_unload()).await;
    };
    block_on(select(mpsl.run(), ble_main));
    // Session torn down. (The per-characteristic value buffers are leaked for the
    // life of the firmware — fine for the load-once usage; one server instance.)
    drop(gatt);
}

async fn run_runner<C: Controller, P: PacketPool>(mut runner: Runner<'_, C, P>) {
    let _ = runner.run().await;
}

async fn wait_unload() {
    while !UNLOAD_REQ.load(Ordering::Acquire) {
        Timer::after(Duration::from_millis(50)).await;
    }
}

async fn serve(
    peripheral: &mut Peripheral<'_, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    gatt: &Gatt,
    server: &Srv,
    stack: &Stack<'_, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    cfg: &RuntimeCfg,
) {
    loop {
        match advertise(peripheral, cfg).await {
            Ok(conn) => {
                if let Some(cb) = cfg.callbacks.on_connected {
                    cb(cfg.user);
                }
                connection_task(&conn, server, gatt, stack, cfg).await;
                if let Some(cb) = cfg.callbacks.on_disconnected {
                    cb(0, cfg.user);
                }
            }
            Err(_) => Timer::after(Duration::from_secs(1)).await,
        }
    }
}

async fn advertise<'a>(
    peripheral: &mut Peripheral<'a, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    cfg: &RuntimeCfg,
) -> Result<Connection<'a, DefaultPacketPool>, BleHostError<nrf_sdc::Error>> {
    let name = unsafe { cstr_or(cfg.device_name, "RUNTIME-BLE") };
    let flags = if cfg.discoverable == 2 {
        BR_EDR_NOT_SUPPORTED
    } else {
        LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED
    };
    let man: &[u8] = if !cfg.manufacturer_data.is_null() && cfg.manufacturer_data_len > 0 {
        unsafe {
            core::slice::from_raw_parts(cfg.manufacturer_data, cfg.manufacturer_data_len as usize)
        }
    } else {
        &[]
    };
    let mut adv = [0u8; 31];
    let adv_len = AdStructure::encode_slice(
        &[
            AdStructure::Flags(flags),
            AdStructure::ManufacturerSpecificData {
                company_identifier: cfg.manufacturer_id,
                payload: man,
            },
            AdStructure::CompleteLocalName(name.as_bytes()),
        ],
        &mut adv[..],
    )?;
    let min_ms = if cfg.adv_interval_min_ms == 0 { 30 } else { cfg.adv_interval_min_ms } as u64;
    let max_ms = if cfg.adv_interval_max_ms == 0 { 60 } else { cfg.adv_interval_max_ms } as u64;
    let adv_params = AdvertisementParameters {
        interval_min: Duration::from_millis(min_ms),
        interval_max: Duration::from_millis(max_ms),
        ..Default::default()
    };
    let advertiser = peripheral
        .advertise(
            &adv_params,
            Advertisement::ConnectableScannableUndirected {
                adv_data: &adv[..adv_len],
                scan_data: &[],
            },
        )
        .await?;
    Ok(advertiser.accept().await?)
}

async fn connection_task(
    conn: &Connection<'_, DefaultPacketPool>,
    server: &Srv,
    gatt: &Gatt,
    stack: &Stack<'_, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    cfg: &RuntimeCfg,
) {
    let gconn = match conn.clone().with_attribute_server(server) {
        Ok(g) => g,
        Err(_) => return,
    };

    loop {
        match gconn.next().await {
            GattConnectionEvent::Disconnected { reason } => {
                use core::fmt::Write;
                let mut s: heapless::String<48> = heapless::String::new();
                let _ = write!(s, "[link] disconnected reason=0x{:02x}\0", reason.into_inner());
                log_str(cfg, &s);
                break;
            }
            GattConnectionEvent::RequestConnectionParams(req) => {
                let _ = req.accept(None, stack).await;
            }
            GattConnectionEvent::Gatt { event } => {
                // Copy an RX write out + find which characteristic, then accept,
                // then dispatch to the app callback.
                let mut buf = [0u8; VALUE_LEN];
                let mut n = 0usize;
                let mut idx = usize::MAX;
                if let GattEvent::Write(w) = &event {
                    let h = w.handle();
                    for (i, c) in gatt.chars.iter().enumerate() {
                        if c.handle == h {
                            idx = i;
                            break;
                        }
                    }
                    if idx != usize::MAX {
                        w.with_data(|_off, data| {
                            n = data.len().min(VALUE_LEN);
                            buf[..n].copy_from_slice(&data[..n]);
                        });
                    }
                }
                if let Ok(reply) = event.accept() {
                    let _ = reply.send().await;
                }
                if idx != usize::MAX {
                    if idx == gatt.nus_rx {
                        if let Some(cb) = cfg.callbacks.on_data {
                            cb(buf.as_ptr(), n, cfg.user);
                        }
                    } else if let Some(cb) = cfg.callbacks.on_write {
                        cb(idx as u16, buf.as_ptr(), n, cfg.user);
                    }
                }
                // Flush a queued TX (runtime_ble_send/notify) as one notify.
                if SEND_REQ.load(Ordering::Acquire) {
                    let chr = SEND_CHR.load(Ordering::Acquire);
                    let target = if chr == NUS_TX_CHR { gatt.nus_tx } else { chr };
                    let len = SEND_LEN.load(Ordering::Acquire).min(VALUE_LEN).min(SEND_BUF_CAP);
                    let mut txbuf = [0u8; VALUE_LEN];
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            core::ptr::addr_of!(SEND_BUF) as *const u8,
                            txbuf.as_mut_ptr(),
                            len,
                        );
                    }
                    SEND_REQ.store(false, Ordering::Release);
                    if let Some(c) = gatt.chars.get(target) {
                        let _ = c.notify_raw(&gconn, &txbuf[..len], false).await;
                    }
                }
            }
            _ => {}
        }
    }
}
