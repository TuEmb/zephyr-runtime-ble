//! radio.rs — chip-agnostic BLE runtime: a custom `block_on` executor parked on
//! a Zephyr semaphore, an embassy-time driver backed by the Zephyr clock, a
//! generic NUS-style GATT peripheral, and the advertise/connection loop.
//!
//! The chip-specific radio bring-up (MPSL/SDC peripherals + interrupt wiring)
//! lives in `chip/<soc>.rs`, selected by the per-chip Cargo feature. Each chip
//! module builds the `SoftdeviceController` + trouble `Stack` and then calls the
//! shared [`run_until_unload`] below.

use alloc::boxed::Box;
use core::cell::RefCell;
use core::future::Future;
use core::pin::pin;
use core::sync::atomic::Ordering;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use embassy_futures::join::join;
use embassy_futures::select::select;
use embassy_time::{Duration, Timer};
use embassy_time_driver::Driver;
use embassy_time_queue_utils::Queue;
use nrf_sdc::mpsl::MultiprotocolServiceLayer;
use trouble_host::prelude::*;

use crate::{RuntimeCfg, SEND_BUF, SEND_BUF_CAP, SEND_LEN, SEND_REQ, UNLOAD_REQ};

// Per-chip bring-up. Exactly one chip feature is enabled (Cargo enforces nothing,
// but the build selects one); `chip::run` is the entry called from lib.rs.
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
    fn runtime_ble_wait(); // k_sem_take(K_FOREVER) — yields to Zephyr
    fn runtime_ble_wake(); // k_sem_give — from wakers/ISRs
    fn runtime_ble_addr(out: *mut u8); // chip factory/derived BLE address, 6 bytes
}

// ---------------------------------------------------------------------------
// Custom single-future executor: poll, then park on a Zephyr semaphore. Any
// wake gives the sem; the loop re-polls. Returns when the future completes
// (i.e. on unload). The waker is used everywhere via Context.
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
// embassy-time driver backed by the Zephyr clock (ms resolution, tick-hz-1000).
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
// Generic GATT peripheral: a Nordic-UART-style service.
//   RX (6e400002): peer -> device, write / write-without-response.
//   TX (6e400003): device -> peer, notify.
// ---------------------------------------------------------------------------
pub(crate) const VALUE_LEN: usize = 244;
const CONNECTIONS_MAX: usize = 1;
const L2CAP_CHANNELS_MAX: usize = 2;

pub(crate) type Resources = HostResources<
    nrf_sdc::SoftdeviceController<'static>,
    DefaultPacketPool,
    CONNECTIONS_MAX,
    L2CAP_CHANNELS_MAX,
>;

#[gatt_server]
struct Server {
    nus: NusService,
}

#[gatt_service(uuid = "6e400001-b5a3-f393-e0a9-e50e24dcca9e")]
struct NusService {
    #[characteristic(
        uuid = "6e400002-b5a3-f393-e0a9-e50e24dcca9e",
        write_without_response,
        write,
        value = [0u8; VALUE_LEN]
    )]
    rx: [u8; VALUE_LEN],
    #[characteristic(
        uuid = "6e400003-b5a3-f393-e0a9-e50e24dcca9e",
        notify,
        value = [0u8; VALUE_LEN]
    )]
    tx: [u8; VALUE_LEN],
}

// The GATT server is built once and kept resident across load/unload cycles
// (the fork backs each characteristic with a shared static buffer guarded against
// a second live instance; one session runs at a time, so it is sound). It costs
// the attribute table (~1 KB) on the heap after the first load; the big resources
// (HostResources, SDC pool, thread stack) stay fully dynamic.
static mut SERVER_PTR: *mut Server<'static> = core::ptr::null_mut();

fn get_or_build_server(name: &'static str) -> Option<&'static Server<'static>> {
    unsafe {
        let slot = core::ptr::addr_of_mut!(SERVER_PTR);
        if (*slot).is_null() {
            let srv = Server::new_with_config(GapConfig::Peripheral(PeripheralConfig {
                name,
                appearance: &appearance::power_device::GENERIC_POWER_DEVICE,
            }))
            .ok()?;
            *slot = Box::into_raw(Box::new(srv));
        }
        Some(&*(*slot))
    }
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

// ---------------------------------------------------------------------------
// Shared session: built by the chip module, run here until unload.
// ---------------------------------------------------------------------------

/// Build (once) and return the resident GATT server for `name`.
pub(crate) fn server_for(name: &'static str) -> Option<&'static Server<'static>> {
    get_or_build_server(name)
}

/// The session's BLE address: the config's custom 6-byte address if set,
/// otherwise the per-device value from the glue (chip factory / hwinfo).
pub(crate) fn device_address(cfg: &RuntimeCfg) -> Address {
    let mut addr = [0u8; 6];
    if !cfg.address.is_null() {
        unsafe { core::ptr::copy_nonoverlapping(cfg.address, addr.as_mut_ptr(), 6) };
    } else {
        unsafe { runtime_ble_addr(addr.as_mut_ptr()) };
    }
    Address::random(addr)
}

/// Drive the host runner + advertise/connection loop until unload is signalled,
/// with MPSL running concurrently. Returns when the session is torn down.
pub(crate) fn run_until_unload(
    mpsl: &MultiprotocolServiceLayer,
    stack: &Stack<'_, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    server: &Server<'_>,
    cfg: &RuntimeCfg,
) {
    let mut peripheral = stack.peripheral();
    let runner = stack.runner();
    let ble_main = async {
        let work = join(run_runner(runner), serve(&mut peripheral, server, stack, cfg));
        select(work, wait_unload()).await;
    };
    block_on(select(mpsl.run(), ble_main));
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
    server: &Server<'_>,
    stack: &Stack<'_, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    cfg: &RuntimeCfg,
) {
    loop {
        match advertise(peripheral, cfg).await {
            Ok(conn) => {
                if let Some(cb) = cfg.callbacks.on_connected {
                    cb(cfg.user);
                }
                connection_task(server, &conn, stack, cfg).await;
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
        BR_EDR_NOT_SUPPORTED // non-discoverable
    } else {
        LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED
    };
    let man: &[u8] = if !cfg.manufacturer_data.is_null() && cfg.manufacturer_data_len > 0 {
        unsafe { core::slice::from_raw_parts(cfg.manufacturer_data, cfg.manufacturer_data_len as usize) }
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
    // Advertising interval from config (default fast 30-60 ms).
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
    server: &Server<'_>,
    conn: &Connection<'_, DefaultPacketPool>,
    stack: &Stack<'_, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    cfg: &RuntimeCfg,
) {
    let gatt = match conn.clone().with_attribute_server(server) {
        Ok(g) => g,
        Err(_) => return,
    };
    let tx = server.nus.tx;
    let rx_handle = server.nus.rx.handle;

    loop {
        match gatt.next().await {
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
                // Copy any RX write out, then accept the GATT event, then notify
                // the app — keeps the trouble event lifetime short.
                let mut buf = [0u8; VALUE_LEN];
                let mut n = 0usize;
                if let GattEvent::Write(w) = &event {
                    if w.handle() == rx_handle {
                        w.with_data(|_off, data| {
                            n = data.len().min(VALUE_LEN);
                            buf[..n].copy_from_slice(&data[..n]);
                        });
                    }
                }
                if let Ok(reply) = event.accept() {
                    let _ = reply.send().await;
                }
                if n > 0 {
                    if let Some(cb) = cfg.callbacks.on_data {
                        cb(buf.as_ptr(), n, cfg.user);
                    }
                }
                // Flush a queued app TX (runtime_ble_send) as a single notify.
                if SEND_REQ.load(Ordering::Acquire) {
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
                    let _ = tx.notify_raw(&gatt, &txbuf[..len], false).await;
                }
            }
            _ => {}
        }
    }
}
