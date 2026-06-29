// SPDX-License-Identifier: Apache-2.0
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
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

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
    /// Peer read a user-defined characteristic `chr`; fill out and return the byte count.
    pub on_read_value:
        Option<extern "C" fn(chr: u16, out: *mut u8, max_len: usize, user: *mut c_void) -> usize>,
    /// Central: a scan advertising report (addr is 6 bytes, LSB first).
    pub on_scan_result: Option<
        extern "C" fn(addr: *const u8, rssi: i8, adv: *const u8, adv_len: usize, user: *mut c_void),
    >,
    /// Central: scan report with address type (RUNTIME_BLE_ADDR_*).
    pub on_scan_result_ext: Option<
        extern "C" fn(
            addr: *const u8,
            addr_kind: u8,
            rssi: i8,
            adv: *const u8,
            adv_len: usize,
            user: *mut c_void,
        ),
    >,
    /// Central: a characteristic found by runtime_ble_client_discover.
    pub on_discovered: Option<
        extern "C" fn(handle: u16, uuid: *const u8, uuid_len: u8, props: u16, user: *mut c_void),
    >,
    /// Central: value returned by runtime_ble_client_read.
    pub on_read: Option<extern "C" fn(handle: u16, data: *const u8, len: usize, user: *mut c_void)>,
    /// Central: a notification/indication from a subscribed characteristic.
    pub on_notification:
        Option<extern "C" fn(handle: u16, data: *const u8, len: usize, user: *mut c_void)>,
    /// Peripheral: peer changed a server-side CCCD.
    pub on_subscription: Option<
        extern "C" fn(chr: u16, notify_enabled: u8, indicate_enabled: u8, user: *mut c_void),
    >,
    /// Link connection parameters changed.
    pub on_conn_params:
        Option<extern "C" fn(interval_ms: u16, latency: u16, timeout_ms: u16, user: *mut c_void)>,
    /// Link PHY changed; values use RUNTIME_BLE_PHY_*.
    pub on_phy_update: Option<extern "C" fn(tx_phy: u8, rx_phy: u8, user: *mut c_void)>,
    /// Link data length changed.
    pub on_data_length_update:
        Option<extern "C" fn(max_tx_octets: u16, max_rx_octets: u16, user: *mut c_void)>,
    /// Pairing/encryption event.
    pub on_security_event:
        Option<extern "C" fn(event: u8, level: u8, passkey: u32, flags: u8, user: *mut c_void)>,
    /// Security: load a persistent bond blob from an application slot.
    pub on_bond_load:
        Option<extern "C" fn(index: u8, out: *mut u8, max_len: usize, user: *mut c_void) -> usize>,
    /// Security: store a persistent bond blob to an application slot.
    pub on_bond_store:
        Option<extern "C" fn(index: u8, blob: *const u8, len: usize, user: *mut c_void)>,
    /// Security: provide local/peer OOB data when requested during pairing.
    pub on_oob_request: Option<
        extern "C" fn(
            local_random: *mut u8,
            local_confirm: *mut u8,
            peer_random: *mut u8,
            peer_confirm: *mut u8,
            user: *mut c_void,
        ) -> u8,
    >,
    /// Security: runtime-generated local OOB data after stack load.
    pub on_oob_local_data:
        Option<extern "C" fn(local_random: *const u8, local_confirm: *const u8, user: *mut c_void)>,
    /// Optional NUL-terminated text log line for the app's console.
    pub on_log: Option<extern "C" fn(line: *const c_char, user: *mut c_void)>,
    /// L2CAP: channel established.
    pub on_l2cap_connected: Option<extern "C" fn(user: *mut c_void)>,
    /// L2CAP: an SDU was received.
    pub on_l2cap_data: Option<extern "C" fn(data: *const u8, len: usize, user: *mut c_void)>,
    /// L2CAP: channel closed.
    pub on_l2cap_disconnected: Option<extern "C" fn(user: *mut c_void)>,
}

/// C ABI: one characteristic (must match runtime_ble.h).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RuntimeBleCharDef {
    pub uuid: *const u8,
    pub uuid_len: u8,
    pub props: u16,
    pub max_len: u16,
    pub permissions: u16,
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
    pub adv_service_uuid: *const u8,
    pub adv_service_uuid_len: u8,
    pub scan_response_data: *const u8,
    pub scan_response_data_len: u8,
    /// 1 = non-connectable advertising/beacon; 0 = connectable GATT server.
    pub nonconnectable: u8,
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
    /// Role: 0 = peripheral (default), 1 = central. See RUNTIME_BLE_ROLE_*.
    pub role: u8,
    /// Central only: optional 6-byte peer to auto-connect on load (null -> none).
    pub peer_address: *const u8,
    pub peer_address_kind: u8,
    pub l2cap_psm: u16,
    pub security_bondable: u8,
    pub security_request_on_connect: u8,
    pub security_oob_available: u8,
    pub bond_slot_count: u8,
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
    pub adv_service_uuid: *const u8,
    pub adv_service_uuid_len: u8,
    pub scan_response_data: *const u8,
    pub scan_response_data_len: u8,
    pub nonconnectable: u8,
    pub adv_interval_min_ms: u16,
    pub adv_interval_max_ms: u16,
    pub discoverable: u8,
    pub address: *const u8,
    pub services: *const RuntimeBleServiceDef,
    pub num_services: u8,
    pub role: u8,
    pub peer_address: *const u8,
    pub peer_address_kind: u8,
    pub l2cap_psm: u16,
    pub security_bondable: u8,
    pub security_request_on_connect: u8,
    pub security_oob_available: u8,
    pub bond_slot_count: u8,
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

// ---- Central command channel (single outstanding; consumed by the central loop) ----
pub(crate) const CCMD_NONE: u32 = 0;
pub(crate) const CCMD_SCAN_START: u32 = 1;
pub(crate) const CCMD_SCAN_STOP: u32 = 2;
pub(crate) const CCMD_CONNECT: u32 = 3;
pub(crate) const CCMD_DISCONNECT: u32 = 4;
pub(crate) const CCMD_DISCOVER: u32 = 5;
pub(crate) const CCMD_READ: u32 = 6;
pub(crate) const CCMD_WRITE: u32 = 7;
pub(crate) const CCMD_SUBSCRIBE: u32 = 8;
pub(crate) const CCMD_WRITE_NO_RSP: u32 = 9;
pub(crate) static CENTRAL_CMD: AtomicU32 = AtomicU32::new(CCMD_NONE);
/// Attribute handle (read/write/subscribe) for the pending command.
pub(crate) static CENTRAL_HANDLE: AtomicU32 = AtomicU32::new(0);
/// 6-byte peer address for CCMD_CONNECT.
pub(crate) static mut CENTRAL_ADDR: [u8; 6] = [0; 6];
/// RUNTIME_BLE_ADDR_* address kind for CCMD_CONNECT.
pub(crate) static CENTRAL_ADDR_KIND: AtomicUsize = AtomicUsize::new(0);
/// Service UUID (LE) + length for CCMD_DISCOVER.
pub(crate) static mut CENTRAL_UUID: [u8; 16] = [0; 16];
pub(crate) static CENTRAL_UUID_LEN: AtomicUsize = AtomicUsize::new(0);
pub(crate) static SCAN_ACTIVE: AtomicBool = AtomicBool::new(false);
pub(crate) static SCAN_INTERVAL_MS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static SCAN_WINDOW_MS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static SCAN_TIMEOUT_MS: AtomicUsize = AtomicUsize::new(0);
// CCMD_WRITE payload reuses SEND_BUF / SEND_LEN (a session is one role only).

// ---- L2CAP outbound SDU (single outstanding; consumed by the l2cap send pump) ----
pub(crate) static L2CAP_SEND_REQ: AtomicBool = AtomicBool::new(false);
pub(crate) static L2CAP_SEND_LEN: AtomicUsize = AtomicUsize::new(0);
pub(crate) static mut L2CAP_SEND_BUF: [u8; SEND_BUF_CAP] = [0; SEND_BUF_CAP];

// ---- Active-link control channel (peripheral or central connection) ----
pub(crate) const LCMD_NONE: u32 = 0;
pub(crate) const LCMD_SET_PHY: u32 = 1;
pub(crate) const LCMD_DLE: u32 = 2;
pub(crate) const LCMD_CONN_PARAMS: u32 = 3;
pub(crate) const LCMD_SECURITY_REQUEST: u32 = 4;
pub(crate) const LCMD_PASSKEY_CONFIRM: u32 = 5;
pub(crate) const LCMD_PASSKEY_CANCEL: u32 = 6;
pub(crate) const LCMD_PASSKEY_INPUT: u32 = 7;
pub(crate) static LINK_CMD: AtomicU32 = AtomicU32::new(LCMD_NONE);
pub(crate) static LINK_PHY: AtomicUsize = AtomicUsize::new(0);
pub(crate) static LINK_DLE_OCTETS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static LINK_DLE_TIME_US: AtomicUsize = AtomicUsize::new(0);
pub(crate) static LINK_CONN_MIN_MS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static LINK_CONN_MAX_MS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static LINK_CONN_LATENCY: AtomicUsize = AtomicUsize::new(0);
pub(crate) static LINK_CONN_TIMEOUT_MS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static LINK_PASSKEY: AtomicU32 = AtomicU32::new(0);

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
            adv_service_uuid: c.adv_service_uuid,
            adv_service_uuid_len: c.adv_service_uuid_len,
            scan_response_data: c.scan_response_data,
            scan_response_data_len: c.scan_response_data_len,
            nonconnectable: c.nonconnectable,
            adv_interval_min_ms: c.adv_interval_min_ms,
            adv_interval_max_ms: c.adv_interval_max_ms,
            discoverable: c.discoverable,
            address: c.address,
            services: c.services,
            num_services: c.num_services,
            role: c.role,
            peer_address: c.peer_address,
            peer_address_kind: c.peer_address_kind,
            l2cap_psm: c.l2cap_psm,
            security_bondable: c.security_bondable,
            security_request_on_connect: c.security_request_on_connect,
            security_oob_available: c.security_oob_available,
            bond_slot_count: c.bond_slot_count,
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

fn link_cmd(cmd: u32) -> c_int {
    if LINK_CMD.load(Ordering::Acquire) != LCMD_NONE {
        return RUNTIME_BLE_ERR_NO_MEM;
    }
    LINK_CMD.store(cmd, Ordering::Release);
    RUNTIME_BLE_OK
}

#[no_mangle]
pub extern "C" fn runtime_ble_set_phy(phy: u8) -> c_int {
    if phy != 1 && phy != 2 {
        return RUNTIME_BLE_ERR_INVALID;
    }
    LINK_PHY.store(phy as usize, Ordering::Release);
    link_cmd(LCMD_SET_PHY)
}

#[no_mangle]
pub extern "C" fn runtime_ble_update_data_length(tx_octets: u16, tx_time_us: u16) -> c_int {
    LINK_DLE_OCTETS.store(tx_octets as usize, Ordering::Release);
    LINK_DLE_TIME_US.store(tx_time_us as usize, Ordering::Release);
    link_cmd(LCMD_DLE)
}

#[no_mangle]
pub extern "C" fn runtime_ble_update_conn_params(
    min_interval_ms: u16,
    max_interval_ms: u16,
    latency: u16,
    timeout_ms: u16,
) -> c_int {
    LINK_CONN_MIN_MS.store(min_interval_ms as usize, Ordering::Release);
    LINK_CONN_MAX_MS.store(max_interval_ms as usize, Ordering::Release);
    LINK_CONN_LATENCY.store(latency as usize, Ordering::Release);
    LINK_CONN_TIMEOUT_MS.store(timeout_ms as usize, Ordering::Release);
    link_cmd(LCMD_CONN_PARAMS)
}

#[no_mangle]
pub extern "C" fn runtime_ble_request_security() -> c_int {
    link_cmd(LCMD_SECURITY_REQUEST)
}

#[no_mangle]
pub extern "C" fn runtime_ble_passkey_confirm(accept: u8) -> c_int {
    link_cmd(if accept == 0 {
        LCMD_PASSKEY_CANCEL
    } else {
        LCMD_PASSKEY_CONFIRM
    })
}

#[no_mangle]
pub extern "C" fn runtime_ble_passkey_input(passkey: u32) -> c_int {
    if passkey > 999_999 {
        return RUNTIME_BLE_ERR_INVALID;
    }
    LINK_PASSKEY.store(passkey, Ordering::Release);
    link_cmd(LCMD_PASSKEY_INPUT)
}

// ---- Central / GATT client API ----
// The functions exist in every build (stable C ABI). Without the `central`
// feature they return RUNTIME_BLE_ERR_INVALID; with it they queue one command
// for the central session loop (see radio.rs).

/// Queue a parameter-less central command if no command is outstanding.
fn central_cmd(cmd: u32, handle: u32) -> c_int {
    #[cfg(feature = "central")]
    {
        if CENTRAL_CMD.load(Ordering::Acquire) != CCMD_NONE {
            return RUNTIME_BLE_ERR_NO_MEM;
        }
        CENTRAL_HANDLE.store(handle, Ordering::Release);
        CENTRAL_CMD.store(cmd, Ordering::Release);
        RUNTIME_BLE_OK
    }
    #[cfg(not(feature = "central"))]
    {
        let _ = (cmd, handle);
        RUNTIME_BLE_ERR_INVALID
    }
}

#[no_mangle]
pub extern "C" fn runtime_ble_scan_start(
    active: u8,
    interval_ms: u16,
    window_ms: u16,
    timeout_ms: u16,
) -> c_int {
    #[cfg(feature = "central")]
    {
        SCAN_ACTIVE.store(active != 0, Ordering::Release);
        SCAN_INTERVAL_MS.store(interval_ms as usize, Ordering::Release);
        SCAN_WINDOW_MS.store(window_ms as usize, Ordering::Release);
        SCAN_TIMEOUT_MS.store(timeout_ms as usize, Ordering::Release);
        central_cmd(CCMD_SCAN_START, 0)
    }
    #[cfg(not(feature = "central"))]
    {
        let _ = (active, interval_ms, window_ms, timeout_ms);
        RUNTIME_BLE_ERR_INVALID
    }
}

#[no_mangle]
pub extern "C" fn runtime_ble_scan_stop() -> c_int {
    central_cmd(CCMD_SCAN_STOP, 0)
}

#[no_mangle]
pub extern "C" fn runtime_ble_connect(addr: *const u8) -> c_int {
    runtime_ble_connect_addr(addr, 0)
}

#[no_mangle]
pub extern "C" fn runtime_ble_connect_addr(addr: *const u8, addr_kind: u8) -> c_int {
    #[cfg(feature = "central")]
    {
        if addr.is_null() {
            return RUNTIME_BLE_ERR_INVALID;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(
                addr,
                core::ptr::addr_of_mut!(CENTRAL_ADDR) as *mut u8,
                6,
            );
        }
        CENTRAL_ADDR_KIND.store(addr_kind as usize, Ordering::Release);
        central_cmd(CCMD_CONNECT, 0)
    }
    #[cfg(not(feature = "central"))]
    {
        let _ = (addr, addr_kind);
        RUNTIME_BLE_ERR_INVALID
    }
}
#[no_mangle]
pub extern "C" fn runtime_ble_disconnect() -> c_int {
    central_cmd(CCMD_DISCONNECT, 0)
}
#[no_mangle]
pub extern "C" fn runtime_ble_client_discover(uuid: *const u8, uuid_len: u8) -> c_int {
    #[cfg(feature = "central")]
    {
        if uuid.is_null() || (uuid_len != 2 && uuid_len != 16) {
            return RUNTIME_BLE_ERR_INVALID;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(
                uuid,
                core::ptr::addr_of_mut!(CENTRAL_UUID) as *mut u8,
                uuid_len as usize,
            );
        }
        CENTRAL_UUID_LEN.store(uuid_len as usize, Ordering::Release);
        central_cmd(CCMD_DISCOVER, 0)
    }
    #[cfg(not(feature = "central"))]
    {
        let _ = (uuid, uuid_len);
        RUNTIME_BLE_ERR_INVALID
    }
}
#[no_mangle]
pub extern "C" fn runtime_ble_client_read(handle: u16) -> c_int {
    central_cmd(CCMD_READ, handle as u32)
}

fn central_write(cmd: u32, handle: u16, data: *const u8, len: usize) -> c_int {
    #[cfg(feature = "central")]
    {
        if data.is_null() || len == 0 || len > SEND_BUF_CAP {
            return RUNTIME_BLE_ERR_INVALID;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(data, core::ptr::addr_of_mut!(SEND_BUF) as *mut u8, len);
        }
        SEND_LEN.store(len, Ordering::Release);
        central_cmd(cmd, handle as u32)
    }
    #[cfg(not(feature = "central"))]
    {
        let _ = (cmd, handle, data, len);
        RUNTIME_BLE_ERR_INVALID
    }
}

#[no_mangle]
pub extern "C" fn runtime_ble_client_write(handle: u16, data: *const u8, len: usize) -> c_int {
    central_write(CCMD_WRITE, handle, data, len)
}

#[no_mangle]
pub extern "C" fn runtime_ble_client_write_no_rsp(
    handle: u16,
    data: *const u8,
    len: usize,
) -> c_int {
    central_write(CCMD_WRITE_NO_RSP, handle, data, len)
}

#[no_mangle]
pub extern "C" fn runtime_ble_client_subscribe(handle: u16) -> c_int {
    central_cmd(CCMD_SUBSCRIBE, handle as u32)
}

// ---- L2CAP API ----
#[no_mangle]
pub extern "C" fn runtime_ble_l2cap_send(data: *const u8, len: usize) -> c_int {
    #[cfg(feature = "l2cap")]
    {
        if data.is_null() || len == 0 || len > SEND_BUF_CAP {
            return RUNTIME_BLE_ERR_INVALID;
        }
        if L2CAP_SEND_REQ.load(Ordering::Acquire) {
            return RUNTIME_BLE_ERR_NO_MEM;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(
                data,
                core::ptr::addr_of_mut!(L2CAP_SEND_BUF) as *mut u8,
                len,
            );
        }
        L2CAP_SEND_LEN.store(len, Ordering::Release);
        L2CAP_SEND_REQ.store(true, Ordering::Release);
        RUNTIME_BLE_OK
    }
    #[cfg(not(feature = "l2cap"))]
    {
        let _ = (data, len);
        RUNTIME_BLE_ERR_INVALID
    }
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
