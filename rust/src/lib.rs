//! runtime-ble — a trouble-based BLE peripheral runtime exposed as a C-ABI Rust
//! `staticlib` for linking into a Zephyr application (built with `CONFIG_BT=n`).
//!
//! - Owns the radio (nrf-sdc + MPSL). Only one BLE stack can own the radio, so
//!   the Zephyr app MUST build with `CONFIG_BT=n` / `CONFIG_MPSL=n`.
//! - Allocates its runtime state from the **Zephyr heap** (`k_aligned_alloc` /
//!   `k_free`) on [`runtime_ble_run`] and frees it on unload — it costs ~no RAM
//!   until loaded.
//! - Exposes a minimal generic GATT peripheral (a Nordic-UART-style service:
//!   one write RX characteristic + one notify TX characteristic). The app gets
//!   received bytes via the `on_data` callback and sends via [`runtime_ble_send`].
//! - The radio bring-up lives behind a per-chip feature (`nrf54l15`,
//!   `nrf52840`, …); the C-ABI/allocator/panic core below builds standalone.

#![no_std]
#![allow(dead_code)]

extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};
use core::ffi::{c_char, c_int, c_void};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// ---- Zephyr-provided externs (resolved when linked into the Zephyr image) ----
extern "C" {
    /// Aligned allocation from the Zephyr system heap.
    fn k_aligned_alloc(align: usize, size: usize) -> *mut c_void;
    /// Free a `k_aligned_alloc`/`k_malloc` pointer.
    fn k_free(ptr: *mut c_void);
    /// App-provided fatal handler: log `msg` and halt (never returns).
    fn runtime_ble_fatal(msg: *const c_char) -> !;
}

// ---- Global allocator backed by the Zephyr heap ----
struct ZephyrHeap;

unsafe impl GlobalAlloc for ZephyrHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let align = layout.align().max(core::mem::size_of::<usize>());
        k_aligned_alloc(align, layout.size()) as *mut u8
    }
    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        k_free(ptr as *mut c_void);
    }
}

#[global_allocator]
static HEAP: ZephyrHeap = ZephyrHeap;

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    use core::fmt::Write;
    let mut s: heapless::String<160> = heapless::String::new();
    let _ = s.push_str("runtime-ble panic");
    if let Some(loc) = info.location() {
        let _ = write!(s, " @ {}:{}", loc.file(), loc.line());
    }
    let _ = s.push('\0');
    unsafe { runtime_ble_fatal(s.as_ptr() as *const c_char) }
}

// ---- C ABI types (must match include/runtime_ble.h) ----

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RuntimeBleCallbacks {
    pub on_connected: Option<extern "C" fn(user: *mut c_void)>,
    pub on_disconnected: Option<extern "C" fn(reason: u8, user: *mut c_void)>,
    /// Bytes written by the peer to the built-in NUS RX characteristic.
    pub on_data: Option<extern "C" fn(data: *const u8, len: usize, user: *mut c_void)>,
    /// Peer wrote to a user-defined characteristic `chr` (flat index).
    pub on_write: Option<extern "C" fn(chr: u16, data: *const u8, len: usize, user: *mut c_void)>,
    /// Optional NUL-terminated text log line for the app's console.
    pub on_log: Option<extern "C" fn(line: *const c_char, user: *mut c_void)>,
}

/// C ABI: one characteristic (must match runtime_ble.h).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RuntimeBleCharDef {
    pub uuid: *const u8,
    pub uuid_len: u8,
    pub props: u16,
    pub max_len: u16,
}

/// C ABI: one service + its characteristics (must match runtime_ble.h).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RuntimeBleServiceDef {
    pub uuid: *const u8,
    pub uuid_len: u8,
    pub chars: *const RuntimeBleCharDef,
    pub num_chars: u8,
}

/// Advertising / GAP configuration. All fields are optional: a zeroed struct
/// gives a sensible default (connectable, general-discoverable, 30-60 ms, name
/// "RUNTIME-BLE", random-static address from hwinfo). Pointed-to data (name,
/// manufacturer_data, address) must outlive the session — use static storage.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RuntimeBleConfig {
    pub device_name: *const c_char,
    /// Company identifier for the manufacturer-specific AD (e.g. 0xFFFF).
    pub manufacturer_id: u16,
    /// Bytes after the company id in the manufacturer-specific AD (or null).
    pub manufacturer_data: *const u8,
    pub manufacturer_data_len: u16,
    /// Advertising interval window in milliseconds (0 -> 30 / 60 default).
    pub adv_interval_min_ms: u16,
    pub adv_interval_max_ms: u16,
    /// 0 = general discoverable (default), 1 = limited, 2 = non-discoverable.
    pub discoverable: u8,
    /// Optional custom 6-byte static-random address; null -> hwinfo-derived.
    pub address: *const u8,
    /// User-defined GATT (null/0 -> built-in NUS). Built at load time.
    pub services: *const RuntimeBleServiceDef,
    pub num_services: u8,
    pub callbacks: RuntimeBleCallbacks,
    pub user: *mut c_void,
}

/// Config captured at init, read once by the runtime thread.
#[derive(Clone, Copy)]
pub(crate) struct RuntimeCfg {
    pub device_name: *const c_char,
    pub manufacturer_id: u16,
    pub manufacturer_data: *const u8,
    pub manufacturer_data_len: u16,
    pub adv_interval_min_ms: u16,
    pub adv_interval_max_ms: u16,
    pub discoverable: u8,
    pub address: *const u8,
    pub services: *const RuntimeBleServiceDef,
    pub num_services: u8,
    pub callbacks: RuntimeBleCallbacks,
    pub user: *mut c_void,
}

// Set once in `runtime_ble_init` before `runtime_ble_run`; read by the thread.
static mut CONFIG: Option<RuntimeCfg> = None;

// Lock-free signals between caller threads and the runtime thread.
pub(crate) static UNLOAD_REQ: AtomicBool = AtomicBool::new(false);
pub(crate) static SEND_REQ: AtomicBool = AtomicBool::new(false);
pub(crate) static SEND_LEN: AtomicUsize = AtomicUsize::new(0);
/// One outstanding TX notification (raw, <= the TX characteristic value length).
pub(crate) const SEND_BUF_CAP: usize = 512;
pub(crate) static mut SEND_BUF: [u8; SEND_BUF_CAP] = [0; SEND_BUF_CAP];
/// Target characteristic for the queued TX: a flat index, or NUS_TX_CHR.
pub(crate) static SEND_CHR: AtomicUsize = AtomicUsize::new(NUS_TX_CHR);
/// Sentinel meaning "the built-in NUS TX characteristic".
pub(crate) const NUS_TX_CHR: usize = usize::MAX;

const RUNTIME_BLE_OK: c_int = 0;
const RUNTIME_BLE_ERR_INVALID: c_int = -1;
const RUNTIME_BLE_ERR_NO_MEM: c_int = -2;

/// Configure the library. Copies `cfg`. Call once before `runtime_ble_run`.
#[no_mangle]
pub extern "C" fn runtime_ble_init(cfg: *const RuntimeBleConfig) -> c_int {
    if cfg.is_null() {
        return RUNTIME_BLE_ERR_INVALID;
    }
    let c = unsafe { &*cfg };
    unsafe {
        CONFIG = Some(RuntimeCfg {
            device_name: c.device_name,
            manufacturer_id: c.manufacturer_id,
            manufacturer_data: c.manufacturer_data,
            manufacturer_data_len: c.manufacturer_data_len,
            adv_interval_min_ms: c.adv_interval_min_ms,
            adv_interval_max_ms: c.adv_interval_max_ms,
            discoverable: c.discoverable,
            address: c.address,
            services: c.services,
            num_services: c.num_services,
            callbacks: c.callbacks,
            user: c.user,
        });
    }
    RUNTIME_BLE_OK
}

/// Signal the running session to tear down. Called by glue's `runtime_ble_unload`
/// (C) before it joins the thread and frees the dynamic stack.
#[no_mangle]
pub extern "C" fn runtime_ble_signal_unload() {
    UNLOAD_REQ.store(true, Ordering::Release);
}

/// Queue one outstanding TX to characteristic `chr`. Single outstanding.
fn queue_tx(chr: usize, data: *const u8, len: usize) -> c_int {
    if data.is_null() || len == 0 || len > SEND_BUF_CAP {
        return RUNTIME_BLE_ERR_INVALID;
    }
    if SEND_REQ.load(Ordering::Acquire) {
        return RUNTIME_BLE_ERR_NO_MEM; // previous send not yet consumed
    }
    unsafe {
        core::ptr::copy_nonoverlapping(data, core::ptr::addr_of_mut!(SEND_BUF) as *mut u8, len);
    }
    SEND_CHR.store(chr, Ordering::Release);
    SEND_LEN.store(len, Ordering::Release);
    SEND_REQ.store(true, Ordering::Release);
    RUNTIME_BLE_OK
}

/// Queue one notification on the built-in NUS TX characteristic.
#[no_mangle]
pub extern "C" fn runtime_ble_send(data: *const u8, len: usize) -> c_int {
    queue_tx(NUS_TX_CHR, data, len)
}

/// Queue one notification/indication on a user-defined characteristic `chr`
/// (flat index in declaration order across config.services).
#[no_mangle]
pub extern "C" fn runtime_ble_notify(chr: u16, data: *const u8, len: usize) -> c_int {
    queue_tx(chr as usize, data, len)
}

/// Run ONE BLE session. Allocates everything on the heap, runs until
/// `runtime_ble_signal_unload`, frees, and RETURNS so the glue thread can exit
/// and free its dynamic stack. Called from the glue BLE thread.
#[no_mangle]
pub extern "C" fn runtime_ble_run(mode: c_int) {
    UNLOAD_REQ.store(false, Ordering::Release);
    SEND_REQ.store(false, Ordering::Release);
    #[cfg(feature = "_radio")]
    {
        // SAFETY: CONFIG is set once in runtime_ble_init before any session.
        let cfg = unsafe { core::ptr::addr_of!(CONFIG).as_ref().unwrap().as_ref() };
        radio::run(cfg, mode);
    }
    #[cfg(not(feature = "_radio"))]
    let _ = mode;
}

#[cfg(feature = "_radio")]
mod radio;
