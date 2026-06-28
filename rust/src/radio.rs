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
#[cfg(any(feature = "central", feature = "l2cap"))]
use embassy_futures::select::select3;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_time::{Duration, Timer};
use embassy_time_driver::Driver;
use embassy_time_queue_utils::Queue;
use nrf_sdc::mpsl::MultiprotocolServiceLayer;
use trouble_host::attribute::{AttributeTable, Characteristic, CharacteristicProps, Service};
use trouble_host::prelude::*;
#[cfg(feature = "central")]
use trouble_host::connection::{ConnectConfig, ScanConfig};
#[cfg(feature = "central")]
use trouble_host::gatt::GattClient;
#[cfg(feature = "l2cap")]
use trouble_host::l2cap::{L2capChannel, L2capChannelConfig};

use crate::{
    RuntimeBleCharDef, RuntimeCfg, NUS_TX_CHR, SEND_BUF, SEND_BUF_CAP,
    SEND_CHR, SEND_LEN, SEND_REQ, UNLOAD_REQ,
};
#[cfg(feature = "central")]
use crate::{
    CCMD_CONNECT, CCMD_DISCONNECT, CCMD_DISCOVER, CCMD_NONE, CCMD_READ, CCMD_SCAN_START,
    CCMD_SCAN_STOP, CCMD_SUBSCRIBE, CCMD_WRITE, CENTRAL_ADDR, CENTRAL_CMD, CENTRAL_HANDLE,
    CENTRAL_UUID, CENTRAL_UUID_LEN,
};
#[cfg(feature = "l2cap")]
use crate::{L2CAP_SEND_BUF, L2CAP_SEND_LEN, L2CAP_SEND_REQ};

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
// The central-capable builds allow two simultaneous links so a device can be a
// peripheral (server) and a central (client) at the same time (RUNTIME_BLE_ROLE_DUAL).
#[cfg(feature = "central")]
const CONNECTIONS_MAX: usize = 2;
#[cfg(not(feature = "central"))]
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

/// Allocate one characteristic value buffer on the heap, record its raw pointer
/// for reclamation at unload, and hand the attribute table a `'static` mutable
/// slice into it. The pointers collected in `stores` are turned back into
/// `Box`es (and freed) by [`serve_session`] once the server/table are dropped,
/// so a load/unload cycle returns all of its RAM.
fn alloc_store(stores: &mut Vec<*mut [u8; VALUE_LEN]>) -> &'static mut [u8] {
    let ptr = Box::into_raw(Box::new([0u8; VALUE_LEN]));
    stores.push(ptr);
    // SAFETY: ptr is freshly allocated and uniquely owned; it is not aliased
    // until the matching Box::from_raw in serve_session's teardown.
    let arr: &'static mut [u8; VALUE_LEN] = unsafe { &mut *ptr };
    &mut arr[..]
}

/// The result of building the GATT: the server, every characteristic by flat
/// index (declaration order), the NUS RX/TX indices (usize::MAX if the user
/// provided their own services), and the heap buffers backing the characteristic
/// values (reclaimed at unload).
struct Gatt {
    server: Box<Srv>,
    chars: Vec<Characteristic<[u8; VALUE_LEN]>>,
    nus_rx: usize,
    nus_tx: usize,
    stores: Vec<*mut [u8; VALUE_LEN]>,
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
    let mut stores: Vec<*mut [u8; VALUE_LEN]> = Vec::new();
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
                alloc_store(&mut stores),
            )
            .build(),
        );
        nus_tx = chars.len();
        chars.push(
            svc.add_characteristic(
                Uuid::new_long(NUS_TX),
                CharacteristicProps::from(0x10), // notify
                [0u8; VALUE_LEN],
                alloc_store(&mut stores),
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
                    svc.add_characteristic(
                        cuuid,
                        map_props(c.props),
                        [0u8; VALUE_LEN],
                        alloc_store(&mut stores),
                    )
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
        stores,
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
        // Dual GAP role (RUNTIME_BLE_ROLE_DUAL): also run the central side
        // (connect + GATT client) so the device is a server (this advertise/serve
        // loop) AND a client at the same time, on two simultaneous links.
        #[cfg(feature = "central")]
        if cfg.role == 2 {
            select3(work, central_loop(stack, cfg), wait_unload()).await;
        } else {
            select(work, wait_unload()).await;
        }
        #[cfg(not(feature = "central"))]
        select(work, wait_unload()).await;
    };
    block_on(select(mpsl.run(), ble_main));

    // Teardown. Drop the server (and the attribute table it owns) and the
    // characteristic handles FIRST, so nothing references the value buffers
    // anymore, THEN reclaim those heap buffers — a load/unload cycle leaks no RAM.
    let Gatt { server, chars, stores, .. } = gatt;
    drop(server);
    drop(chars);
    for ptr in stores {
        // SAFETY: each ptr came from alloc_store (Box::into_raw); the table that
        // held the only `&'static mut` to it has just been dropped.
        unsafe {
            let _ = Box::from_raw(ptr);
        }
    }
}

async fn run_runner<C: Controller, P: PacketPool>(mut runner: Runner<'_, C, P>) {
    let _ = runner.run().await;
}

/// Run a peripheral connection: the GATT server, plus (when l2cap is enabled
/// and a PSM is configured) an L2CAP listener, concurrently.
async fn run_connection(
    conn: &Connection<'_, DefaultPacketPool>,
    server: &Srv,
    gatt: &Gatt,
    stack: &Stack<'_, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    cfg: &RuntimeCfg,
) {
    #[cfg(feature = "l2cap")]
    {
        if cfg.l2cap_psm != 0 {
            select(
                connection_task(conn, server, gatt, stack, cfg),
                l2cap_peripheral(stack, conn, cfg),
            )
            .await;
            return;
        }
    }
    connection_task(conn, server, gatt, stack, cfg).await;
}

// ---------------------------------------------------------------------------
// L2CAP connection-oriented channels. Compiled only with `--features l2cap`.
// Shared by both roles: the peripheral listens, the central creates.
// ---------------------------------------------------------------------------
#[cfg(feature = "l2cap")]
async fn l2cap_peripheral(
    stack: &Stack<'_, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    conn: &Connection<'_, DefaultPacketPool>,
    cfg: &RuntimeCfg,
) {
    let listener = L2capChannel::listen(stack, conn);
    loop {
        match listener.accept(&L2capChannelConfig::default()).await {
            Ok(ch) => l2cap_serve(stack, ch, cfg).await,
            Err(_) => Timer::after(Duration::from_millis(200)).await,
        }
        if UNLOAD_REQ.load(Ordering::Acquire) {
            break;
        }
    }
}

/// Pump an established L2CAP channel: deliver received SDUs to on_l2cap_data and
/// send queued SDUs (runtime_ble_l2cap_send), until the channel closes or unload.
#[cfg(feature = "l2cap")]
async fn l2cap_serve(
    stack: &Stack<'_, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    channel: L2capChannel<'_, DefaultPacketPool>,
    cfg: &RuntimeCfg,
) {
    if let Some(cb) = cfg.callbacks.on_l2cap_connected {
        cb(cfg.user);
    }
    L2CAP_SEND_REQ.store(false, Ordering::Release);
    let (mut writer, mut reader) = channel.split();

    let recv = async {
        loop {
            match reader.receive_sdu(stack).await {
                Ok(sdu) => {
                    let d = sdu.as_ref();
                    if let Some(cb) = cfg.callbacks.on_l2cap_data {
                        cb(d.as_ptr(), d.len(), cfg.user);
                    }
                }
                Err(_) => break,
            }
        }
    };
    let send = async {
        loop {
            if L2CAP_SEND_REQ.load(Ordering::Acquire) {
                let len = L2CAP_SEND_LEN.load(Ordering::Acquire).min(VALUE_LEN);
                let mut b = [0u8; VALUE_LEN];
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        core::ptr::addr_of!(L2CAP_SEND_BUF) as *const u8,
                        b.as_mut_ptr(),
                        len,
                    );
                }
                L2CAP_SEND_REQ.store(false, Ordering::Release);
                let _ = writer.send(stack, &b[..len]).await;
            }
            Timer::after(Duration::from_millis(20)).await;
        }
    };
    select3(recv, send, wait_unload()).await;
    if let Some(cb) = cfg.callbacks.on_l2cap_disconnected {
        cb(cfg.user);
    }
}

// ---------------------------------------------------------------------------
// Central role: scan/connect + GATT client. Compiled only with `--features
// central` (the SDC central support is added in chip/<soc>.rs::build_sdc).
// ---------------------------------------------------------------------------
#[cfg(feature = "central")]
type ClientP<'a> = GattClient<'a, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool, 4>;

/// One central session: connect (by config.peer_address or a queued connect
/// command), run a GATT client until disconnect/unload, repeat.
#[cfg(feature = "central")]
pub(crate) fn serve_central(
    mpsl: &MultiprotocolServiceLayer,
    stack: &Stack<'_, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    cfg: &RuntimeCfg,
) {
    // The host runner must run concurrently so HCI events (connection complete,
    // ATT responses, notifications) are processed while we connect and talk.
    let runner = stack.runner();
    block_on(select(
        mpsl.run(),
        select(run_runner(runner), central_loop(stack, cfg)),
    ));
}

#[cfg(feature = "central")]
async fn central_loop(
    stack: &Stack<'_, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    cfg: &RuntimeCfg,
) {
    let mut central = stack.central();
    let mut auto = !cfg.peer_address.is_null();

    loop {
        if UNLOAD_REQ.load(Ordering::Acquire) {
            return;
        }
        let addr = if auto {
            auto = false;
            let mut a = [0u8; 6];
            unsafe { core::ptr::copy_nonoverlapping(cfg.peer_address, a.as_mut_ptr(), 6) };
            a
        } else {
            match wait_connect_cmd().await {
                Some(a) => a,
                None => return, // unloaded
            }
        };

        let filt = [Address::random(addr)];
        let cc = ConnectConfig {
            scan_config: ScanConfig {
                filter_accept_list: &filt,
                ..Default::default()
            },
            connect_params: Default::default(),
        };
        log_str(cfg, "[central] connecting\0");
        match central.connect(&cc).await {
            Ok(conn) => {
                if let Some(cb) = cfg.callbacks.on_connected {
                    cb(cfg.user);
                }
                run_central_conn(stack, &conn, cfg).await;
                if let Some(cb) = cfg.callbacks.on_disconnected {
                    cb(0, cfg.user);
                }
            }
            Err(_) => {
                log_str(cfg, "[central] connect failed\0");
                Timer::after(Duration::from_secs(1)).await;
            }
        }
    }
}

/// Park (polling) until a CCMD_CONNECT is queued; returns its target address.
#[cfg(feature = "central")]
async fn wait_connect_cmd() -> Option<[u8; 6]> {
    loop {
        if UNLOAD_REQ.load(Ordering::Acquire) {
            return None;
        }
        let cmd = CENTRAL_CMD.load(Ordering::Acquire);
        if cmd == CCMD_CONNECT {
            CENTRAL_CMD.store(CCMD_NONE, Ordering::Release);
            let mut a = [0u8; 6];
            unsafe {
                core::ptr::copy_nonoverlapping(
                    core::ptr::addr_of!(CENTRAL_ADDR) as *const u8,
                    a.as_mut_ptr(),
                    6,
                );
            }
            return Some(a);
        }
        if cmd != CCMD_NONE {
            // Drop commands that need an active link while idle.
            CENTRAL_CMD.store(CCMD_NONE, Ordering::Release);
        }
        Timer::after(Duration::from_millis(30)).await;
    }
}

/// Run the GATT client on `conn`: a command pump (discover/read/write/subscribe/
/// disconnect) + a notification pump, until the link drops or unload.
#[cfg(feature = "central")]
async fn client_session(
    stack: &Stack<'_, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    conn: &Connection<'_, DefaultPacketPool>,
    cfg: &RuntimeCfg,
) {
    let client = match ClientP::new(stack, conn).await {
        Ok(c) => c,
        Err(_) => {
            log_str(cfg, "[central] gatt client init failed\0");
            return;
        }
    };
    let mut listener = client.listen_all().ok();
    // Remember (value handle, CCCD handle) of discovered characteristics so
    // subscribe() can write the right CCCD.
    let store: RefCell<heapless::Vec<(u16, Option<u16>), 8>> = RefCell::new(heapless::Vec::new());

    let commands = async {
        loop {
            match CENTRAL_CMD.swap(CCMD_NONE, Ordering::AcqRel) {
                CCMD_DISCONNECT => conn.disconnect(),
                CCMD_DISCOVER => client_discover(&client, &store, cfg).await,
                CCMD_READ => {
                    let h = CENTRAL_HANDLE.load(Ordering::Acquire) as u16;
                    let mut buf = [0u8; VALUE_LEN];
                    match client.read_handle(h, &mut buf).await {
                        Ok(n) => {
                            if let Some(cb) = cfg.callbacks.on_read {
                                cb(h, buf.as_ptr(), n.min(VALUE_LEN), cfg.user);
                            }
                        }
                        Err(_) => log_str(cfg, "[central] read failed\0"),
                    }
                }
                CCMD_WRITE => {
                    let h = CENTRAL_HANDLE.load(Ordering::Acquire) as u16;
                    let len = SEND_LEN.load(Ordering::Acquire).min(VALUE_LEN).min(SEND_BUF_CAP);
                    let mut wbuf = [0u8; VALUE_LEN];
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            core::ptr::addr_of!(SEND_BUF) as *const u8,
                            wbuf.as_mut_ptr(),
                            len,
                        );
                    }
                    let _ = client.write_handle(h, &wbuf[..len]).await;
                }
                CCMD_SUBSCRIBE => {
                    let h = CENTRAL_HANDLE.load(Ordering::Acquire) as u16;
                    let cccd = store
                        .borrow()
                        .iter()
                        .find(|(handle, _)| *handle == h)
                        .and_then(|(_, cccd)| *cccd)
                        .unwrap_or(h + 1);
                    let _ = client.write_handle(cccd, &[0x01, 0x00]).await;
                }
                CCMD_SCAN_START | CCMD_SCAN_STOP => {
                    log_str(cfg, "[central] scan not implemented in this build\0")
                }
                _ => {}
            }
            Timer::after(Duration::from_millis(20)).await;
        }
    };

    let notifs = async {
        match listener.as_mut() {
            Some(l) => loop {
                let n = l.next().await;
                if let Some(cb) = cfg.callbacks.on_notification {
                    let d = n.as_ref();
                    cb(n.handle(), d.as_ptr(), d.len(), cfg.user);
                }
            },
            None => core::future::pending::<()>().await,
        }
    };

    // client.task() drives ATT responses + notifications; it returns when the
    // link drops, ending the session.
    select3(client.task(), join(commands, notifs), wait_unload()).await;
}

/// Discover the characteristics of the service whose UUID is in CENTRAL_UUID;
/// report each via on_discovered and remember them (for subscribe's CCCD).
#[cfg(feature = "central")]
async fn client_discover(
    client: &ClientP<'_>,
    store: &RefCell<heapless::Vec<(u16, Option<u16>), 8>>,
    cfg: &RuntimeCfg,
) {
    let len = CENTRAL_UUID_LEN.load(Ordering::Acquire);
    let uuid = unsafe { uuid_from(core::ptr::addr_of!(CENTRAL_UUID) as *const u8, len as u8) };
    let svcs = match client.services_by_uuid(&uuid).await {
        Ok(s) => s,
        Err(_) => {
            log_str(cfg, "[central] discover: service not found\0");
            return;
        }
    };
    for s in svcs.iter() {
        if let Ok(chars) = client.characteristics::<8>(s).await {
            for c in chars.iter() {
                let _ = store.borrow_mut().push((c.handle, c.cccd_handle));
                if let Some(cb) = cfg.callbacks.on_discovered {
                    let raw = c.uuid.as_raw();
                    cb(c.handle, raw.as_ptr(), raw.len() as u8, 0, cfg.user);
                }
            }
        }
    }
}

/// Run a central connection: the GATT client, plus (when l2cap is enabled and a
/// PSM is configured) an L2CAP channel to the peer, concurrently.
#[cfg(feature = "central")]
async fn run_central_conn(
    stack: &Stack<'_, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    conn: &Connection<'_, DefaultPacketPool>,
    cfg: &RuntimeCfg,
) {
    #[cfg(feature = "l2cap")]
    {
        if cfg.l2cap_psm != 0 {
            select(
                client_session(stack, conn, cfg),
                l2cap_central(stack, conn, cfg),
            )
            .await;
            return;
        }
    }
    client_session(stack, conn, cfg).await;
}

/// Central side of L2CAP: open a channel to the peer's PSM, then pump it.
#[cfg(all(feature = "central", feature = "l2cap"))]
async fn l2cap_central(
    stack: &Stack<'_, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    conn: &Connection<'_, DefaultPacketPool>,
    cfg: &RuntimeCfg,
) {
    Timer::after(Duration::from_millis(400)).await; // let the link/MTU settle
    match L2capChannel::create(stack, conn, cfg.l2cap_psm, &L2capChannelConfig::default()).await {
        Ok(ch) => l2cap_serve(stack, ch, cfg).await,
        Err(_) => log_str(cfg, "[l2cap] create failed\0"),
    }
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
                run_connection(&conn, server, gatt, stack, cfg).await;
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
