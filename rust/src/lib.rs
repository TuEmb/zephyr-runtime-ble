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
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicUsize, Ordering};

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
    /// Central: scan report with address type and metadata flags.
    pub on_scan_result_meta: Option<
        extern "C" fn(
            addr: *const u8,
            addr_kind: u8,
            rssi: i8,
            adv: *const u8,
            adv_len: usize,
            flags: u16,
            primary_phy: u8,
            secondary_phy: u8,
            tx_power_dbm: i8,
            sid: u8,
            user: *mut c_void,
        ),
    >,
    /// Central: a primary service found by service discovery.
    pub on_service: Option<
        extern "C" fn(
            start_handle: u16,
            end_handle: u16,
            uuid: *const u8,
            uuid_len: u8,
            user: *mut c_void,
        ),
    >,
    /// Central: a characteristic found by runtime_ble_client_discover.
    pub on_discovered: Option<
        extern "C" fn(handle: u16, uuid: *const u8, uuid_len: u8, props: u16, user: *mut c_void),
    >,
    /// Central: a descriptor found by runtime_ble_client_discover_descriptors.
    pub on_descriptor:
        Option<extern "C" fn(handle: u16, uuid: *const u8, uuid_len: u8, user: *mut c_void)>,
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
    /// Current negotiated ATT MTU.
    pub on_att_mtu: Option<extern "C" fn(att_mtu: u16, user: *mut c_void)>,
    /// LE frame spacing update.
    pub on_frame_space: Option<extern "C" fn(frame_space_us: u32, user: *mut c_void)>,
    /// LE connection rate/subrate changed.
    pub on_connection_rate: Option<
        extern "C" fn(
            interval_ms: u16,
            subrate_factor: u16,
            latency: u16,
            continuation_number: u16,
            timeout_ms: u16,
            user: *mut c_void,
        ),
    >,
    /// Link RSSI read result.
    pub on_rssi: Option<extern "C" fn(rssi: i8, user: *mut c_void)>,
    /// Pairing/encryption event.
    pub on_security_event:
        Option<extern "C" fn(event: u8, level: u8, passkey: u32, flags: u8, user: *mut c_void)>,
    /// Security: load a persistent bond blob from an application slot.
    pub on_bond_load:
        Option<extern "C" fn(index: u8, out: *mut u8, max_len: usize, user: *mut c_void) -> usize>,
    /// Security: store a persistent bond blob to an application slot.
    pub on_bond_store:
        Option<extern "C" fn(index: u8, blob: *const u8, len: usize, user: *mut c_void)>,
    /// Security: one restored/runtime bond returned by runtime_ble_bond_enumerate().
    pub on_bond: Option<
        extern "C" fn(
            index: u8,
            addr: *const u8,
            addr_kind: u8,
            level: u8,
            key_len: u8,
            flags: u8,
            user: *mut c_void,
        ),
    >,
    /// Security: completion for runtime_ble_bond_delete[_all]().
    pub on_bond_deleted: Option<extern "C" fn(index: u8, status: i8, user: *mut c_void)>,
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
    /// Central: command completion status.
    pub on_client_status: Option<extern "C" fn(op: u8, status: i8, handle: u16, user: *mut c_void)>,
    /// Peripheral: peer wrote to a user-defined descriptor.
    pub on_descriptor_write: Option<
        extern "C" fn(
            handle: u16,
            chr: u16,
            desc: u8,
            data: *const u8,
            len: usize,
            user: *mut c_void,
        ),
    >,
    /// Peripheral: peer read a user-defined descriptor; fill out and return the byte count.
    pub on_descriptor_read_value: Option<
        extern "C" fn(
            handle: u16,
            chr: u16,
            desc: u8,
            out: *mut u8,
            max_len: usize,
            user: *mut c_void,
        ) -> usize,
    >,
    /// Current security state read result.
    pub on_security_state:
        Option<extern "C" fn(level: u8, key_len: u8, flags: u8, user: *mut c_void)>,
    /// Peripheral: peer wrote to a characteristic, including ATT offset.
    pub on_write_ext: Option<
        extern "C" fn(chr: u16, offset: u16, data: *const u8, len: usize, user: *mut c_void),
    >,
    /// Peripheral: peer wrote to a descriptor, including ATT offset.
    pub on_descriptor_write_ext: Option<
        extern "C" fn(
            handle: u16,
            chr: u16,
            desc: u8,
            offset: u16,
            data: *const u8,
            len: usize,
            user: *mut c_void,
        ),
    >,
}

/// C ABI: one read-only descriptor (must match runtime_ble.h).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RuntimeBleDescDef {
    pub uuid: *const u8,
    pub uuid_len: u8,
    pub value: *const u8,
    pub value_len: u16,
    pub permissions: u16,
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
    pub descriptors: *const RuntimeBleDescDef,
    pub num_descriptors: u8,
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
/// manufacturer_data, service data, address) must outlive the session — use static storage.
/// C ABI: advertising / GAP settings (must match runtime_ble_adv_t).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RuntimeBleAdv {
    pub data: *const u8,
    pub data_len: u8,
    pub manufacturer_id: u16,
    pub manufacturer_data: *const u8,
    pub manufacturer_data_len: u16,
    pub service_uuid: *const u8,
    pub service_uuid_len: u8,
    pub service_data_uuid: *const u8,
    pub service_data_uuid_len: u8,
    pub service_data: *const u8,
    pub service_data_len: u8,
    pub appearance: u16,
    pub appearance_present: u8,
    pub tx_power_dbm: i8,
    pub tx_power_present: u8,
    pub scan_response_data: *const u8,
    pub scan_response_data_len: u8,
    pub nonconnectable: u8,
    pub interval_min_ms: u16,
    pub interval_max_ms: u16,
    pub channel_map: u8,
    pub filter_policy: u8,
    pub accept_address: *const u8,
    pub accept_address_kind: u8,
    pub discoverable: u8,
    pub directed_peer_address: *const u8,
    pub directed_peer_address_kind: u8,
    pub directed_high_duty: u8,
    pub extended: u8,
    pub primary_phy: u8,
    pub secondary_phy: u8,
    pub periodic: u8,
    pub periodic_interval_min_ms: u16,
    pub periodic_interval_max_ms: u16,
    pub periodic_data: *const u8,
    pub periodic_data_len: u8,
    pub periodic_include_tx_power: u8,
}

/// C ABI: central-role settings (must match runtime_ble_central_t).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RuntimeBleCentral {
    pub peer_address: *const u8,
    pub peer_address_kind: u8,
    pub conn_min_interval_ms: u16,
    pub conn_max_interval_ms: u16,
    pub conn_latency: u16,
    pub conn_timeout_ms: u16,
}

/// C ABI: L2CAP CoC settings (must match runtime_ble_l2cap_cfg_t).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RuntimeBleL2cap {
    pub psm: u16,
    pub mtu: u16,
    pub mps: u16,
    pub initial_credits: u16,
    pub credit_policy: u8,
    pub credit_policy_value: u16,
}

/// C ABI: Security Manager settings (must match runtime_ble_security_t).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RuntimeBleSecurity {
    pub bondable: u8,
    pub request_on_connect: u8,
    pub oob_available: u8,
    pub io_capability: u8,
    pub bond_slot_count: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RuntimeBleConfig {
    /// Must equal RUNTIME_BLE_ABI_VERSION; guards against a header/lib mismatch.
    pub abi_version: u32,
    pub device_name: *const c_char,
    /// Optional custom 6-byte static-random address; null -> hwinfo-derived.
    pub address: *const u8,
    /// Role: 0 = peripheral (default), 1 = central. See RUNTIME_BLE_ROLE_*.
    pub role: u8,
    pub adv: RuntimeBleAdv,
    pub central: RuntimeBleCentral,
    pub l2cap: RuntimeBleL2cap,
    pub security: RuntimeBleSecurity,
    /// User-defined GATT (null/0 -> built-in NUS). Built at load time.
    pub services: *const RuntimeBleServiceDef,
    pub num_services: u8,
    pub sdc_disable: u32,
    pub callbacks: RuntimeBleCallbacks,
    pub user: *mut c_void,
}

/// Config captured at init, read once by the runtime thread.
#[derive(Clone, Copy)]
pub(crate) struct RuntimeCfg {
    pub device_name: *const c_char,
    pub adv_data: *const u8,
    pub adv_data_len: u8,
    pub manufacturer_id: u16,
    pub manufacturer_data: *const u8,
    pub manufacturer_data_len: u16,
    pub adv_service_uuid: *const u8,
    pub adv_service_uuid_len: u8,
    pub adv_service_data_uuid: *const u8,
    pub adv_service_data_uuid_len: u8,
    pub adv_service_data: *const u8,
    pub adv_service_data_len: u8,
    pub appearance: u16,
    pub adv_appearance: u8,
    pub adv_tx_power_dbm: i8,
    pub adv_tx_power_present: u8,
    pub scan_response_data: *const u8,
    pub scan_response_data_len: u8,
    pub nonconnectable: u8,
    pub adv_interval_min_ms: u16,
    pub adv_interval_max_ms: u16,
    pub adv_channel_map: u8,
    pub adv_filter_policy: u8,
    pub adv_accept_address: *const u8,
    pub adv_accept_address_kind: u8,
    pub discoverable: u8,
    pub address: *const u8,
    pub directed_peer_address: *const u8,
    pub directed_peer_address_kind: u8,
    pub directed_high_duty: u8,
    pub adv_extended: u8,
    pub adv_primary_phy: u8,
    pub adv_secondary_phy: u8,
    pub periodic_adv: u8,
    pub periodic_adv_interval_min_ms: u16,
    pub periodic_adv_interval_max_ms: u16,
    pub periodic_adv_data: *const u8,
    pub periodic_adv_data_len: u8,
    pub periodic_adv_include_tx_power: u8,
    pub services: *const RuntimeBleServiceDef,
    pub num_services: u8,
    pub role: u8,
    pub peer_address: *const u8,
    pub peer_address_kind: u8,
    pub central_conn_min_interval_ms: u16,
    pub central_conn_max_interval_ms: u16,
    pub central_conn_latency: u16,
    pub central_conn_timeout_ms: u16,
    pub l2cap_psm: u16,
    pub l2cap_mtu: u16,
    pub l2cap_mps: u16,
    pub l2cap_initial_credits: u16,
    pub l2cap_credit_policy: u8,
    pub l2cap_credit_policy_value: u16,
    pub security_bondable: u8,
    pub security_request_on_connect: u8,
    pub security_oob_available: u8,
    pub security_io_capability: u8,
    pub bond_slot_count: u8,
    pub sdc_disable: u32,
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
/// TX mode: automatic notify-or-indicate selection, or explicit indication.
pub(crate) const SEND_KIND_AUTO: usize = 0;
pub(crate) const SEND_KIND_INDICATE: usize = 1;
pub(crate) static SEND_KIND: AtomicUsize = AtomicUsize::new(SEND_KIND_AUTO);
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
pub(crate) const CCMD_DISCOVER_DESCRIPTORS: u32 = 10;
pub(crate) const CCMD_SUBSCRIBE_INDICATE: u32 = 11;
pub(crate) const CCMD_READ_BLOB: u32 = 12;
pub(crate) const CCMD_DISCOVER_ALL: u32 = 13;
pub(crate) const CCMD_DISCOVER_SERVICES: u32 = 14;
pub(crate) const CCMD_READ_BY_UUID: u32 = 15;
pub(crate) const CCMD_UNSUBSCRIBE: u32 = 16;
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
pub(crate) static SCAN_FILTER_DUPLICATES: AtomicBool = AtomicBool::new(false);
pub(crate) static SCAN_PHY_OPTIONS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static SCAN_FILTER_ADDR_ENABLED: AtomicBool = AtomicBool::new(false);
pub(crate) static mut SCAN_FILTER_ADDR: [u8; 6] = [0; 6];
pub(crate) static SCAN_FILTER_ADDR_KIND: AtomicUsize = AtomicUsize::new(0);
// CCMD_WRITE payload reuses SEND_BUF / SEND_LEN (a session is one role only).

// ---- L2CAP outbound SDU (single outstanding; consumed by the l2cap send pump) ----
pub(crate) static L2CAP_SEND_REQ: AtomicBool = AtomicBool::new(false);
pub(crate) static L2CAP_SEND_LEN: AtomicUsize = AtomicUsize::new(0);
pub(crate) static mut L2CAP_SEND_BUF: [u8; SEND_BUF_CAP] = [0; SEND_BUF_CAP];
pub(crate) static L2CAP_DISCONNECT_REQ: AtomicBool = AtomicBool::new(false);

// ---- Active-link control channel (peripheral or central connection) ----
pub(crate) const LCMD_NONE: u32 = 0;
pub(crate) const LCMD_SET_PHY: u32 = 1;
pub(crate) const LCMD_DLE: u32 = 2;
pub(crate) const LCMD_CONN_PARAMS: u32 = 3;
pub(crate) const LCMD_SECURITY_REQUEST: u32 = 4;
pub(crate) const LCMD_PASSKEY_CONFIRM: u32 = 5;
pub(crate) const LCMD_PASSKEY_CANCEL: u32 = 6;
pub(crate) const LCMD_PASSKEY_INPUT: u32 = 7;
pub(crate) const LCMD_READ_RSSI: u32 = 8;
pub(crate) const LCMD_READ_ATT_MTU: u32 = 9;
pub(crate) const LCMD_FRAME_SPACE: u32 = 10;
pub(crate) const LCMD_CONNECTION_RATE: u32 = 11;
pub(crate) const LCMD_READ_PHY: u32 = 12;
pub(crate) const LCMD_READ_SECURITY: u32 = 13;
pub(crate) static LINK_CMD: AtomicU32 = AtomicU32::new(LCMD_NONE);
pub(crate) static LINK_PHY: AtomicUsize = AtomicUsize::new(0);
pub(crate) static LINK_DLE_OCTETS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static LINK_DLE_TIME_US: AtomicUsize = AtomicUsize::new(0);
pub(crate) static LINK_CONN_MIN_MS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static LINK_CONN_MAX_MS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static LINK_CONN_LATENCY: AtomicUsize = AtomicUsize::new(0);
pub(crate) static LINK_CONN_TIMEOUT_MS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static LINK_FRAME_SPACE_MIN_US: AtomicUsize = AtomicUsize::new(0);
pub(crate) static LINK_FRAME_SPACE_MAX_US: AtomicUsize = AtomicUsize::new(0);
pub(crate) static LINK_FRAME_SPACE_PHY_MASK: AtomicUsize = AtomicUsize::new(0);
pub(crate) static LINK_FRAME_SPACE_TYPES: AtomicUsize = AtomicUsize::new(0);
pub(crate) static LINK_RATE_SUBRATE_MIN: AtomicUsize = AtomicUsize::new(0);
pub(crate) static LINK_RATE_SUBRATE_MAX: AtomicUsize = AtomicUsize::new(0);
pub(crate) static LINK_RATE_CONTINUATION: AtomicUsize = AtomicUsize::new(0);
pub(crate) static LINK_PASSKEY: AtomicU32 = AtomicU32::new(0);

// ---- Security bond/admin command channel ----
pub(crate) const BCMD_NONE: u32 = 0;
pub(crate) const BCMD_ENUMERATE: u32 = 1;
pub(crate) const BCMD_DELETE: u32 = 2;
pub(crate) const BCMD_DELETE_ALL: u32 = 3;
pub(crate) const BCMD_SET_IO_CAPABILITY: u32 = 4;
pub(crate) static BOND_CMD: AtomicU32 = AtomicU32::new(BCMD_NONE);
pub(crate) static BOND_INDEX: AtomicUsize = AtomicUsize::new(0);
pub(crate) static BOND_IO_CAPABILITY: AtomicUsize = AtomicUsize::new(0);

pub(crate) const RUNTIME_BLE_OK: c_int = 0;
pub(crate) const RUNTIME_BLE_ERR_INVALID: c_int = -1;
const RUNTIME_BLE_ERR_NO_MEM: c_int = -2;
const RUNTIME_BLE_ERR_ABI: c_int = -6;
/// ABI version of the config/callback layout (must match runtime_ble.h). Bump on
/// any change to RuntimeBleConfig / RuntimeBleCallbacks.
const RUNTIME_BLE_ABI_VERSION: u32 = 1;
// Detailed load-failure codes (must match runtime_ble.h). Set by the runtime
// thread during bring-up and returned by runtime_ble_load() / _load_status().
pub(crate) const RUNTIME_BLE_ERR_MPSL: c_int = -3;
pub(crate) const RUNTIME_BLE_ERR_SDC: c_int = -4;
const RUNTIME_BLE_ERR_TIMEOUT: c_int = -5;
/// Sentinel: bring-up not finished yet. Set before the runtime thread starts.
pub(crate) const RUNTIME_BLE_LOAD_PENDING: c_int = 1;

/// Bring-up result, published by the runtime thread and read by glue's
/// runtime_ble_load() once it is signalled (via runtime_ble_load_done()).
pub(crate) static LOAD_STATUS: AtomicI32 = AtomicI32::new(RUNTIME_BLE_LOAD_PENDING);

/// Read the last/most-recent load result: RUNTIME_BLE_OK, a negative
/// RUNTIME_BLE_ERR_*, or RUNTIME_BLE_LOAD_PENDING while a load is in progress.
#[no_mangle]
pub extern "C" fn runtime_ble_load_status() -> c_int {
    LOAD_STATUS.load(Ordering::Acquire)
}

// Bits for RuntimeBleConfig.sdc_disable (must match runtime_ble.h). Drop optional
// SoftDevice Controller features the app does not use. Consumed in chip::build_sdc.
pub(crate) const SDC_DISABLE_EXT_ADV: u32 = 1 << 0;
pub(crate) const SDC_DISABLE_PERIODIC_ADV: u32 = 1 << 1;
pub(crate) const SDC_DISABLE_CODED_PHY: u32 = 1 << 2;
pub(crate) const SDC_DISABLE_2M_PHY: u32 = 1 << 3;
pub(crate) const SDC_DISABLE_DLE: u32 = 1 << 4;
pub(crate) const SDC_DISABLE_SUBRATING: u32 = 1 << 5;
pub(crate) const SDC_DISABLE_FRAME_SPACE: u32 = 1 << 6;

/// Configure the library. Copies `cfg`. Call once before `runtime_ble_run`.
#[no_mangle]
pub extern "C" fn runtime_ble_init(cfg: *const RuntimeBleConfig) -> c_int {
    if cfg.is_null() {
        return RUNTIME_BLE_ERR_INVALID;
    }
    let c = unsafe { &*cfg };
    if c.abi_version != RUNTIME_BLE_ABI_VERSION {
        return RUNTIME_BLE_ERR_ABI;
    }
    let (adv, cen, l2, sec) = (&c.adv, &c.central, &c.l2cap, &c.security);
    if l2.credit_policy > 1
        || (l2.mtu != 0 && l2.mtu < 23)
        || (l2.mps != 0 && l2.mps < 23)
        || adv.filter_policy > 3
        || adv.extended > 1
        || adv.primary_phy > 3
        || adv.secondary_phy > 3
        || adv.periodic > 1
        || (adv.periodic != 0 && (adv.extended == 0 || adv.nonconnectable == 0))
        || (adv.filter_policy != 0 && adv.accept_address.is_null())
    {
        return RUNTIME_BLE_ERR_INVALID;
    }
    // Flatten the grouped public config into the internal (flat) RuntimeCfg so the
    // rest of the runtime is unchanged by the public struct's grouping.
    unsafe {
        CONFIG = Some(RuntimeCfg {
            device_name: c.device_name,
            adv_data: adv.data,
            adv_data_len: adv.data_len,
            manufacturer_id: adv.manufacturer_id,
            manufacturer_data: adv.manufacturer_data,
            manufacturer_data_len: adv.manufacturer_data_len,
            adv_service_uuid: adv.service_uuid,
            adv_service_uuid_len: adv.service_uuid_len,
            adv_service_data_uuid: adv.service_data_uuid,
            adv_service_data_uuid_len: adv.service_data_uuid_len,
            adv_service_data: adv.service_data,
            adv_service_data_len: adv.service_data_len,
            appearance: adv.appearance,
            adv_appearance: adv.appearance_present,
            adv_tx_power_dbm: adv.tx_power_dbm,
            adv_tx_power_present: adv.tx_power_present,
            scan_response_data: adv.scan_response_data,
            scan_response_data_len: adv.scan_response_data_len,
            nonconnectable: adv.nonconnectable,
            adv_interval_min_ms: adv.interval_min_ms,
            adv_interval_max_ms: adv.interval_max_ms,
            adv_channel_map: adv.channel_map,
            adv_filter_policy: adv.filter_policy,
            adv_accept_address: adv.accept_address,
            adv_accept_address_kind: adv.accept_address_kind,
            discoverable: adv.discoverable,
            address: c.address,
            directed_peer_address: adv.directed_peer_address,
            directed_peer_address_kind: adv.directed_peer_address_kind,
            directed_high_duty: adv.directed_high_duty,
            adv_extended: adv.extended,
            adv_primary_phy: adv.primary_phy,
            adv_secondary_phy: adv.secondary_phy,
            periodic_adv: adv.periodic,
            periodic_adv_interval_min_ms: adv.periodic_interval_min_ms,
            periodic_adv_interval_max_ms: adv.periodic_interval_max_ms,
            periodic_adv_data: adv.periodic_data,
            periodic_adv_data_len: adv.periodic_data_len,
            periodic_adv_include_tx_power: adv.periodic_include_tx_power,
            services: c.services,
            num_services: c.num_services,
            role: c.role,
            peer_address: cen.peer_address,
            peer_address_kind: cen.peer_address_kind,
            central_conn_min_interval_ms: cen.conn_min_interval_ms,
            central_conn_max_interval_ms: cen.conn_max_interval_ms,
            central_conn_latency: cen.conn_latency,
            central_conn_timeout_ms: cen.conn_timeout_ms,
            l2cap_psm: l2.psm,
            l2cap_mtu: l2.mtu,
            l2cap_mps: l2.mps,
            l2cap_initial_credits: l2.initial_credits,
            l2cap_credit_policy: l2.credit_policy,
            l2cap_credit_policy_value: l2.credit_policy_value,
            security_bondable: sec.bondable,
            security_request_on_connect: sec.request_on_connect,
            security_oob_available: sec.oob_available,
            security_io_capability: sec.io_capability,
            bond_slot_count: sec.bond_slot_count,
            sdc_disable: c.sdc_disable,
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
fn queue_tx(chr: usize, kind: usize, data: *const u8, len: usize) -> c_int {
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
    SEND_KIND.store(kind, Ordering::Release);
    SEND_LEN.store(len, Ordering::Release);
    SEND_REQ.store(true, Ordering::Release);
    RUNTIME_BLE_OK
}

/// Queue one notification on the built-in NUS TX characteristic.
#[no_mangle]
pub extern "C" fn runtime_ble_send(data: *const u8, len: usize) -> c_int {
    queue_tx(NUS_TX_CHR, SEND_KIND_AUTO, data, len)
}

/// Queue one notification/indication on a user-defined characteristic `chr`
/// (flat index in declaration order across config.services).
#[no_mangle]
pub extern "C" fn runtime_ble_notify(chr: u16, data: *const u8, len: usize) -> c_int {
    queue_tx(chr as usize, SEND_KIND_AUTO, data, len)
}

/// Queue one indication on a user-defined characteristic `chr`.
#[no_mangle]
pub extern "C" fn runtime_ble_indicate(chr: u16, data: *const u8, len: usize) -> c_int {
    queue_tx(chr as usize, SEND_KIND_INDICATE, data, len)
}

/// Resolve the flat index of the first user-defined characteristic whose UUID
/// matches `uuid` (LE bytes, `uuid_len` 2 or 16). Walks `config.services` in the
/// same declaration order that build_gatt assigns indices, so the result is the
/// `chr` value accepted by runtime_ble_notify/indicate and reported by the
/// on_write/on_subscription callbacks. Lets an app look characteristics up by UUID
/// instead of hard-coding a declaration-order index. Returns the index (>= 0), or
/// a negative RUNTIME_BLE_ERR_* if not configured / not found / arguments invalid.
#[no_mangle]
pub extern "C" fn runtime_ble_char_index(uuid: *const u8, uuid_len: u8) -> c_int {
    if uuid.is_null() || (uuid_len != 2 && uuid_len != 16) {
        return RUNTIME_BLE_ERR_INVALID;
    }
    // SAFETY: CONFIG is set once in runtime_ble_init before this is called.
    let cfg = match unsafe { core::ptr::addr_of!(CONFIG).as_ref().unwrap().as_ref() } {
        Some(c) => c,
        None => return RUNTIME_BLE_ERR_INVALID,
    };
    if cfg.services.is_null() || cfg.num_services == 0 {
        return RUNTIME_BLE_ERR_INVALID; // built-in NUS: no user-defined chars
    }
    let want = unsafe { core::slice::from_raw_parts(uuid, uuid_len as usize) };
    let services =
        unsafe { core::slice::from_raw_parts(cfg.services, cfg.num_services as usize) };
    let mut idx: u16 = 0;
    for s in services {
        if s.chars.is_null() {
            continue;
        }
        let cdefs = unsafe { core::slice::from_raw_parts(s.chars, s.num_chars as usize) };
        for c in cdefs {
            if c.uuid_len == uuid_len && !c.uuid.is_null() {
                let have = unsafe { core::slice::from_raw_parts(c.uuid, c.uuid_len as usize) };
                if have == want {
                    return idx as c_int;
                }
            }
            idx += 1;
        }
    }
    RUNTIME_BLE_ERR_INVALID
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
    if phy != 1 && phy != 2 && phy != 3 {
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
pub extern "C" fn runtime_ble_update_frame_space(
    min_us: u32,
    max_us: u32,
    phy_mask: u8,
    spacing_types: u8,
) -> c_int {
    LINK_FRAME_SPACE_MIN_US.store(min_us as usize, Ordering::Release);
    LINK_FRAME_SPACE_MAX_US.store(max_us as usize, Ordering::Release);
    LINK_FRAME_SPACE_PHY_MASK.store(phy_mask as usize, Ordering::Release);
    LINK_FRAME_SPACE_TYPES.store(spacing_types as usize, Ordering::Release);
    link_cmd(LCMD_FRAME_SPACE)
}

#[no_mangle]
pub extern "C" fn runtime_ble_request_connection_rate(
    min_interval_ms: u16,
    max_interval_ms: u16,
    subrate_min: u16,
    subrate_max: u16,
    latency: u16,
    continuation_number: u16,
    timeout_ms: u16,
) -> c_int {
    LINK_CONN_MIN_MS.store(min_interval_ms as usize, Ordering::Release);
    LINK_CONN_MAX_MS.store(max_interval_ms as usize, Ordering::Release);
    LINK_RATE_SUBRATE_MIN.store(subrate_min as usize, Ordering::Release);
    LINK_RATE_SUBRATE_MAX.store(subrate_max as usize, Ordering::Release);
    LINK_CONN_LATENCY.store(latency as usize, Ordering::Release);
    LINK_RATE_CONTINUATION.store(continuation_number as usize, Ordering::Release);
    LINK_CONN_TIMEOUT_MS.store(timeout_ms as usize, Ordering::Release);
    link_cmd(LCMD_CONNECTION_RATE)
}

#[no_mangle]
pub extern "C" fn runtime_ble_read_rssi() -> c_int {
    link_cmd(LCMD_READ_RSSI)
}

#[no_mangle]
pub extern "C" fn runtime_ble_read_att_mtu() -> c_int {
    link_cmd(LCMD_READ_ATT_MTU)
}

#[no_mangle]
pub extern "C" fn runtime_ble_read_phy() -> c_int {
    link_cmd(LCMD_READ_PHY)
}

#[no_mangle]
pub extern "C" fn runtime_ble_read_security() -> c_int {
    link_cmd(LCMD_READ_SECURITY)
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

fn valid_io_capability(capability: u8) -> bool {
    capability <= 5
}

fn bond_admin_cmd(cmd: u32, value: usize) -> c_int {
    if BOND_CMD.load(Ordering::Acquire) != BCMD_NONE {
        return RUNTIME_BLE_ERR_NO_MEM;
    }
    match cmd {
        BCMD_DELETE => BOND_INDEX.store(value, Ordering::Release),
        BCMD_SET_IO_CAPABILITY => BOND_IO_CAPABILITY.store(value, Ordering::Release),
        _ => {}
    }
    BOND_CMD.store(cmd, Ordering::Release);
    RUNTIME_BLE_OK
}

#[no_mangle]
pub extern "C" fn runtime_ble_set_io_capability(capability: u8) -> c_int {
    if !valid_io_capability(capability) {
        return RUNTIME_BLE_ERR_INVALID;
    }
    bond_admin_cmd(BCMD_SET_IO_CAPABILITY, capability as usize)
}

#[no_mangle]
pub extern "C" fn runtime_ble_bond_enumerate() -> c_int {
    bond_admin_cmd(BCMD_ENUMERATE, 0)
}

#[no_mangle]
pub extern "C" fn runtime_ble_bond_delete(index: u8) -> c_int {
    bond_admin_cmd(BCMD_DELETE, index as usize)
}

#[no_mangle]
pub extern "C" fn runtime_ble_bond_delete_all() -> c_int {
    bond_admin_cmd(BCMD_DELETE_ALL, 0)
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
    runtime_ble_scan_start_ex(
        active,
        interval_ms,
        window_ms,
        timeout_ms,
        0,
        core::ptr::null(),
        0,
    )
}

#[no_mangle]
pub extern "C" fn runtime_ble_scan_start_ex(
    active: u8,
    interval_ms: u16,
    window_ms: u16,
    timeout_ms: u16,
    options: u8,
    filter_addr: *const u8,
    filter_addr_kind: u8,
) -> c_int {
    #[cfg(feature = "central")]
    {
        SCAN_ACTIVE.store(active != 0, Ordering::Release);
        SCAN_INTERVAL_MS.store(interval_ms as usize, Ordering::Release);
        SCAN_WINDOW_MS.store(window_ms as usize, Ordering::Release);
        SCAN_TIMEOUT_MS.store(timeout_ms as usize, Ordering::Release);
        SCAN_FILTER_DUPLICATES.store(options & 0x01 != 0, Ordering::Release);
        SCAN_PHY_OPTIONS.store((options & 0x0e) as usize, Ordering::Release);
        SCAN_FILTER_ADDR_ENABLED.store(!filter_addr.is_null(), Ordering::Release);
        if !filter_addr.is_null() {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    filter_addr,
                    core::ptr::addr_of_mut!(SCAN_FILTER_ADDR) as *mut u8,
                    6,
                );
            }
        }
        SCAN_FILTER_ADDR_KIND.store(filter_addr_kind as usize, Ordering::Release);
        central_cmd(CCMD_SCAN_START, 0)
    }
    #[cfg(not(feature = "central"))]
    {
        let _ = (
            active,
            interval_ms,
            window_ms,
            timeout_ms,
            options,
            filter_addr,
            filter_addr_kind,
        );
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
pub extern "C" fn runtime_ble_client_discover_services() -> c_int {
    central_cmd(CCMD_DISCOVER_SERVICES, 0)
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
pub extern "C" fn runtime_ble_client_discover_all() -> c_int {
    central_cmd(CCMD_DISCOVER_ALL, 0)
}

#[no_mangle]
pub extern "C" fn runtime_ble_client_read(handle: u16) -> c_int {
    central_cmd(CCMD_READ, handle as u32)
}

#[no_mangle]
pub extern "C" fn runtime_ble_client_read_blob(handle: u16, offset: u16) -> c_int {
    central_cmd(CCMD_READ_BLOB, ((offset as u32) << 16) | handle as u32)
}

#[no_mangle]
pub extern "C" fn runtime_ble_client_read_by_uuid(
    start_handle: u16,
    end_handle: u16,
    uuid: *const u8,
    uuid_len: u8,
) -> c_int {
    #[cfg(feature = "central")]
    {
        if start_handle == 0
            || end_handle < start_handle
            || uuid.is_null()
            || (uuid_len != 2 && uuid_len != 16)
        {
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
        central_cmd(
            CCMD_READ_BY_UUID,
            ((start_handle as u32) << 16) | end_handle as u32,
        )
    }
    #[cfg(not(feature = "central"))]
    {
        let _ = (start_handle, end_handle, uuid, uuid_len);
        RUNTIME_BLE_ERR_INVALID
    }
}

#[no_mangle]
pub extern "C" fn runtime_ble_client_read_descriptor(handle: u16) -> c_int {
    runtime_ble_client_read(handle)
}

#[no_mangle]
pub extern "C" fn runtime_ble_client_read_descriptor_blob(handle: u16, offset: u16) -> c_int {
    runtime_ble_client_read_blob(handle, offset)
}

#[no_mangle]
pub extern "C" fn runtime_ble_client_discover_descriptors(
    start_handle: u16,
    end_handle: u16,
) -> c_int {
    if start_handle == 0 || end_handle < start_handle {
        return RUNTIME_BLE_ERR_INVALID;
    }
    central_cmd(
        CCMD_DISCOVER_DESCRIPTORS,
        ((start_handle as u32) << 16) | end_handle as u32,
    )
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
pub extern "C" fn runtime_ble_client_write_descriptor(
    handle: u16,
    data: *const u8,
    len: usize,
) -> c_int {
    runtime_ble_client_write(handle, data, len)
}

#[no_mangle]
pub extern "C" fn runtime_ble_client_subscribe(handle: u16) -> c_int {
    central_cmd(CCMD_SUBSCRIBE, handle as u32)
}

#[no_mangle]
pub extern "C" fn runtime_ble_client_subscribe_indicate(handle: u16) -> c_int {
    central_cmd(CCMD_SUBSCRIBE_INDICATE, handle as u32)
}

#[no_mangle]
pub extern "C" fn runtime_ble_client_unsubscribe(handle: u16) -> c_int {
    central_cmd(CCMD_UNSUBSCRIBE, handle as u32)
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

#[no_mangle]
pub extern "C" fn runtime_ble_l2cap_disconnect() -> c_int {
    #[cfg(feature = "l2cap")]
    {
        L2CAP_DISCONNECT_REQ.store(true, Ordering::Release);
        RUNTIME_BLE_OK
    }
    #[cfg(not(feature = "l2cap"))]
    {
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
