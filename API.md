# runtime-ble API reference

The full C API. For a 5-minute introduction see the [Quick Start in the
README](README.md#quick-start); for build instructions see
[`README.md`](README.md) and [`rust/README.md`](rust/README.md).

Everything is configured from C through `runtime_ble_config_t` — the GATT layout
and the advertising/GAP parameters are all set there, with **no Rust rebuild**
needed to define your own services. Callbacks run on the BLE thread, so keep
them short.

## Contents

- [Peripheral: user-defined GATT](#peripheral-user-defined-gatt)
- [Advertising / GAP](#advertising--gap)
- [Characteristics, descriptors, callbacks](#characteristics-descriptors-callbacks)
- [Notify vs. indicate](#notify-vs-indicate)
- [Live-link updates](#live-link-updates-phy-data-length-conn-params)
- [Security, bonding, OOB](#security-bonding-oob)
- [Central / GATT client](#central--gatt-client)
- [Dual role](#dual-role)
- [L2CAP CoC](#l2cap-coc)

## Peripheral: user-defined GATT

```c
/* Declare your GATT (or leave services NULL for a built-in NUS). */
static const uint8_t user_desc_uuid[2] = { 0x01, 0x29 };
static const uint8_t tx_desc[] = "Echo notifications";
static const runtime_ble_desc_def_t tx_descs[] = {
    { .uuid = user_desc_uuid, .uuid_len = sizeof(user_desc_uuid),
      .value = tx_desc, .value_len = sizeof(tx_desc) - 1 },
};
static const runtime_ble_char_def_t chrs[] = {
    { .uuid = rx_uuid, .uuid_len = 16,
      .props = RUNTIME_BLE_PROP_WRITE | RUNTIME_BLE_PROP_WRITE_NR, .max_len = 244 },
    { .uuid = tx_uuid, .uuid_len = 16,
      .props = RUNTIME_BLE_PROP_NOTIFY, .max_len = 244,
      .permissions = RUNTIME_BLE_PERM_CCCD_ENCRYPT,
      .descriptors = tx_descs, .num_descriptors = 1 },
};
static const runtime_ble_service_def_t svcs[] = {
    { .uuid = svc_uuid, .uuid_len = 16, .chars = chrs, .num_chars = 2 },
};
static const uint8_t svc_data_uuid[2] = { 0xF0, 0xFE }, svc_data[] = { 0x01, 0x64 };
static const runtime_ble_config_t cfg = {
    .device_name = "RUNTIME-BLE", .manufacturer_id = 0xFFFF,
    .adv_service_uuid = svc_uuid, .adv_service_uuid_len = 16,
    .adv_service_data_uuid = svc_data_uuid, .adv_service_data_uuid_len = 2,
    .adv_service_data = svc_data, .adv_service_data_len = sizeof(svc_data),
    .appearance = 0x0540, .adv_appearance = 1,          /* Generic sensor */
    .adv_tx_power_dbm = 0, .adv_tx_power_present = 1,   /* AD type 0x0a + controller hint */
    /* .nonconnectable = 1, for beacon/broadcast-only advertising */
    /* .directed_peer_address = peer, for directed reconnect advertising */
    .adv_interval_min_ms = 30, .adv_interval_max_ms = 60,
    .adv_channel_map = RUNTIME_BLE_ADV_CH_ALL,           /* 0 also means all */
    /* .adv_filter_policy = RUNTIME_BLE_ADV_FILTER_CONN, .adv_accept_address = peer, */
    .discoverable = 0,                                   /* 0 general, 1 limited, 2 none */
    .services = svcs, .num_services = 1,
    .callbacks = { .on_write = on_write, .on_connected = on_conn, ... },
};
runtime_ble_init(&cfg);
runtime_ble_load();              // bring radio up + advertise
runtime_ble_notify(1, buf, n);   // notify characteristic #1 (TX)
runtime_ble_indicate(1, buf, n); // force an ATT indication when supported
runtime_ble_read_rssi();         // -> on_rssi(rssi)
runtime_ble_set_phy(RUNTIME_BLE_PHY_2M);       // or RUNTIME_BLE_PHY_CODED
runtime_ble_update_data_length(251, 2120);
runtime_ble_update_conn_params(30, 60, 0, 4000);
runtime_ble_read_att_mtu();      // -> on_att_mtu(att_mtu)
runtime_ble_read_phy();          // -> on_phy_update(tx, rx)
runtime_ble_read_security();     // -> on_security_state(level, key_len, flags)
runtime_ble_request_security();  // pairing/encryption; events -> on_security_event
runtime_ble_unload();            // tear down, free session RAM
```

## Advertising / GAP

For fully custom beacons, set `adv_data`/`adv_data_len` to raw AD structures
(up to 31 bytes); when present it bypasses the automatic advertising builder.
For fast reconnect to a known central, set `directed_peer_address`,
`directed_peer_address_kind`, and optionally `directed_high_duty`; directed
legacy advertising is connectable, non-scannable, and carries no AD payload.
Set `adv_channel_map` with `RUNTIME_BLE_ADV_CH_37/38/39` bits to restrict the
legacy advertising channels; zero uses all three channels.

## Characteristics, descriptors, callbacks

Characteristics are addressed by **flat index** (declaration order). Callbacks:
`on_connected`, `on_disconnected`, `on_write(chr, …)`, `on_read_value(chr, …)`
(or `on_descriptor_write(handle, chr, desc, …)` /
`on_descriptor_read_value(handle, chr, desc, …)` for user-defined descriptors)
(or `on_write_ext` / `on_descriptor_write_ext` when the app needs ATT offsets)
(or `on_data` for the built-in NUS RX), `on_subscription(chr, notify, indicate)`
when a peer writes a CCCD, `on_conn_params`, `on_phy_update`,
`on_data_length_update`, `on_att_mtu`, `on_frame_space`, `on_connection_rate`,
`on_rssi`, `on_security_event`, `on_security_state`, `on_bond_load`,
`on_oob_request`, `on_oob_local_data`, `on_log`. They run on the BLE thread —
keep them short.

Characteristics may include static descriptors, such as User Description
(`0x2901`); descriptor UUID/value buffers must remain valid for the loaded
session. Descriptors are read-only by default; set
`RUNTIME_BLE_PERM_WRITE_ALLOWED` or a `RUNTIME_BLE_PERM_WRITE_*` security bit in
the descriptor permissions to make the runtime keep a writable descriptor value.
Descriptor callbacks receive the ATT handle plus the owning characteristic index
and per-characteristic descriptor index.

## Notify vs. indicate

Use `runtime_ble_indicate()` when a characteristic advertises
`RUNTIME_BLE_PROP_INDICATE` and the app needs ATT confirmation semantics. The
older `runtime_ble_notify()` keeps its auto behavior: notify if available,
otherwise indicate.

## Live-link updates (PHY, data length, conn params)

On an active link, applications can request PHY, data length, classic
connection-parameter, frame-spacing, and connection-rate/subrate updates with
`runtime_ble_set_phy()`, `runtime_ble_update_data_length()`,
`runtime_ble_update_conn_params()`, `runtime_ble_update_frame_space()`, and
`runtime_ble_request_connection_rate()`. Results arrive through
`on_phy_update`, `on_data_length_update`, `on_conn_params`,
`on_frame_space`, and `on_connection_rate` when the controller/peer reports
them.

## Security, bonding, OOB

Call `runtime_ble_read_security()` to snapshot the active link's current
security level, encryption key length (0 when unavailable), and bonded-peer
flag through `on_security_state`.

Set `runtime_ble_char_def_t.permissions` with `RUNTIME_BLE_PERM_READ_*`,
`RUNTIME_BLE_PERM_WRITE_*`, or `RUNTIME_BLE_PERM_CCCD_*` to require encrypted or
authenticated links for individual ATT operations.

For persistent bonding, set `security_bondable = 1` and implement
`on_bond_load(index, out, max, user)` / `on_bond_store(index, blob, len, user)`.
Store the opaque `RUNTIME_BLE_BOND_BLOB_MAX` bytes in flash/settings as-is; the
runtime restores them into the BLE stack on the next `runtime_ble_load()`.
Use `security_io_capability` or `runtime_ble_set_io_capability()` with
`RUNTIME_BLE_IO_CAP_*` when pairing should use display, keyboard, or numeric
comparison instead of the default no-input/no-output capability. Use
`runtime_ble_bond_enumerate()` to receive restored/runtime bonds via `on_bond`,
and `runtime_ble_bond_delete()` / `runtime_ble_bond_delete_all()` to remove
bonds; deletion calls `on_bond_store(index, NULL, 0, user)` so app storage can
clear the matching slot.

For OOB pairing, set `security_oob_available = 1`. When the Security Manager
loads it calls `on_oob_local_data(local_random, local_confirm, user)` with the
16-byte local values to send through your out-of-band channel. When pairing needs
both sides' data it emits `RUNTIME_BLE_SECURITY_OOB_REQUEST` and calls
`on_oob_request(local_random, local_confirm, peer_random, peer_confirm, user)`.
Fill each 16-byte buffer and return non-zero to continue; for legacy OOB put the
TK in `*_random` and zero `*_confirm`.

## Central / GATT client

By default the runtime is a **peripheral** (advertise + GATT server, above). It
can also be a **central / GATT client** — build the central-capable lib
(`CONFIG_RUNTIME_BLE_CENTRAL=y`, links `libruntime_ble_central.a`) and set
`config.role = RUNTIME_BLE_ROLE_CENTRAL`:

```c
runtime_ble_scan_start(1, 100, 50, 0);  // active scan; results -> on_scan_result
runtime_ble_scan_start_ex(1, 100, 50, 0,
                          RUNTIME_BLE_SCAN_OPT_FILTER_DUPLICATES |
                          RUNTIME_BLE_SCAN_OPT_PHY_1M |
                          RUNTIME_BLE_SCAN_OPT_PHY_CODED,
                          NULL, 0);    // scan with controller duplicate filtering
runtime_ble_scan_stop();
runtime_ble_connect_addr(addr, RUNTIME_BLE_ADDR_RANDOM);
                                        // or config.peer_address to auto-connect
runtime_ble_client_discover_services(); // -> on_service(start, end, uuid, …)
runtime_ble_client_discover_all();      // -> on_service(...), on_discovered(...)
runtime_ble_client_discover(svc, 16);   // -> on_discovered(handle, …)
runtime_ble_client_discover_descriptors(start, end);
                                        // -> on_descriptor(handle, uuid, …)
runtime_ble_client_subscribe(handle);   // -> on_notification(handle, …)
runtime_ble_client_subscribe_indicate(handle);
                                        // subscribe with CCCD indications
runtime_ble_client_unsubscribe(handle); // disable CCCD notifications/indications
runtime_ble_client_write(handle, buf, n);
runtime_ble_client_write_no_rsp(handle, buf, n);
runtime_ble_client_read(handle);        // -> on_read(handle, …)
runtime_ble_client_read_blob(handle, off);
runtime_ble_client_read_by_uuid(start, end, uuid, uuid_len);
                                        // -> on_read(handle, …) for matches
runtime_ble_client_read_descriptor(desc_handle);
runtime_ble_client_write_descriptor(desc_handle, buf, n);
                                        // long reads from an ATT offset
```

Set `central_conn_min_interval_ms`, `central_conn_max_interval_ms`,
`central_conn_latency`, and `central_conn_timeout_ms` to tune the initial LE
connection parameters used by the central create-connection procedure; zero
values keep the runtime defaults.
Use `on_scan_result_ext` when the central needs the peer's address type; pass
that value to `runtime_ble_connect_addr()` or `config.peer_address_kind`.
Use `on_scan_result_meta` when the scanner also needs report metadata such as
connectable/scannable, scan-response, legacy/extended, PHY, TX power, and SID.
Use `runtime_ble_scan_start_ex()` to enable controller duplicate filtering,
select scan PHYs (1M, 2M, coded), or limit scan reports to one peer address
with the controller accept list.
Use `runtime_ble_client_discover_services()` to enumerate primary services, or
`runtime_ble_client_discover_all()` when the central does not know the target
service UUID up front; services arrive through `on_service`, and discovered
characteristics still use `on_discovered`.
The central client tracks up to 8 primary services and 32 discovered
characteristics for follow-up descriptor discovery and CCCD subscription.
Use `runtime_ble_client_subscribe_indicate()` for peers that expose indications
instead of notifications; both deliver incoming values via `on_notification`.
Incoming client indications are confirmed automatically after the callback runs.
Use `runtime_ble_client_unsubscribe()` to write CCCD=0x0000 for a discovered
characteristic and stop notifications or indications from that peer.
Use `runtime_ble_client_read_blob()` to continue reading long attributes from a
specific ATT offset; returned bytes still arrive through `on_read`.
Use `runtime_ble_client_read_by_uuid()` to issue ATT Read By Type over a handle
range when the central knows a characteristic UUID but not its value handle.
Use the descriptor read/write helpers when `on_descriptor` has reported a
descriptor handle; descriptor reads also return through `on_read`.
Use `on_client_status` to observe completion/failure of GATT client commands;
`op` is `RUNTIME_BLE_CLIENT_OP_*` and `status` is `RUNTIME_BLE_CLIENT_STATUS_*`.
See [`examples/gatt_client/`](examples/gatt_client/) (HW-verified
against the peripheral echo example). The role is feature-gated so peripheral-
only apps stay on the lean default lib (see [`rust/README.md`](rust/README.md)).

## Dual role

`config.role = RUNTIME_BLE_ROLE_DUAL` makes a central-capable build act as a
**GATT server *and* client simultaneously** (two links): it advertises + serves
incoming centrals while also connecting to `peer_address` as a client. See
[`examples/dual/`](examples/dual/) (HW-verified: advertises `RTBLE-DUAL` while
connected as a client).

## L2CAP CoC

Build with `CONFIG_RUNTIME_BLE_L2CAP=y` and set `config.l2cap_psm`. Once
`on_l2cap_connected` fires, send SDUs with `runtime_ble_l2cap_send()` and close
the channel with `runtime_ble_l2cap_disconnect()`; received SDUs arrive through
`on_l2cap_data`. Optional CoC tuning is available through `config.l2cap_mtu`,
`l2cap_mps`, `l2cap_initial_credits`, `l2cap_credit_policy`, and
`l2cap_credit_policy_value` (zero keeps the runtime defaults).
