// SPDX-License-Identifier: Apache-2.0
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

#[cfg(feature = "central")]
use bt_hci::param::FilterDuplicates;
use bt_hci::param::{AddrKind, BdAddr, LeAdvEventKind, LeExtAdvDataStatus, PhyMask, SpacingTypes};
use bt_hci::uuid::BluetoothUuid16;
use embassy_futures::join::join;
#[cfg(feature = "central")]
use embassy_futures::select::Either;
use embassy_futures::select::{select, select3};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_time::{Duration, Timer};
use embassy_time_driver::Driver;
use embassy_time_queue_utils::Queue;
use nrf_sdc::mpsl::MultiprotocolServiceLayer;
use trouble_host::advertise::{AdvChannelMap, TxPower};
use trouble_host::attribute::{
    AttPermissions, AttributeTable, Characteristic, CharacteristicProps, PermissionLevel, Service,
};
#[cfg(feature = "central")]
use trouble_host::connection::{ConnectConfig, PhySet, ScanConfig};
use trouble_host::connection::{ConnectRateParams, RequestedConnParams};
#[cfg(feature = "central")]
use trouble_host::gatt::GattClient;
#[cfg(feature = "l2cap")]
use trouble_host::l2cap::{L2capChannel, L2capChannelConfig};
use trouble_host::prelude::*;
#[cfg(feature = "central")]
use trouble_host::scan::Scanner;
use trouble_host::{BondInformation, Identity, IdentityResolvingKey, LongTermKey, OobData};

use crate::{
    RuntimeBleCharDef, RuntimeCfg, NUS_TX_CHR, SEND_BUF, SEND_BUF_CAP, SEND_CHR, SEND_KIND,
    SEND_KIND_INDICATE, SEND_LEN, SEND_REQ, UNLOAD_REQ,
};
#[cfg(feature = "central")]
use crate::{
    CCMD_CONNECT, CCMD_DISCONNECT, CCMD_DISCOVER, CCMD_DISCOVER_ALL, CCMD_DISCOVER_DESCRIPTORS,
    CCMD_DISCOVER_SERVICES, CCMD_NONE, CCMD_READ, CCMD_READ_BLOB, CCMD_READ_BY_UUID,
    CCMD_SCAN_START, CCMD_SCAN_STOP, CCMD_SUBSCRIBE, CCMD_SUBSCRIBE_INDICATE, CCMD_WRITE,
    CCMD_WRITE_NO_RSP, CENTRAL_ADDR, CENTRAL_ADDR_KIND, CENTRAL_CMD, CENTRAL_HANDLE, CENTRAL_UUID,
    CENTRAL_UUID_LEN, SCAN_ACTIVE, SCAN_FILTER_ADDR, SCAN_FILTER_ADDR_ENABLED,
    SCAN_FILTER_ADDR_KIND, SCAN_FILTER_DUPLICATES, SCAN_INTERVAL_MS, SCAN_PHY_OPTIONS,
    SCAN_TIMEOUT_MS, SCAN_WINDOW_MS,
};
#[cfg(feature = "l2cap")]
use crate::{L2CAP_DISCONNECT_REQ, L2CAP_SEND_BUF, L2CAP_SEND_LEN, L2CAP_SEND_REQ};
use crate::{
    LCMD_CONNECTION_RATE, LCMD_CONN_PARAMS, LCMD_DLE, LCMD_FRAME_SPACE, LCMD_NONE,
    LCMD_PASSKEY_CANCEL, LCMD_PASSKEY_CONFIRM, LCMD_PASSKEY_INPUT, LCMD_READ_ATT_MTU,
    LCMD_READ_PHY, LCMD_READ_RSSI, LCMD_READ_SECURITY, LCMD_SECURITY_REQUEST, LCMD_SET_PHY,
    LINK_CMD, LINK_CONN_LATENCY, LINK_CONN_MAX_MS, LINK_CONN_MIN_MS, LINK_CONN_TIMEOUT_MS,
    LINK_DLE_OCTETS, LINK_DLE_TIME_US, LINK_FRAME_SPACE_MAX_US, LINK_FRAME_SPACE_MIN_US,
    LINK_FRAME_SPACE_PHY_MASK, LINK_FRAME_SPACE_TYPES, LINK_PASSKEY, LINK_PHY,
    LINK_RATE_CONTINUATION, LINK_RATE_SUBRATE_MAX, LINK_RATE_SUBRATE_MIN,
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
const BOND_BLOB_LEN: usize = 43;
const BOND_BLOB_VERSION: u8 = 1;
const BOND_SLOT_CAP: usize = 4;
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
const C_PERM_READ_ENCRYPT: u16 = 1 << 0;
const C_PERM_READ_AUTH: u16 = 1 << 1;
const C_PERM_WRITE_ENCRYPT: u16 = 1 << 2;
const C_PERM_WRITE_AUTH: u16 = 1 << 3;
const C_PERM_CCCD_ENCRYPT: u16 = 1 << 4;
const C_PERM_CCCD_AUTH: u16 = 1 << 5;
const C_PERM_WRITE_ALLOWED: u16 = 1 << 6;

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

fn map_permission(mask: u16, encrypt_bit: u16, auth_bit: u16) -> Option<PermissionLevel> {
    if mask & auth_bit != 0 {
        Some(PermissionLevel::AuthenticationRequired)
    } else if mask & encrypt_bit != 0 {
        Some(PermissionLevel::EncryptionRequired)
    } else {
        None
    }
}

fn descriptor_permissions(mask: u16) -> AttPermissions {
    AttPermissions {
        read: map_permission(mask, C_PERM_READ_ENCRYPT, C_PERM_READ_AUTH)
            .unwrap_or(PermissionLevel::Allowed),
        write: if mask & C_PERM_WRITE_ALLOWED != 0 {
            PermissionLevel::Allowed
        } else {
            map_permission(mask, C_PERM_WRITE_ENCRYPT, C_PERM_WRITE_AUTH)
                .unwrap_or(PermissionLevel::NotAllowed)
        },
    }
}

fn descriptor_is_writable(mask: u16) -> bool {
    mask & (C_PERM_WRITE_ALLOWED | C_PERM_WRITE_ENCRYPT | C_PERM_WRITE_AUTH) != 0
}

// Reverse of map_props: BLE CharacteristicProp bits -> C property bits, for
// reporting a discovered characteristic's properties to on_discovered.
#[cfg(feature = "central")]
fn props_to_c(ble: u8) -> u16 {
    let mut p: u16 = 0;
    if ble & 0x02 != 0 {
        p |= C_PROP_READ;
    }
    if ble & 0x04 != 0 {
        p |= C_PROP_WRITE_NR;
    }
    if ble & 0x08 != 0 {
        p |= C_PROP_WRITE;
    }
    if ble & 0x10 != 0 {
        p |= C_PROP_NOTIFY;
    }
    if ble & 0x20 != 0 {
        p |= C_PROP_INDICATE;
    }
    p
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
    props: Vec<u16>,
    descriptors: Vec<DescMeta>,
    nus_rx: usize,
    nus_tx: usize,
    stores: Vec<*mut [u8; VALUE_LEN]>,
    appearance_store: Option<*mut BluetoothUuid16>,
    bond_slots: RefCell<heapless::Vec<(Identity, u8), BOND_SLOT_CAP>>,
}

#[derive(Clone, Copy)]
struct DescMeta {
    handle: u16,
    chr: u16,
    desc: u8,
}

fn build_gatt(cfg: &RuntimeCfg, name: &'static str) -> Option<Gatt> {
    let mut table: AttributeTable<'static, NoopRawMutex, ATT_MAX> = AttributeTable::new();
    let (gap_appearance, appearance_store) = gap_appearance(cfg.appearance);
    if GapConfig::Peripheral(PeripheralConfig {
        name,
        appearance: gap_appearance,
    })
    .build(&mut table)
    .is_err()
    {
        if let Some(ptr) = appearance_store {
            unsafe {
                let _ = Box::from_raw(ptr);
            }
        }
        return None;
    }

    let mut chars: Vec<Characteristic<[u8; VALUE_LEN]>> = Vec::new();
    let mut props: Vec<u16> = Vec::new();
    let mut descriptors: Vec<DescMeta> = Vec::new();
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
        props.push(C_PROP_WRITE | C_PROP_WRITE_NR);
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
        props.push(C_PROP_NOTIFY);
    } else {
        let services =
            unsafe { core::slice::from_raw_parts(cfg.services, cfg.num_services as usize) };
        for s in services {
            let suuid = unsafe { uuid_from(s.uuid, s.uuid_len) };
            let mut svc = table.add_service(Service::new(suuid));
            let cdefs: &[RuntimeBleCharDef] =
                unsafe { core::slice::from_raw_parts(s.chars, s.num_chars as usize) };
            for c in cdefs {
                let chr_index = chars.len() as u16;
                let cuuid = unsafe { uuid_from(c.uuid, c.uuid_len) };
                let mut chr = svc.add_characteristic(
                    cuuid,
                    map_props(c.props),
                    [0u8; VALUE_LEN],
                    alloc_store(&mut stores),
                );
                if let Some(level) =
                    map_permission(c.permissions, C_PERM_READ_ENCRYPT, C_PERM_READ_AUTH)
                {
                    chr = chr.read_permission(level);
                }
                if let Some(level) =
                    map_permission(c.permissions, C_PERM_WRITE_ENCRYPT, C_PERM_WRITE_AUTH)
                {
                    chr = chr.write_permission(level);
                }
                if c.props & (C_PROP_NOTIFY | C_PROP_INDICATE) != 0 {
                    if let Some(level) =
                        map_permission(c.permissions, C_PERM_CCCD_ENCRYPT, C_PERM_CCCD_AUTH)
                    {
                        chr = chr.cccd_permission(level);
                    }
                }
                if !c.descriptors.is_null() && c.num_descriptors != 0 {
                    let descs = unsafe {
                        core::slice::from_raw_parts(c.descriptors, c.num_descriptors as usize)
                    };
                    for (desc_index, d) in descs.iter().enumerate() {
                        let value = if d.value.is_null() {
                            &[][..]
                        } else {
                            unsafe { core::slice::from_raw_parts(d.value, d.value_len as usize) }
                        };
                        let uuid = unsafe { uuid_from(d.uuid, d.uuid_len) };
                        let permissions = descriptor_permissions(d.permissions);
                        let handle = if descriptor_is_writable(d.permissions) {
                            let len = value.len().min(VALUE_LEN);
                            let mut init: heapless::Vec<u8, VALUE_LEN> = heapless::Vec::new();
                            let _ = init.extend_from_slice(&value[..len]);
                            chr.add_descriptor(uuid, permissions, init, alloc_store(&mut stores))
                                .handle()
                        } else {
                            chr.add_descriptor_ro::<[u8], _>(uuid, permissions.read, value)
                                .handle()
                        };
                        descriptors.push(DescMeta {
                            handle,
                            chr: chr_index,
                            desc: desc_index.min(u8::MAX as usize) as u8,
                        });
                    }
                }
                chars.push(chr.build());
                props.push(c.props);
            }
        }
    }

    Some(Gatt {
        server: Box::new(AttributeServer::new(table)),
        chars,
        props,
        descriptors,
        nus_rx,
        nus_tx,
        stores,
        appearance_store,
        bond_slots: RefCell::new(heapless::Vec::new()),
    })
}

fn gap_appearance(value: u16) -> (&'static BluetoothUuid16, Option<*mut BluetoothUuid16>) {
    if value == 0 {
        return (&appearance::power_device::GENERIC_POWER_DEVICE, None);
    }
    let ptr = Box::into_raw(Box::new(BluetoothUuid16::new(value)));
    // SAFETY: ptr is kept alive in Gatt.appearance_store and reclaimed only
    // after the attribute server/table has been dropped.
    (unsafe { &*ptr }, Some(ptr))
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

const RTBLE_ADDR_RANDOM: u8 = 0;
const RTBLE_ADDR_PUBLIC: u8 = 1;
const SCAN_F_CONNECTABLE: u16 = 1 << 0;
const SCAN_F_SCANNABLE: u16 = 1 << 1;
const SCAN_F_DIRECTED: u16 = 1 << 2;
const SCAN_F_SCAN_RESPONSE: u16 = 1 << 3;
const SCAN_F_LEGACY: u16 = 1 << 4;
const SCAN_F_DATA_INCOMPLETE: u16 = 1 << 5;
const SCAN_F_DATA_TRUNCATED: u16 = 1 << 6;
const LE_LIMITED_DISCOVERABLE: u8 = 0x01;

fn c_addr_kind(kind: u8) -> AddrKind {
    if kind == RTBLE_ADDR_PUBLIC {
        AddrKind::PUBLIC
    } else {
        AddrKind::RANDOM
    }
}

fn addr_kind_to_c(kind: AddrKind) -> u8 {
    if kind == AddrKind::PUBLIC {
        RTBLE_ADDR_PUBLIC
    } else {
        RTBLE_ADDR_RANDOM
    }
}

fn peer_address(addr: [u8; 6], kind: u8) -> Address {
    Address::new(c_addr_kind(kind), BdAddr::new(addr))
}

#[cfg(feature = "central")]
fn central_connect_params(cfg: &RuntimeCfg) -> RequestedConnParams {
    let min_ms = cfg.central_conn_min_interval_ms;
    let max_ms = cfg.central_conn_max_interval_ms;
    let timeout_ms = cfg.central_conn_timeout_ms;
    let params = RequestedConnParams {
        min_connection_interval: Duration::from_millis(if min_ms == 0 {
            80
        } else {
            min_ms as u64
        }),
        max_connection_interval: Duration::from_millis(if max_ms == 0 {
            80
        } else {
            max_ms as u64
        }),
        max_latency: cfg.central_conn_latency,
        min_event_length: Duration::from_secs(0),
        max_event_length: Duration::from_secs(0),
        supervision_timeout: Duration::from_millis(if timeout_ms == 0 {
            8000
        } else {
            timeout_ms as u64
        }),
    };
    if params.is_valid() {
        params
    } else {
        log_str(cfg, "[central] invalid connect params\0");
        RequestedConnParams::default()
    }
}

fn legacy_adv_flags(kind: LeAdvEventKind) -> u16 {
    match kind {
        LeAdvEventKind::AdvInd => SCAN_F_CONNECTABLE | SCAN_F_SCANNABLE | SCAN_F_LEGACY,
        LeAdvEventKind::AdvDirectInd => SCAN_F_CONNECTABLE | SCAN_F_DIRECTED | SCAN_F_LEGACY,
        LeAdvEventKind::AdvScanInd => SCAN_F_SCANNABLE | SCAN_F_LEGACY,
        LeAdvEventKind::AdvNonconnInd => SCAN_F_LEGACY,
        LeAdvEventKind::ScanRsp => SCAN_F_SCAN_RESPONSE | SCAN_F_LEGACY,
    }
}

fn ext_data_status_flags(status: LeExtAdvDataStatus) -> u16 {
    match status {
        LeExtAdvDataStatus::Complete => 0,
        LeExtAdvDataStatus::IncompleteMoreExpected => SCAN_F_DATA_INCOMPLETE,
        LeExtAdvDataStatus::IncompleteTruncated => SCAN_F_DATA_TRUNCATED,
        LeExtAdvDataStatus::Reserved => SCAN_F_DATA_INCOMPLETE,
    }
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
    restore_bonds(stack, cfg, &gatt.bond_slots);
    emit_local_oob_data(stack, cfg);

    let mut peripheral = stack.peripheral();
    let runner = stack.runner();
    let ble_main = async {
        let work = join(
            run_runner(runner, cfg),
            serve(&mut peripheral, &gatt, server, stack, cfg),
        );
        // Dual GAP role (RUNTIME_BLE_ROLE_DUAL): also run the central side
        // (connect + GATT client) so the device is a server (this advertise/serve
        // loop) AND a client at the same time, on two simultaneous links.
        #[cfg(feature = "central")]
        if cfg.role == 2 {
            select3(
                work,
                central_loop(stack, cfg, &gatt.bond_slots),
                wait_unload(),
            )
            .await;
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
    let Gatt {
        server,
        chars,
        props,
        bond_slots,
        stores,
        appearance_store,
        ..
    } = gatt;
    drop(server);
    drop(chars);
    drop(props);
    drop(bond_slots);
    for ptr in stores {
        // SAFETY: each ptr came from alloc_store (Box::into_raw); the table that
        // held the only `&'static mut` to it has just been dropped.
        unsafe {
            let _ = Box::from_raw(ptr);
        }
    }
    if let Some(ptr) = appearance_store {
        unsafe {
            let _ = Box::from_raw(ptr);
        }
    }
}

struct RtEventHandler<'a> {
    cfg: &'a RuntimeCfg,
}

impl trouble_host::prelude::EventHandler for RtEventHandler<'_> {
    fn on_adv_reports(&self, reports: trouble_host::prelude::LeAdvReportsIter) {
        for report in reports {
            let Ok(report) = report else { continue };
            if let Some(cb) = self.cfg.callbacks.on_scan_result_meta {
                cb(
                    report.addr.raw().as_ptr(),
                    addr_kind_to_c(report.addr_kind),
                    report.rssi,
                    report.data.as_ptr(),
                    report.data.len(),
                    legacy_adv_flags(report.event_kind),
                    1,
                    0,
                    127,
                    0xff,
                    self.cfg.user,
                );
            }
            if let Some(cb) = self.cfg.callbacks.on_scan_result_ext {
                cb(
                    report.addr.raw().as_ptr(),
                    addr_kind_to_c(report.addr_kind),
                    report.rssi,
                    report.data.as_ptr(),
                    report.data.len(),
                    self.cfg.user,
                );
            }
            if let Some(cb) = self.cfg.callbacks.on_scan_result {
                cb(
                    report.addr.raw().as_ptr(),
                    report.rssi,
                    report.data.as_ptr(),
                    report.data.len(),
                    self.cfg.user,
                );
            }
        }
    }

    fn on_ext_adv_reports(&self, reports: trouble_host::prelude::LeExtAdvReportsIter) {
        for report in reports {
            let Ok(report) = report else { continue };
            if let Some(cb) = self.cfg.callbacks.on_scan_result_meta {
                let mut flags = ext_data_status_flags(report.event_kind.data_status());
                if report.event_kind.connectable() {
                    flags |= SCAN_F_CONNECTABLE;
                }
                if report.event_kind.scannable() {
                    flags |= SCAN_F_SCANNABLE;
                }
                if report.event_kind.directed() {
                    flags |= SCAN_F_DIRECTED;
                }
                if report.event_kind.scan_response() {
                    flags |= SCAN_F_SCAN_RESPONSE;
                }
                if report.event_kind.legacy() {
                    flags |= SCAN_F_LEGACY;
                }
                cb(
                    report.addr.raw().as_ptr(),
                    addr_kind_to_c(report.addr_kind),
                    report.rssi,
                    report.data.as_ptr(),
                    report.data.len(),
                    flags,
                    phy_to_c(report.primary_adv_phy),
                    report.secondary_adv_phy.map_or(0, phy_to_c),
                    report.tx_power,
                    report.adv_sid,
                    self.cfg.user,
                );
            }
            if let Some(cb) = self.cfg.callbacks.on_scan_result_ext {
                cb(
                    report.addr.raw().as_ptr(),
                    addr_kind_to_c(report.addr_kind),
                    report.rssi,
                    report.data.as_ptr(),
                    report.data.len(),
                    self.cfg.user,
                );
            }
            if let Some(cb) = self.cfg.callbacks.on_scan_result {
                cb(
                    report.addr.raw().as_ptr(),
                    report.rssi,
                    report.data.as_ptr(),
                    report.data.len(),
                    self.cfg.user,
                );
            }
        }
    }
}

async fn run_runner<C: Controller, P: PacketPool>(mut runner: Runner<'_, C, P>, cfg: &RuntimeCfg) {
    let handler = RtEventHandler { cfg };
    let _ = runner.run_with_handler(&handler).await;
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

fn phy_to_c(phy: trouble_host::prelude::PhyKind) -> u8 {
    match phy {
        trouble_host::prelude::PhyKind::Le1M => 1,
        trouble_host::prelude::PhyKind::Le2M => 2,
        trouble_host::prelude::PhyKind::LeCoded | trouble_host::prelude::PhyKind::LeCodedS2 => 3,
    }
}

fn security_level_to_c(level: trouble_host::prelude::SecurityLevel) -> u8 {
    match level {
        trouble_host::prelude::SecurityLevel::EncryptedAuthenticated => 2,
        trouble_host::prelude::SecurityLevel::Encrypted => 1,
        trouble_host::prelude::SecurityLevel::NoEncryption => 0,
    }
}

fn c_to_security_level(level: u8) -> Option<trouble_host::prelude::SecurityLevel> {
    match level {
        0 => Some(trouble_host::prelude::SecurityLevel::NoEncryption),
        1 => Some(trouble_host::prelude::SecurityLevel::Encrypted),
        2 => Some(trouble_host::prelude::SecurityLevel::EncryptedAuthenticated),
        _ => None,
    }
}

fn bond_slot_count(cfg: &RuntimeCfg) -> u8 {
    let n = if cfg.bond_slot_count == 0 {
        BOND_SLOT_CAP as u8
    } else {
        cfg.bond_slot_count
    };
    n.min(BOND_SLOT_CAP as u8)
}

fn serialize_bond(bond: &BondInformation, out: &mut [u8; BOND_BLOB_LEN]) -> usize {
    out.fill(0);
    out[0] = BOND_BLOB_VERSION;
    out[1] = u8::from(bond.is_bonded) | u8::from(bond.identity.irk.is_some()) << 1;
    out[2] = security_level_to_c(bond.security_level);
    out[3] = bond.identity.addr.kind.as_raw();
    out[4..10].copy_from_slice(bond.identity.addr.addr.raw());
    out[10..26].copy_from_slice(&bond.ltk.to_le_bytes());
    if let Some(irk) = bond.identity.irk {
        out[26..42].copy_from_slice(&irk.to_le_bytes());
    }
    BOND_BLOB_LEN
}

fn deserialize_bond(blob: &[u8]) -> Option<BondInformation> {
    if blob.len() < 42 || blob[0] != BOND_BLOB_VERSION {
        return None;
    }
    let security_level = c_to_security_level(blob[2])?;
    let mut addr = [0u8; 6];
    addr.copy_from_slice(&blob[4..10]);
    let mut ltk = [0u8; 16];
    ltk.copy_from_slice(&blob[10..26]);
    let irk = if blob[1] & 0x02 != 0 {
        let mut raw = [0u8; 16];
        raw.copy_from_slice(&blob[26..42]);
        IdentityResolvingKey::from_le_bytes(raw)
    } else {
        None
    };
    Some(BondInformation::new(
        Identity {
            addr: Address::new(AddrKind::new(blob[3]), BdAddr::new(addr)),
            irk,
        },
        LongTermKey::from_le_bytes(ltk),
        security_level,
        blob[1] & 0x01 != 0,
    ))
}

fn restore_bonds(
    stack: &Stack<'_, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    cfg: &RuntimeCfg,
    slots: &RefCell<heapless::Vec<(Identity, u8), BOND_SLOT_CAP>>,
) {
    let Some(load) = cfg.callbacks.on_bond_load else {
        return;
    };
    for index in 0..bond_slot_count(cfg) {
        let mut blob = [0u8; BOND_BLOB_LEN];
        let len = load(index, blob.as_mut_ptr(), blob.len(), cfg.user).min(blob.len());
        if len == 0 {
            continue;
        }
        let Some(bond) = deserialize_bond(&blob[..len]) else {
            log_str(cfg, "[security] ignored invalid bond blob\0");
            continue;
        };
        let identity = bond.identity;
        if stack.add_bond_information(bond).is_ok() {
            let _ = slots.borrow_mut().push((identity, index));
        }
    }
}

fn store_bond(
    cfg: &RuntimeCfg,
    slots: &RefCell<heapless::Vec<(Identity, u8), BOND_SLOT_CAP>>,
    bond: &BondInformation,
) {
    let Some(store) = cfg.callbacks.on_bond_store else {
        return;
    };
    let slot_count = bond_slot_count(cfg);
    if slot_count == 0 {
        return;
    }
    let mut slot = 0u8;
    {
        let mut map = slots.borrow_mut();
        if let Some((_, existing)) = map.iter().find(|(identity, _)| *identity == bond.identity) {
            slot = *existing;
        } else if map.len() < slot_count as usize {
            slot = map.len() as u8;
            let _ = map.push((bond.identity, slot));
        } else if !map.is_empty() {
            map[0] = (bond.identity, 0);
        }
    }
    let mut blob = [0u8; BOND_BLOB_LEN];
    let len = serialize_bond(bond, &mut blob);
    store(slot, blob.as_ptr(), len, cfg.user);
}

fn emit_local_oob_data(
    stack: &Stack<'_, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    cfg: &RuntimeCfg,
) {
    if cfg.security_oob_available == 0 {
        return;
    }
    let Some(cb) = cfg.callbacks.on_oob_local_data else {
        return;
    };
    let oob = stack.get_local_oob_data();
    cb(oob.random.as_ptr(), oob.confirm.as_ptr(), cfg.user);
}

fn emit_security_event(
    cfg: &RuntimeCfg,
    event: u8,
    level: trouble_host::prelude::SecurityLevel,
    passkey: u32,
    bonded: bool,
) {
    if let Some(cb) = cfg.callbacks.on_security_event {
        cb(
            event,
            security_level_to_c(level),
            passkey,
            if bonded { 1 } else { 0 },
            cfg.user,
        );
    }
}

fn emit_security_state(conn: &Connection<'_, DefaultPacketPool>, cfg: &RuntimeCfg) {
    if let Some(cb) = cfg.callbacks.on_security_state {
        let level = conn.security_level().map(security_level_to_c).unwrap_or(0);
        let flags = if conn.is_bonded_peer() { 1 } else { 0 };
        cb(level, 0, flags, cfg.user);
    }
}

fn load_oob_data(cfg: &RuntimeCfg) -> Option<(OobData, OobData)> {
    emit_security_event(
        cfg,
        8,
        trouble_host::prelude::SecurityLevel::NoEncryption,
        0,
        false,
    );
    let cb = cfg.callbacks.on_oob_request?;
    let mut local_random = [0u8; 16];
    let mut local_confirm = [0u8; 16];
    let mut peer_random = [0u8; 16];
    let mut peer_confirm = [0u8; 16];
    if cb(
        local_random.as_mut_ptr(),
        local_confirm.as_mut_ptr(),
        peer_random.as_mut_ptr(),
        peer_confirm.as_mut_ptr(),
        cfg.user,
    ) == 0
    {
        return None;
    }
    Some((
        OobData {
            random: local_random,
            confirm: local_confirm,
        },
        OobData {
            random: peer_random,
            confirm: peer_confirm,
        },
    ))
}

async fn link_control(
    conn: &Connection<'_, DefaultPacketPool>,
    stack: &Stack<'_, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    cfg: &RuntimeCfg,
) {
    loop {
        link_control_once(conn, stack, cfg).await;
        Timer::after(Duration::from_millis(20)).await;
    }
}

async fn link_control_once(
    conn: &Connection<'_, DefaultPacketPool>,
    stack: &Stack<'_, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    cfg: &RuntimeCfg,
) {
    match LINK_CMD.swap(LCMD_NONE, Ordering::AcqRel) {
        LCMD_SET_PHY => {
            let phy = match LINK_PHY.load(Ordering::Acquire) {
                2 => trouble_host::prelude::PhyKind::Le2M,
                3 => trouble_host::prelude::PhyKind::LeCoded,
                _ => trouble_host::prelude::PhyKind::Le1M,
            };
            let _ = conn.set_phy(stack, phy).await;
        }
        LCMD_DLE => {
            let octets = LINK_DLE_OCTETS.load(Ordering::Acquire);
            let time_us = LINK_DLE_TIME_US.load(Ordering::Acquire);
            let octets = if octets == 0 {
                251
            } else {
                octets.min(u16::MAX as usize)
            };
            let time_us = if time_us == 0 {
                2120
            } else {
                time_us.min(u16::MAX as usize)
            };
            let _ = conn
                .update_data_length(stack, octets as u16, time_us as u16)
                .await;
        }
        LCMD_CONN_PARAMS => {
            let min_ms = LINK_CONN_MIN_MS.load(Ordering::Acquire);
            let max_ms = LINK_CONN_MAX_MS.load(Ordering::Acquire);
            let timeout_ms = LINK_CONN_TIMEOUT_MS.load(Ordering::Acquire);
            let params = RequestedConnParams {
                min_connection_interval: Duration::from_millis(if min_ms == 0 {
                    80
                } else {
                    min_ms as u64
                }),
                max_connection_interval: Duration::from_millis(if max_ms == 0 {
                    80
                } else {
                    max_ms as u64
                }),
                max_latency: LINK_CONN_LATENCY
                    .load(Ordering::Acquire)
                    .min(u16::MAX as usize) as u16,
                min_event_length: Duration::from_secs(0),
                max_event_length: Duration::from_secs(0),
                supervision_timeout: Duration::from_millis(if timeout_ms == 0 {
                    8000
                } else {
                    timeout_ms as u64
                }),
            };
            if params.is_valid() {
                let _ = conn.update_connection_params(stack, &params).await;
            } else {
                log_str(cfg, "[link] invalid connection params\0");
            }
        }
        LCMD_FRAME_SPACE => {
            let min_us = LINK_FRAME_SPACE_MIN_US.load(Ordering::Acquire);
            let max_us = LINK_FRAME_SPACE_MAX_US.load(Ordering::Acquire);
            let min_us = if min_us == 0 { 150 } else { min_us as u64 };
            let max_us = if max_us == 0 { 10_000 } else { max_us as u64 };
            if min_us > max_us {
                log_str(cfg, "[link] invalid frame spacing\0");
            } else {
                let phy_bits = LINK_FRAME_SPACE_PHY_MASK.load(Ordering::Acquire) as u8;
                let type_bits = LINK_FRAME_SPACE_TYPES.load(Ordering::Acquire) as u8;
                let phys = PhyMask::new()
                    .set_le_1m_phy(phy_bits == 0 || (phy_bits & 0x01) != 0)
                    .set_le_2m_phy(phy_bits == 0 || (phy_bits & 0x02) != 0)
                    .set_le_coded_phy(phy_bits == 0 || (phy_bits & 0x04) != 0);
                let spacing_types = SpacingTypes::new()
                    .set_t_ifs_acl_cp(type_bits == 0 || (type_bits & 0x01) != 0)
                    .set_t_ifs_acl_pc(type_bits == 0 || (type_bits & 0x02) != 0)
                    .set_t_mces((type_bits & 0x04) != 0)
                    .set_t_ifs_cis((type_bits & 0x08) != 0)
                    .set_t_mss_cis((type_bits & 0x10) != 0);
                let _ = conn
                    .update_frame_space(
                        stack,
                        Duration::from_micros(min_us),
                        Duration::from_micros(max_us),
                        phys,
                        spacing_types,
                    )
                    .await;
            }
        }
        LCMD_CONNECTION_RATE => {
            let min_ms = LINK_CONN_MIN_MS.load(Ordering::Acquire);
            let max_ms = LINK_CONN_MAX_MS.load(Ordering::Acquire);
            let timeout_ms = LINK_CONN_TIMEOUT_MS.load(Ordering::Acquire);
            let subrate_min = LINK_RATE_SUBRATE_MIN.load(Ordering::Acquire);
            let subrate_max = LINK_RATE_SUBRATE_MAX.load(Ordering::Acquire);
            let params = ConnectRateParams {
                min_connection_interval: Duration::from_millis(if min_ms == 0 {
                    80
                } else {
                    min_ms as u64
                }),
                max_connection_interval: Duration::from_millis(if max_ms == 0 {
                    80
                } else {
                    max_ms as u64
                }),
                subrate_min: if subrate_min == 0 {
                    1
                } else {
                    subrate_min.min(u16::MAX as usize) as u16
                },
                subrate_max: if subrate_max == 0 {
                    1
                } else {
                    subrate_max.min(u16::MAX as usize) as u16
                },
                max_latency: LINK_CONN_LATENCY
                    .load(Ordering::Acquire)
                    .min(u16::MAX as usize) as u16,
                continuation_number: LINK_RATE_CONTINUATION
                    .load(Ordering::Acquire)
                    .min(u16::MAX as usize) as u16,
                supervision_timeout: Duration::from_millis(if timeout_ms == 0 {
                    8000
                } else {
                    timeout_ms as u64
                }),
                min_ce_length: Duration::from_secs(0),
                max_ce_length: Duration::from_secs(0),
            };
            if params.min_connection_interval <= params.max_connection_interval
                && params.subrate_min <= params.subrate_max
            {
                let _ = conn.request_connection_rate(stack, &params).await;
            } else {
                log_str(cfg, "[link] invalid connection rate\0");
            }
        }
        LCMD_SECURITY_REQUEST => {
            let _ = conn.request_security();
        }
        LCMD_PASSKEY_CONFIRM => {
            let _ = conn.pass_key_confirm();
        }
        LCMD_PASSKEY_CANCEL => {
            let _ = conn.pass_key_cancel();
        }
        LCMD_PASSKEY_INPUT => {
            let _ = conn.pass_key_input(LINK_PASSKEY.load(Ordering::Acquire));
        }
        LCMD_READ_RSSI => {
            if let Ok(rssi) = conn.rssi(stack).await {
                if let Some(cb) = cfg.callbacks.on_rssi {
                    cb(rssi, cfg.user);
                }
            }
        }
        LCMD_READ_ATT_MTU => emit_att_mtu(conn, cfg),
        LCMD_READ_PHY => {
            if let Ok((tx_phy, rx_phy)) = conn.read_phy(stack).await {
                if let Some(cb) = cfg.callbacks.on_phy_update {
                    cb(phy_to_c(tx_phy), phy_to_c(rx_phy), cfg.user);
                }
            }
        }
        LCMD_READ_SECURITY => emit_security_state(conn, cfg),
        _ => {}
    }
}

fn emit_att_mtu(conn: &Connection<'_, DefaultPacketPool>, cfg: &RuntimeCfg) {
    if let Some(cb) = cfg.callbacks.on_att_mtu {
        cb(conn.att_mtu(), cfg.user);
    }
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
    L2CAP_DISCONNECT_REQ.store(false, Ordering::Release);
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
            if L2CAP_DISCONNECT_REQ.swap(false, Ordering::AcqRel) {
                L2CAP_SEND_REQ.store(false, Ordering::Release);
                writer.disconnect();
            }
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
const CLIENT_SERVICE_CAP: usize = 8;
#[cfg(feature = "central")]
const CLIENT_CHAR_CAP: usize = 32;
#[cfg(feature = "central")]
type ClientP<'a> =
    GattClient<'a, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool, CLIENT_SERVICE_CAP>;
#[cfg(feature = "central")]
type ClientCharStore = RefCell<heapless::Vec<(u16, Option<u16>), CLIENT_CHAR_CAP>>;

#[cfg(feature = "central")]
enum IdleCmd {
    Connect([u8; 6], u8),
    ScanStart,
}

/// One central session: connect (by config.peer_address or a queued connect
/// command), run a GATT client until disconnect/unload, repeat.
#[cfg(feature = "central")]
pub(crate) fn serve_central(
    mpsl: &MultiprotocolServiceLayer,
    stack: &Stack<'_, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    cfg: &RuntimeCfg,
) {
    let bond_slots = RefCell::new(heapless::Vec::new());
    restore_bonds(stack, cfg, &bond_slots);
    emit_local_oob_data(stack, cfg);
    // The host runner must run concurrently so HCI events (connection complete,
    // ATT responses, notifications) are processed while we connect and talk.
    let runner = stack.runner();
    block_on(select(
        mpsl.run(),
        select(
            run_runner(runner, cfg),
            central_loop(stack, cfg, &bond_slots),
        ),
    ));
}

#[cfg(feature = "central")]
async fn central_loop(
    stack: &Stack<'_, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    cfg: &RuntimeCfg,
    bond_slots: &RefCell<heapless::Vec<(Identity, u8), BOND_SLOT_CAP>>,
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
            peer_address(a, cfg.peer_address_kind)
        } else {
            match wait_idle_cmd().await {
                Some(IdleCmd::Connect(a, kind)) => peer_address(a, kind),
                Some(IdleCmd::ScanStart) => {
                    let mut scanner = Scanner::new(central);
                    let connect_after_scan = run_scan(&mut scanner, cfg).await;
                    central = scanner.into_inner();
                    match connect_after_scan {
                        Some((a, kind)) => peer_address(a, kind),
                        None => continue,
                    }
                }
                None => return, // unloaded
            }
        };

        let filt = [addr];
        let cc = ConnectConfig {
            scan_config: ScanConfig {
                filter_accept_list: &filt,
                ..Default::default()
            },
            connect_params: central_connect_params(cfg),
        };
        log_str(cfg, "[central] connecting\0");
        match central.connect(&cc).await {
            Ok(conn) => {
                if let Some(cb) = cfg.callbacks.on_connected {
                    cb(cfg.user);
                }
                run_central_conn(stack, &conn, cfg, bond_slots).await;
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

/// Park (polling) until an idle central command is queued.
#[cfg(feature = "central")]
async fn wait_idle_cmd() -> Option<IdleCmd> {
    loop {
        if UNLOAD_REQ.load(Ordering::Acquire) {
            return None;
        }
        match CENTRAL_CMD.swap(CCMD_NONE, Ordering::AcqRel) {
            CCMD_CONNECT => {
                let mut a = [0u8; 6];
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        core::ptr::addr_of!(CENTRAL_ADDR) as *const u8,
                        a.as_mut_ptr(),
                        6,
                    );
                }
                let kind = CENTRAL_ADDR_KIND.load(Ordering::Acquire) as u8;
                return Some(IdleCmd::Connect(a, kind));
            }
            CCMD_SCAN_START => return Some(IdleCmd::ScanStart),
            CCMD_NONE | CCMD_SCAN_STOP => {}
            _ => {
                // Drop commands that need an active link while idle.
            }
        }
        Timer::after(Duration::from_millis(30)).await;
    }
}

#[cfg(feature = "central")]
async fn run_scan(
    scanner: &mut Scanner<'_, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    cfg: &RuntimeCfg,
) -> Option<([u8; 6], u8)> {
    let interval_ms = SCAN_INTERVAL_MS.load(Ordering::Acquire);
    let window_ms = SCAN_WINDOW_MS.load(Ordering::Acquire);
    let timeout_ms = SCAN_TIMEOUT_MS.load(Ordering::Acquire);
    let mut filter_addr = [Address::random([0u8; 6]); 1];
    let filter_accept_list = if SCAN_FILTER_ADDR_ENABLED.load(Ordering::Acquire) {
        let mut a = [0u8; 6];
        unsafe {
            core::ptr::copy_nonoverlapping(
                core::ptr::addr_of!(SCAN_FILTER_ADDR) as *const u8,
                a.as_mut_ptr(),
                6,
            );
        }
        filter_addr[0] = peer_address(a, SCAN_FILTER_ADDR_KIND.load(Ordering::Acquire) as u8);
        &filter_addr[..]
    } else {
        &[]
    };
    let scan_config = ScanConfig {
        active: SCAN_ACTIVE.load(Ordering::Acquire),
        filter_accept_list,
        phys: scan_phys(),
        interval: Duration::from_millis(if interval_ms == 0 {
            100
        } else {
            interval_ms as u64
        }),
        window: Duration::from_millis(if window_ms == 0 { 50 } else { window_ms as u64 }),
        timeout: Duration::from_millis(timeout_ms as u64),
        filter_duplicates: if SCAN_FILTER_DUPLICATES.load(Ordering::Acquire) {
            FilterDuplicates::Enabled
        } else {
            FilterDuplicates::Disabled
        },
        ..Default::default()
    };
    log_str(cfg, "[central] scanning\0");
    let _session = match scanner.scan(&scan_config).await {
        Ok(s) => s,
        Err(_) => {
            log_str(cfg, "[central] scan failed\0");
            return None;
        }
    };

    let stop_at = if timeout_ms == 0 {
        None
    } else {
        Some(embassy_time::Instant::now() + Duration::from_millis(timeout_ms as u64))
    };
    loop {
        if UNLOAD_REQ.load(Ordering::Acquire) {
            return None;
        }
        if stop_at.is_some_and(|at| embassy_time::Instant::now() >= at) {
            log_str(cfg, "[central] scan timeout\0");
            return None;
        }
        match CENTRAL_CMD.swap(CCMD_NONE, Ordering::AcqRel) {
            CCMD_SCAN_STOP => {
                log_str(cfg, "[central] scan stopped\0");
                return None;
            }
            CCMD_CONNECT => {
                let mut a = [0u8; 6];
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        core::ptr::addr_of!(CENTRAL_ADDR) as *const u8,
                        a.as_mut_ptr(),
                        6,
                    );
                }
                let kind = CENTRAL_ADDR_KIND.load(Ordering::Acquire) as u8;
                return Some((a, kind));
            }
            CCMD_SCAN_START | CCMD_NONE => {}
            _ => {}
        }
        Timer::after(Duration::from_millis(30)).await;
    }
}

#[cfg(feature = "central")]
fn scan_phys() -> PhySet {
    match SCAN_PHY_OPTIONS.load(Ordering::Acquire) & 0x0e {
        0x04 => PhySet::M2,
        0x08 => PhySet::Coded,
        0x06 => PhySet::M1M2,
        0x0a => PhySet::M1Coded,
        0x0c => PhySet::M2Coded,
        0x0e => PhySet::M1M2Coded,
        _ => PhySet::M1,
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
    let store: ClientCharStore = RefCell::new(heapless::Vec::new());

    let commands = async {
        loop {
            match CENTRAL_CMD.swap(CCMD_NONE, Ordering::AcqRel) {
                CCMD_DISCONNECT => conn.disconnect(),
                CCMD_DISCOVER_SERVICES => client_discover_services(&client, cfg).await,
                CCMD_DISCOVER => client_discover(&client, &store, cfg).await,
                CCMD_DISCOVER_ALL => client_discover_all(&client, &store, cfg).await,
                CCMD_DISCOVER_DESCRIPTORS => client_discover_descriptors(&client, cfg).await,
                CCMD_READ_BY_UUID => client_read_by_uuid(&client, cfg).await,
                cmd @ (CCMD_READ | CCMD_READ_BLOB) => {
                    let packed = CENTRAL_HANDLE.load(Ordering::Acquire);
                    let h = packed as u16;
                    let offset = (packed >> 16) as u16;
                    let op = if cmd == CCMD_READ_BLOB {
                        CLIENT_OP_READ_BLOB
                    } else {
                        CLIENT_OP_READ
                    };
                    let mut buf = [0u8; VALUE_LEN];
                    let read = if cmd == CCMD_READ_BLOB {
                        client.read_handle_blob(h, offset, &mut buf).await
                    } else {
                        client.read_handle(h, &mut buf).await
                    };
                    match read {
                        Ok(n) => {
                            if let Some(cb) = cfg.callbacks.on_read {
                                cb(h, buf.as_ptr(), n.min(VALUE_LEN), cfg.user);
                            }
                            emit_client_status(cfg, op, CLIENT_STATUS_OK, h);
                        }
                        Err(_) => {
                            log_str(cfg, "[central] read failed\0");
                            emit_client_status(cfg, op, CLIENT_STATUS_FAILED, h);
                        }
                    }
                }
                cmd @ (CCMD_WRITE | CCMD_WRITE_NO_RSP) => {
                    let h = CENTRAL_HANDLE.load(Ordering::Acquire) as u16;
                    let len = SEND_LEN
                        .load(Ordering::Acquire)
                        .min(VALUE_LEN)
                        .min(SEND_BUF_CAP);
                    let mut wbuf = [0u8; VALUE_LEN];
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            core::ptr::addr_of!(SEND_BUF) as *const u8,
                            wbuf.as_mut_ptr(),
                            len,
                        );
                    }
                    let op = if cmd == CCMD_WRITE_NO_RSP {
                        CLIENT_OP_WRITE_NO_RSP
                    } else {
                        CLIENT_OP_WRITE
                    };
                    let result = if cmd == CCMD_WRITE_NO_RSP {
                        client.write_handle_without_response(h, &wbuf[..len]).await
                    } else {
                        client.write_handle(h, &wbuf[..len]).await
                    };
                    emit_client_status(
                        cfg,
                        op,
                        if result.is_ok() {
                            CLIENT_STATUS_OK
                        } else {
                            CLIENT_STATUS_FAILED
                        },
                        h,
                    );
                }
                cmd @ (CCMD_SUBSCRIBE | CCMD_SUBSCRIBE_INDICATE) => {
                    let h = CENTRAL_HANDLE.load(Ordering::Acquire) as u16;
                    let cccd = store
                        .borrow()
                        .iter()
                        .find(|(handle, _)| *handle == h)
                        .and_then(|(_, cccd)| *cccd)
                        .unwrap_or(h + 1);
                    let value = if cmd == CCMD_SUBSCRIBE_INDICATE {
                        [0x02, 0x00]
                    } else {
                        [0x01, 0x00]
                    };
                    let result = client.write_handle(cccd, &value).await;
                    emit_client_status(
                        cfg,
                        if cmd == CCMD_SUBSCRIBE_INDICATE {
                            CLIENT_OP_SUBSCRIBE_INDICATE
                        } else {
                            CLIENT_OP_SUBSCRIBE
                        },
                        if result.is_ok() {
                            CLIENT_STATUS_OK
                        } else {
                            CLIENT_STATUS_FAILED
                        },
                        h,
                    );
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
                if n.is_indication() {
                    let _ = client.confirm_indication().await;
                }
            },
            None => core::future::pending::<()>().await,
        }
    };

    // client.task() drives ATT responses + notifications; it returns when the
    // link drops, ending the session.
    select3(client.task(), join(commands, notifs), wait_unload()).await;
}

#[cfg(feature = "central")]
const CLIENT_STATUS_OK: i8 = 0;
#[cfg(feature = "central")]
const CLIENT_STATUS_FAILED: i8 = -1;
#[cfg(feature = "central")]
const CLIENT_OP_DISCOVER_SERVICES: u8 = 1;
#[cfg(feature = "central")]
const CLIENT_OP_DISCOVER_CHARS: u8 = 2;
#[cfg(feature = "central")]
const CLIENT_OP_DISCOVER_ALL: u8 = 3;
#[cfg(feature = "central")]
const CLIENT_OP_DISCOVER_DESCRIPTORS: u8 = 4;
#[cfg(feature = "central")]
const CLIENT_OP_READ: u8 = 5;
#[cfg(feature = "central")]
const CLIENT_OP_READ_BLOB: u8 = 6;
#[cfg(feature = "central")]
const CLIENT_OP_WRITE: u8 = 7;
#[cfg(feature = "central")]
const CLIENT_OP_WRITE_NO_RSP: u8 = 8;
#[cfg(feature = "central")]
const CLIENT_OP_SUBSCRIBE: u8 = 9;
#[cfg(feature = "central")]
const CLIENT_OP_SUBSCRIBE_INDICATE: u8 = 10;
#[cfg(feature = "central")]
const CLIENT_OP_READ_BY_UUID: u8 = 11;

#[cfg(feature = "central")]
fn emit_client_status(cfg: &RuntimeCfg, op: u8, status: i8, handle: u16) {
    if let Some(cb) = cfg.callbacks.on_client_status {
        cb(op, status, handle, cfg.user);
    }
}

#[cfg(feature = "central")]
fn report_service(service: &trouble_host::gatt::ServiceHandle, cfg: &RuntimeCfg) {
    if let Some(cb) = cfg.callbacks.on_service {
        let range = service.handle_range();
        let raw_uuid = service.uuid();
        let raw = raw_uuid.as_raw();
        cb(
            *range.start(),
            *range.end(),
            raw.as_ptr(),
            raw.len() as u8,
            cfg.user,
        );
    }
}

#[cfg(feature = "central")]
async fn report_service_chars(
    client: &ClientP<'_>,
    service: &trouble_host::gatt::ServiceHandle,
    store: &ClientCharStore,
    cfg: &RuntimeCfg,
) {
    if let Ok(chars) = client.characteristics::<CLIENT_CHAR_CAP>(service).await {
        for c in chars.iter() {
            let _ = store.borrow_mut().push((c.handle, c.cccd_handle));
            if let Some(cb) = cfg.callbacks.on_discovered {
                let raw = c.uuid.as_raw();
                cb(
                    c.handle,
                    raw.as_ptr(),
                    raw.len() as u8,
                    props_to_c(c.props.to_raw()),
                    cfg.user,
                );
            }
        }
    }
}

#[cfg(feature = "central")]
async fn client_discover_all(client: &ClientP<'_>, store: &ClientCharStore, cfg: &RuntimeCfg) {
    match client.services().await {
        Ok(svcs) => {
            for s in svcs.iter() {
                report_service(s, cfg);
                report_service_chars(client, s, store, cfg).await;
            }
            emit_client_status(cfg, CLIENT_OP_DISCOVER_ALL, CLIENT_STATUS_OK, 0);
        }
        Err(_) => {
            log_str(cfg, "[central] discover all failed\0");
            emit_client_status(cfg, CLIENT_OP_DISCOVER_ALL, CLIENT_STATUS_FAILED, 0);
        }
    }
}

#[cfg(feature = "central")]
async fn client_discover_services(client: &ClientP<'_>, cfg: &RuntimeCfg) {
    match client.services().await {
        Ok(svcs) => {
            for s in svcs.iter() {
                report_service(s, cfg);
            }
            emit_client_status(cfg, CLIENT_OP_DISCOVER_SERVICES, CLIENT_STATUS_OK, 0);
        }
        Err(_) => {
            log_str(cfg, "[central] discover services failed\0");
            emit_client_status(cfg, CLIENT_OP_DISCOVER_SERVICES, CLIENT_STATUS_FAILED, 0);
        }
    }
}

/// Discover the characteristics of the service whose UUID is in CENTRAL_UUID;
/// report each via on_discovered and remember them (for subscribe's CCCD).
#[cfg(feature = "central")]
async fn client_discover(client: &ClientP<'_>, store: &ClientCharStore, cfg: &RuntimeCfg) {
    let len = CENTRAL_UUID_LEN.load(Ordering::Acquire);
    let uuid = unsafe { uuid_from(core::ptr::addr_of!(CENTRAL_UUID) as *const u8, len as u8) };
    let svcs = match client.services_by_uuid(&uuid).await {
        Ok(s) => s,
        Err(_) => {
            log_str(cfg, "[central] discover: service not found\0");
            emit_client_status(cfg, CLIENT_OP_DISCOVER_CHARS, CLIENT_STATUS_FAILED, 0);
            return;
        }
    };
    for s in svcs.iter() {
        report_service_chars(client, s, store, cfg).await;
    }
    emit_client_status(cfg, CLIENT_OP_DISCOVER_CHARS, CLIENT_STATUS_OK, 0);
}

#[cfg(feature = "central")]
async fn client_discover_descriptors(client: &ClientP<'_>, cfg: &RuntimeCfg) {
    let range = CENTRAL_HANDLE.load(Ordering::Acquire);
    let start = (range >> 16) as u16;
    let end = range as u16;
    if start == 0 || end < start {
        emit_client_status(
            cfg,
            CLIENT_OP_DISCOVER_DESCRIPTORS,
            CLIENT_STATUS_FAILED,
            start,
        );
        return;
    }
    let result = client
        .find_information(start, end, |handle, uuid| {
            if let Some(cb) = cfg.callbacks.on_descriptor {
                let raw = uuid.as_raw();
                cb(handle, raw.as_ptr(), raw.len() as u8, cfg.user);
            }
            core::ops::ControlFlow::<()>::Continue(())
        })
        .await;
    emit_client_status(
        cfg,
        CLIENT_OP_DISCOVER_DESCRIPTORS,
        if result.is_ok() {
            CLIENT_STATUS_OK
        } else {
            CLIENT_STATUS_FAILED
        },
        start,
    );
}

#[cfg(feature = "central")]
async fn client_read_by_uuid(client: &ClientP<'_>, cfg: &RuntimeCfg) {
    let packed = CENTRAL_HANDLE.load(Ordering::Acquire);
    let start = (packed >> 16) as u16;
    let end = packed as u16;
    let len = CENTRAL_UUID_LEN.load(Ordering::Acquire) as u8;
    let uuid = unsafe { uuid_from(core::ptr::addr_of!(CENTRAL_UUID) as *const u8, len) };
    let result = client
        .read_by_type(start, end, &uuid, |handle, data| {
            if let Some(cb) = cfg.callbacks.on_read {
                let n = data.len().min(VALUE_LEN);
                cb(handle, data.as_ptr(), n, cfg.user);
            }
            core::ops::ControlFlow::<()>::Continue(())
        })
        .await;
    emit_client_status(
        cfg,
        CLIENT_OP_READ_BY_UUID,
        if result.is_ok() {
            CLIENT_STATUS_OK
        } else {
            CLIENT_STATUS_FAILED
        },
        start,
    );
}

/// Run a central connection: the GATT client, plus (when l2cap is enabled and a
/// PSM is configured) an L2CAP channel to the peer, concurrently.
#[cfg(feature = "central")]
async fn run_central_conn(
    stack: &Stack<'_, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    conn: &Connection<'_, DefaultPacketPool>,
    cfg: &RuntimeCfg,
    bond_slots: &RefCell<heapless::Vec<(Identity, u8), BOND_SLOT_CAP>>,
) {
    if cfg.security_bondable != 0 {
        let _ = conn.set_bondable(true);
    }
    if cfg.security_oob_available != 0 {
        let _ = conn.set_oob_available(true);
    }
    if cfg.security_request_on_connect != 0 {
        let _ = conn.request_security();
    }

    #[cfg(feature = "l2cap")]
    {
        if cfg.l2cap_psm != 0 {
            select3(
                client_session(stack, conn, cfg),
                l2cap_central(stack, conn, cfg),
                central_connection_events(conn, stack, cfg, bond_slots),
            )
            .await;
            return;
        }
    }
    select3(
        client_session(stack, conn, cfg),
        central_connection_events(conn, stack, cfg, bond_slots),
        wait_unload(),
    )
    .await;
}

#[cfg(feature = "central")]
async fn central_connection_events(
    conn: &Connection<'_, DefaultPacketPool>,
    stack: &Stack<'_, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    cfg: &RuntimeCfg,
    bond_slots: &RefCell<heapless::Vec<(Identity, u8), BOND_SLOT_CAP>>,
) {
    let mtu_report_at = embassy_time::Instant::now() + Duration::from_millis(400);
    let mut mtu_reported = false;
    loop {
        let tick = async {
            link_control_once(conn, stack, cfg).await;
            if !mtu_reported && embassy_time::Instant::now() >= mtu_report_at {
                emit_att_mtu(conn, cfg);
                mtu_reported = true;
            }
            Timer::after(Duration::from_millis(20)).await;
        };
        match select(conn.next(), tick).await {
            Either::First(event) => match event {
                ConnectionEvent::Disconnected { reason } => {
                    use core::fmt::Write;
                    let mut s: heapless::String<48> = heapless::String::new();
                    let _ = write!(
                        s,
                        "[link] disconnected reason=0x{:02x}\0",
                        reason.into_inner()
                    );
                    log_str(cfg, &s);
                    break;
                }
                ConnectionEvent::RequestConnectionParams(req) => {
                    let _ = req.accept(None, stack).await;
                }
                ConnectionEvent::ConnectionParamsUpdated {
                    conn_interval,
                    peripheral_latency,
                    supervision_timeout,
                } => {
                    if let Some(cb) = cfg.callbacks.on_conn_params {
                        cb(
                            conn_interval.as_millis().min(u16::MAX as u64) as u16,
                            peripheral_latency,
                            supervision_timeout.as_millis().min(u16::MAX as u64) as u16,
                            cfg.user,
                        );
                    }
                }
                ConnectionEvent::PhyUpdated { tx_phy, rx_phy } => {
                    if let Some(cb) = cfg.callbacks.on_phy_update {
                        cb(phy_to_c(tx_phy), phy_to_c(rx_phy), cfg.user);
                    }
                }
                ConnectionEvent::DataLengthUpdated {
                    max_tx_octets,
                    max_rx_octets,
                    ..
                } => {
                    if let Some(cb) = cfg.callbacks.on_data_length_update {
                        cb(max_tx_octets, max_rx_octets, cfg.user);
                    }
                }
                ConnectionEvent::FrameSpaceUpdated { frame_space, .. } => {
                    if let Some(cb) = cfg.callbacks.on_frame_space {
                        cb(
                            frame_space.as_micros().min(u32::MAX as u64) as u32,
                            cfg.user,
                        );
                    }
                }
                ConnectionEvent::ConnectionRateChanged {
                    conn_interval,
                    subrate_factor,
                    peripheral_latency,
                    continuation_number,
                    supervision_timeout,
                } => {
                    if let Some(cb) = cfg.callbacks.on_connection_rate {
                        cb(
                            conn_interval.as_millis().min(u16::MAX as u64) as u16,
                            subrate_factor,
                            peripheral_latency,
                            continuation_number,
                            supervision_timeout.as_millis().min(u16::MAX as u64) as u16,
                            cfg.user,
                        );
                    }
                }
                ConnectionEvent::PassKeyDisplay(key) => {
                    emit_security_event(
                        cfg,
                        1,
                        trouble_host::prelude::SecurityLevel::NoEncryption,
                        key.value(),
                        false,
                    );
                }
                ConnectionEvent::PassKeyConfirm(key) => {
                    emit_security_event(
                        cfg,
                        2,
                        trouble_host::prelude::SecurityLevel::NoEncryption,
                        key.value(),
                        false,
                    );
                }
                ConnectionEvent::PassKeyInput => {
                    emit_security_event(
                        cfg,
                        3,
                        trouble_host::prelude::SecurityLevel::NoEncryption,
                        0,
                        false,
                    );
                }
                ConnectionEvent::PairingComplete {
                    security_level,
                    bond,
                } => {
                    if let Some(b) = bond.as_ref() {
                        store_bond(cfg, bond_slots, b);
                    }
                    emit_security_event(
                        cfg,
                        4,
                        security_level,
                        0,
                        bond.as_ref().is_some_and(|b| b.is_bonded),
                    );
                }
                ConnectionEvent::PairingFailed(_) => {
                    emit_security_event(
                        cfg,
                        5,
                        trouble_host::prelude::SecurityLevel::NoEncryption,
                        0,
                        false,
                    );
                }
                ConnectionEvent::BondLost => {
                    emit_security_event(
                        cfg,
                        6,
                        trouble_host::prelude::SecurityLevel::NoEncryption,
                        0,
                        false,
                    );
                }
                ConnectionEvent::Encrypted {
                    security_level,
                    bond,
                } => {
                    if let Some(b) = bond.as_ref() {
                        store_bond(cfg, bond_slots, b);
                    }
                    emit_security_event(
                        cfg,
                        7,
                        security_level,
                        0,
                        bond.as_ref().is_some_and(|b| b.is_bonded),
                    );
                }
                ConnectionEvent::OobRequest => {
                    if let Some((local, peer)) = load_oob_data(cfg) {
                        let _ = conn.provide_oob_data(local, peer);
                    } else {
                        log_str(cfg, "[security] OOB data requested\0");
                    }
                }
            },
            Either::Second(()) => {}
        }
    }
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
    if cfg.nonconnectable != 0 {
        while !UNLOAD_REQ.load(Ordering::Acquire) {
            if advertise_nonconnectable(peripheral, cfg).await.is_ok() {
                break;
            }
            Timer::after(Duration::from_secs(1)).await;
        }
        return;
    }

    loop {
        match advertise_connectable(peripheral, cfg).await {
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

async fn advertise_connectable<'a>(
    peripheral: &mut Peripheral<'a, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    cfg: &RuntimeCfg,
) -> Result<Connection<'a, DefaultPacketPool>, BleHostError<nrf_sdc::Error>> {
    if let Some(peer) = directed_peer(cfg) {
        let adv_params = advertising_params(cfg);
        let advertisement = if cfg.directed_high_duty != 0 {
            Advertisement::ConnectableNonscannableDirectedHighDuty { peer }
        } else {
            Advertisement::ConnectableNonscannableDirected { peer }
        };
        let advertiser = peripheral.advertise(&adv_params, advertisement).await?;
        return Ok(advertiser.accept().await?);
    }

    let (adv, adv_len, scan_data, adv_params) = advertising_parts(cfg)?;
    let advertiser = peripheral
        .advertise(
            &adv_params,
            Advertisement::ConnectableScannableUndirected {
                adv_data: &adv[..adv_len],
                scan_data,
            },
        )
        .await?;
    Ok(advertiser.accept().await?)
}

async fn advertise_nonconnectable(
    peripheral: &mut Peripheral<'_, nrf_sdc::SoftdeviceController<'static>, DefaultPacketPool>,
    cfg: &RuntimeCfg,
) -> Result<(), BleHostError<nrf_sdc::Error>> {
    let (adv, adv_len, scan_data, adv_params) = advertising_parts(cfg)?;
    if scan_data.is_empty() {
        let _advertiser = peripheral
            .advertise(
                &adv_params,
                Advertisement::NonconnectableNonscannableUndirected {
                    adv_data: &adv[..adv_len],
                },
            )
            .await?;
        wait_unload().await;
    } else {
        let _advertiser = peripheral
            .advertise(
                &adv_params,
                Advertisement::NonconnectableScannableUndirected {
                    adv_data: &adv[..adv_len],
                    scan_data,
                },
            )
            .await?;
        wait_unload().await;
    }
    Ok(())
}

fn advertising_parts<'a>(
    cfg: &'a RuntimeCfg,
) -> Result<([u8; 31], usize, &'a [u8], AdvertisementParameters), BleHostError<nrf_sdc::Error>> {
    let name = unsafe { cstr_or(cfg.device_name, "RUNTIME-BLE") };
    let flags = discoverability_flags(cfg);
    let man: &[u8] = if !cfg.manufacturer_data.is_null() && cfg.manufacturer_data_len > 0 {
        unsafe {
            core::slice::from_raw_parts(cfg.manufacturer_data, cfg.manufacturer_data_len as usize)
        }
    } else {
        &[]
    };
    let (adv, adv_len) = advertising_payload(cfg, flags, man, name)
        .ok_or(BleHostError::BleHost(trouble_host::Error::InvalidValue))?;
    let scan_data: &[u8] = if !cfg.scan_response_data.is_null() && cfg.scan_response_data_len > 0 {
        unsafe {
            core::slice::from_raw_parts(
                cfg.scan_response_data,
                cfg.scan_response_data_len.min(31) as usize,
            )
        }
    } else {
        &[]
    };
    let adv_params = advertising_params(cfg);
    Ok((adv, adv_len, scan_data, adv_params))
}

fn discoverability_flags(cfg: &RuntimeCfg) -> u8 {
    match cfg.discoverable {
        1 => LE_LIMITED_DISCOVERABLE | BR_EDR_NOT_SUPPORTED,
        2 => BR_EDR_NOT_SUPPORTED,
        _ => LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED,
    }
}

fn advertising_params(cfg: &RuntimeCfg) -> AdvertisementParameters {
    let min_ms = if cfg.adv_interval_min_ms == 0 {
        30
    } else {
        cfg.adv_interval_min_ms
    } as u64;
    let max_ms = if cfg.adv_interval_max_ms == 0 {
        60
    } else {
        cfg.adv_interval_max_ms
    } as u64;
    AdvertisementParameters {
        interval_min: Duration::from_millis(min_ms),
        interval_max: Duration::from_millis(max_ms),
        tx_power: adv_tx_power(cfg),
        channel_map: adv_channel_map(cfg),
        ..Default::default()
    }
}

fn adv_channel_map(cfg: &RuntimeCfg) -> Option<AdvChannelMap> {
    let bits = cfg.adv_channel_map & 0x07;
    if bits == 0 {
        None
    } else {
        Some(
            AdvChannelMap::new()
                .enable_channel_37((bits & 0x01) != 0)
                .enable_channel_38((bits & 0x02) != 0)
                .enable_channel_39((bits & 0x04) != 0),
        )
    }
}

fn directed_peer(cfg: &RuntimeCfg) -> Option<Address> {
    if cfg.directed_peer_address.is_null() {
        return None;
    }
    let mut addr = [0u8; 6];
    unsafe {
        core::ptr::copy_nonoverlapping(cfg.directed_peer_address, addr.as_mut_ptr(), 6);
    }
    Some(peer_address(addr, cfg.directed_peer_address_kind))
}

fn advertising_payload(
    cfg: &RuntimeCfg,
    flags: u8,
    manufacturer_data: &[u8],
    name: &str,
) -> Option<([u8; 31], usize)> {
    let mut adv = [0u8; 31];
    if !cfg.adv_data.is_null() && cfg.adv_data_len > 0 {
        let len = cfg.adv_data_len as usize;
        if len > adv.len() {
            return None;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(cfg.adv_data, adv.as_mut_ptr(), len);
        }
        return Some((adv, len));
    }
    let adv_len = build_adv_payload(cfg, flags, manufacturer_data, name, &mut adv)?;
    Some((adv, adv_len))
}

fn adv_tx_power(cfg: &RuntimeCfg) -> TxPower {
    if cfg.adv_tx_power_present == 0 {
        return TxPower::ZerodBm;
    }
    match cfg.adv_tx_power_dbm {
        -40 => TxPower::Minus40dBm,
        -20 => TxPower::Minus20dBm,
        -16 => TxPower::Minus16dBm,
        -12 => TxPower::Minus12dBm,
        -8 => TxPower::Minus8dBm,
        -4 => TxPower::Minus4dBm,
        2 => TxPower::Plus2dBm,
        3 => TxPower::Plus3dBm,
        4 => TxPower::Plus4dBm,
        5 => TxPower::Plus5dBm,
        6 => TxPower::Plus6dBm,
        7 => TxPower::Plus7dBm,
        8 => TxPower::Plus8dBm,
        10 => TxPower::Plus10dBm,
        12 => TxPower::Plus12dBm,
        14 => TxPower::Plus14dBm,
        16 => TxPower::Plus16dBm,
        18 => TxPower::Plus18dBm,
        20 => TxPower::Plus20dBm,
        _ => TxPower::ZerodBm,
    }
}

fn push_ad(dst: &mut [u8; 31], pos: &mut usize, ty: u8, data: &[u8]) -> bool {
    let need = data.len() + 2;
    if *pos + need > dst.len() || data.len() > 254 {
        return false;
    }
    dst[*pos] = (data.len() + 1) as u8;
    dst[*pos + 1] = ty;
    dst[*pos + 2..*pos + need].copy_from_slice(data);
    *pos += need;
    true
}

fn build_adv_payload(
    cfg: &RuntimeCfg,
    flags: u8,
    manufacturer_data: &[u8],
    name: &str,
    dst: &mut [u8; 31],
) -> Option<usize> {
    let mut pos = 0usize;
    if !push_ad(dst, &mut pos, 0x01, &[flags]) {
        return None;
    }
    if cfg.manufacturer_id != 0 || !manufacturer_data.is_empty() {
        let mut man = [0u8; 29];
        let total = 2 + manufacturer_data.len();
        if total > man.len() {
            return None;
        }
        man[..2].copy_from_slice(&cfg.manufacturer_id.to_le_bytes());
        man[2..total].copy_from_slice(manufacturer_data);
        if !push_ad(dst, &mut pos, 0xff, &man[..total]) {
            return None;
        }
    }
    if !cfg.adv_service_uuid.is_null()
        && (cfg.adv_service_uuid_len == 2 || cfg.adv_service_uuid_len == 16)
    {
        let uuid = unsafe {
            core::slice::from_raw_parts(cfg.adv_service_uuid, cfg.adv_service_uuid_len as usize)
        };
        let ty = if cfg.adv_service_uuid_len == 2 {
            0x03
        } else {
            0x07
        };
        if !push_ad(dst, &mut pos, ty, uuid) {
            return None;
        }
    }
    if !cfg.adv_service_data_uuid.is_null()
        && !cfg.adv_service_data.is_null()
        && cfg.adv_service_data_len > 0
        && (cfg.adv_service_data_uuid_len == 2 || cfg.adv_service_data_uuid_len == 16)
    {
        let uuid_len = cfg.adv_service_data_uuid_len as usize;
        let data_len = cfg.adv_service_data_len as usize;
        let total = uuid_len + data_len;
        let mut service_data = [0u8; 29];
        if total > service_data.len() {
            return None;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(
                cfg.adv_service_data_uuid,
                service_data.as_mut_ptr(),
                uuid_len,
            );
            core::ptr::copy_nonoverlapping(
                cfg.adv_service_data,
                service_data.as_mut_ptr().add(uuid_len),
                data_len,
            );
        }
        let ty = if cfg.adv_service_data_uuid_len == 2 {
            0x16
        } else {
            0x21
        };
        if !push_ad(dst, &mut pos, ty, &service_data[..total]) {
            return None;
        }
    }
    if cfg.adv_appearance != 0 {
        let appearance = if cfg.appearance == 0 {
            u16::from(appearance::power_device::GENERIC_POWER_DEVICE)
        } else {
            cfg.appearance
        };
        if !push_ad(dst, &mut pos, 0x19, &appearance.to_le_bytes()) {
            return None;
        }
    }
    if cfg.adv_tx_power_present != 0 && !push_ad(dst, &mut pos, 0x0a, &[cfg.adv_tx_power_dbm as u8])
    {
        return None;
    }
    if !push_ad(dst, &mut pos, 0x09, name.as_bytes()) {
        let avail = dst.len().saturating_sub(pos);
        if avail < 3 {
            return None;
        }
        let short_len = name.len().min(avail - 2);
        if !push_ad(dst, &mut pos, 0x08, &name.as_bytes()[..short_len]) {
            return None;
        }
    }
    Some(pos)
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
    if cfg.security_bondable != 0 {
        let _ = conn.set_bondable(true);
    }
    if cfg.security_oob_available != 0 {
        let _ = conn.set_oob_available(true);
    }
    if cfg.security_request_on_connect != 0 {
        let _ = conn.request_security();
    }

    let events = async {
        loop {
            match gconn.next().await {
                GattConnectionEvent::Disconnected { reason } => {
                    use core::fmt::Write;
                    let mut s: heapless::String<48> = heapless::String::new();
                    let _ = write!(
                        s,
                        "[link] disconnected reason=0x{:02x}\0",
                        reason.into_inner()
                    );
                    log_str(cfg, &s);
                    break;
                }
                GattConnectionEvent::RequestConnectionParams(req) => {
                    let _ = req.accept(None, stack).await;
                }
                GattConnectionEvent::ConnectionParamsUpdated {
                    conn_interval,
                    peripheral_latency,
                    supervision_timeout,
                } => {
                    if let Some(cb) = cfg.callbacks.on_conn_params {
                        cb(
                            conn_interval.as_millis().min(u16::MAX as u64) as u16,
                            peripheral_latency,
                            supervision_timeout.as_millis().min(u16::MAX as u64) as u16,
                            cfg.user,
                        );
                    }
                }
                GattConnectionEvent::PhyUpdated { tx_phy, rx_phy } => {
                    if let Some(cb) = cfg.callbacks.on_phy_update {
                        cb(phy_to_c(tx_phy), phy_to_c(rx_phy), cfg.user);
                    }
                }
                GattConnectionEvent::DataLengthUpdated {
                    max_tx_octets,
                    max_rx_octets,
                    ..
                } => {
                    if let Some(cb) = cfg.callbacks.on_data_length_update {
                        cb(max_tx_octets, max_rx_octets, cfg.user);
                    }
                }
                GattConnectionEvent::FrameSpaceUpdated { frame_space, .. } => {
                    if let Some(cb) = cfg.callbacks.on_frame_space {
                        cb(
                            frame_space.as_micros().min(u32::MAX as u64) as u32,
                            cfg.user,
                        );
                    }
                }
                GattConnectionEvent::ConnectionRateChanged {
                    conn_interval,
                    subrate_factor,
                    peripheral_latency,
                    continuation_number,
                    supervision_timeout,
                } => {
                    if let Some(cb) = cfg.callbacks.on_connection_rate {
                        cb(
                            conn_interval.as_millis().min(u16::MAX as u64) as u16,
                            subrate_factor,
                            peripheral_latency,
                            continuation_number,
                            supervision_timeout.as_millis().min(u16::MAX as u64) as u16,
                            cfg.user,
                        );
                    }
                }
                GattConnectionEvent::PassKeyDisplay(key) => {
                    emit_security_event(
                        cfg,
                        1,
                        trouble_host::prelude::SecurityLevel::NoEncryption,
                        key.value(),
                        false,
                    );
                }
                GattConnectionEvent::PassKeyConfirm(key) => {
                    emit_security_event(
                        cfg,
                        2,
                        trouble_host::prelude::SecurityLevel::NoEncryption,
                        key.value(),
                        false,
                    );
                }
                GattConnectionEvent::PassKeyInput => {
                    emit_security_event(
                        cfg,
                        3,
                        trouble_host::prelude::SecurityLevel::NoEncryption,
                        0,
                        false,
                    );
                }
                GattConnectionEvent::PairingComplete {
                    security_level,
                    bond,
                } => {
                    if let Some(b) = bond.as_ref() {
                        store_bond(cfg, &gatt.bond_slots, b);
                    }
                    emit_security_event(
                        cfg,
                        4,
                        security_level,
                        0,
                        bond.as_ref().is_some_and(|b| b.is_bonded),
                    );
                }
                GattConnectionEvent::PairingFailed(_) => {
                    emit_security_event(
                        cfg,
                        5,
                        trouble_host::prelude::SecurityLevel::NoEncryption,
                        0,
                        false,
                    );
                }
                GattConnectionEvent::BondLost => {
                    emit_security_event(
                        cfg,
                        6,
                        trouble_host::prelude::SecurityLevel::NoEncryption,
                        0,
                        false,
                    );
                }
                GattConnectionEvent::Encrypted {
                    security_level,
                    bond,
                } => {
                    if let Some(b) = bond.as_ref() {
                        store_bond(cfg, &gatt.bond_slots, b);
                    }
                    emit_security_event(
                        cfg,
                        7,
                        security_level,
                        0,
                        bond.as_ref().is_some_and(|b| b.is_bonded),
                    );
                }
                GattConnectionEvent::OobRequest => {
                    if let Some((local, peer)) = load_oob_data(cfg) {
                        let _ = gconn.provide_oob_data(local, peer);
                    } else {
                        log_str(cfg, "[security] OOB data requested\0");
                    }
                }
                GattConnectionEvent::Gatt { event } => match event {
                    GattEvent::Write(w) => {
                        let mut buf = [0u8; VALUE_LEN];
                        let mut n = 0usize;
                        let mut idx = usize::MAX;
                        let mut sub_idx = usize::MAX;
                        let mut desc_meta = None;
                        let mut notify_enabled = 0u8;
                        let mut indicate_enabled = 0u8;
                        let h = w.handle();
                        for (i, c) in gatt.chars.iter().enumerate() {
                            if c.handle == h {
                                idx = i;
                                break;
                            } else if c.cccd_handle == Some(h) {
                                sub_idx = i;
                                break;
                            }
                        }
                        if idx == usize::MAX && sub_idx == usize::MAX {
                            desc_meta = gatt.descriptors.iter().copied().find(|d| d.handle == h);
                        }
                        if idx != usize::MAX || sub_idx != usize::MAX || desc_meta.is_some() {
                            w.with_data(|_off, data| {
                                n = data.len().min(VALUE_LEN);
                                buf[..n].copy_from_slice(&data[..n]);
                                if sub_idx != usize::MAX && data.len() >= 2 {
                                    let cccd = u16::from_le_bytes([data[0], data[1]]);
                                    notify_enabled = u8::from(cccd & 0x0001 != 0);
                                    indicate_enabled = u8::from(cccd & 0x0002 != 0);
                                }
                            });
                        }
                        if let Ok(reply) = w.accept() {
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
                        } else if sub_idx != usize::MAX {
                            if let Some(cb) = cfg.callbacks.on_subscription {
                                cb(sub_idx as u16, notify_enabled, indicate_enabled, cfg.user);
                            }
                        } else if let Some(meta) = desc_meta {
                            if let Some(cb) = cfg.callbacks.on_descriptor_write {
                                cb(meta.handle, meta.chr, meta.desc, buf.as_ptr(), n, cfg.user);
                            }
                        }
                    }
                    GattEvent::Read(r) => {
                        let mut idx = usize::MAX;
                        let h = r.handle();
                        for (i, c) in gatt.chars.iter().enumerate() {
                            if c.handle == h {
                                idx = i;
                                break;
                            }
                        }
                        let reply = if idx != usize::MAX {
                            if let Some(cb) = cfg.callbacks.on_read_value {
                                let mut out = [0u8; VALUE_LEN];
                                let n = cb(idx as u16, out.as_mut_ptr(), out.len(), cfg.user)
                                    .min(VALUE_LEN);
                                r.accept_unprocessed(&out[..n])
                            } else {
                                r.accept()
                            }
                        } else if let Some(meta) =
                            gatt.descriptors.iter().copied().find(|d| d.handle == h)
                        {
                            if let Some(cb) = cfg.callbacks.on_descriptor_read_value {
                                let mut out = [0u8; VALUE_LEN];
                                let n = cb(
                                    meta.handle,
                                    meta.chr,
                                    meta.desc,
                                    out.as_mut_ptr(),
                                    out.len(),
                                    cfg.user,
                                )
                                .min(VALUE_LEN);
                                r.accept_unprocessed(&out[..n])
                            } else {
                                r.accept()
                            }
                        } else {
                            r.accept()
                        };
                        if let Ok(reply) = reply {
                            let _ = reply.send().await;
                        }
                    }
                    other => {
                        if let Ok(reply) = other.accept() {
                            let _ = reply.send().await;
                        }
                    }
                },
            }
        }
    };

    let tx = async {
        loop {
            if SEND_REQ.load(Ordering::Acquire) {
                let chr = SEND_CHR.load(Ordering::Acquire);
                let kind = SEND_KIND.load(Ordering::Acquire);
                let target = if chr == NUS_TX_CHR { gatt.nus_tx } else { chr };
                let len = SEND_LEN
                    .load(Ordering::Acquire)
                    .min(VALUE_LEN)
                    .min(SEND_BUF_CAP);
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
                    let props = gatt.props.get(target).copied().unwrap_or(0);
                    if kind == SEND_KIND_INDICATE {
                        if props & C_PROP_INDICATE != 0 {
                            let _ = c.indicate_raw(&gconn, &txbuf[..len], false).await;
                        }
                    } else if props & C_PROP_NOTIFY != 0 {
                        let _ = c.notify_raw(&gconn, &txbuf[..len], false).await;
                    } else if props & C_PROP_INDICATE != 0 {
                        let _ = c.indicate_raw(&gconn, &txbuf[..len], false).await;
                    }
                }
            }
            Timer::after(Duration::from_millis(10)).await;
        }
    };

    let att_mtu_report = async {
        Timer::after(Duration::from_millis(400)).await;
        emit_att_mtu(conn, cfg);
    };

    select3(
        events,
        join(tx, join(link_control(conn, stack, cfg), att_mtu_report)),
        wait_unload(),
    )
    .await;
}
