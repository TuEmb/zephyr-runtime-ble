/*
 * runtime_ble.h — C ABI for the trouble-based loadable BLE runtime.
 *
 * Ownership: this library OWNS the radio (nrf-sdc + MPSL). The application MUST
 * build with CONFIG_BT=n and CONFIG_MPSL=n — only one stack can own the radio
 * (enforced by `depends on !BT` in Kconfig). Runtime state is allocated from the
 * Zephyr heap on runtime_ble_load() and freed on runtime_ble_unload(), so it
 * costs ~no RAM until loaded.
 *
 * The GATT layout is user-defined (config.services); if omitted it falls back to
 * a built-in Nordic-UART-style peripheral:
 *   - RX (6e400002): peer -> device, write / write-without-response
 *   - TX (6e400003): device -> peer, notify
 *
 * Typical use:
 *     static runtime_ble_config_t cfg = {
 *         .device_name = "RUNTIME-BLE",
 *         .manufacturer_id = 0xFFFF,
 *         .callbacks = { .on_data = on_data, .on_connected = on_conn, ... },
 *     };
 *     runtime_ble_init(&cfg);   // configure (no radio yet)
 *     runtime_ble_load();       // bring radio up, advertise
 *     ...
 *     runtime_ble_send(buf, len);
 *     ...
 *     runtime_ble_unload();     // tear down, free all session RAM
 *
 * Callbacks run on the BLE runtime thread — keep them short.
 */
#ifndef RUNTIME_BLE_H_
#define RUNTIME_BLE_H_

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Result codes. */
#define RUNTIME_BLE_OK           0
#define RUNTIME_BLE_ERR_INVALID -1
#define RUNTIME_BLE_ERR_NO_MEM  -2

/* ---- Role (config.role) ---- */
#define RUNTIME_BLE_ROLE_PERIPHERAL 0   /* advertise + GATT server (default)  */
#define RUNTIME_BLE_ROLE_CENTRAL    1   /* scan/connect + GATT client         */
#define RUNTIME_BLE_ROLE_DUAL       2   /* both at once: server + client      */

/* ---- Optional user-defined GATT ----
 * Characteristic property bitmask. */
#define RUNTIME_BLE_PROP_READ        (1u << 0)
#define RUNTIME_BLE_PROP_WRITE       (1u << 1)  /* write with response    */
#define RUNTIME_BLE_PROP_WRITE_NR    (1u << 2)  /* write without response */
#define RUNTIME_BLE_PROP_NOTIFY      (1u << 3)
#define RUNTIME_BLE_PROP_INDICATE    (1u << 4)

/* One characteristic. `uuid` is little-endian, 2 bytes (16-bit) or 16 (128-bit). */
typedef struct {
	const uint8_t *uuid;
	uint8_t        uuid_len;   /* 2 or 16 */
	uint16_t       props;      /* RUNTIME_BLE_PROP_* bitmask */
	uint16_t       max_len;    /* value buffer size in bytes */
} runtime_ble_char_def_t;

/* One service and its characteristics. */
typedef struct {
	const uint8_t                *uuid;
	uint8_t                       uuid_len;   /* 2 or 16 */
	const runtime_ble_char_def_t *chars;
	uint8_t                       num_chars;
} runtime_ble_service_def_t;

/* Application callbacks. Invoked from the BLE runtime thread — keep them short. */
typedef struct {
	void (*on_connected)(void *user);
	void (*on_disconnected)(uint8_t reason, void *user);
	/* Bytes written by the peer to the built-in NUS RX characteristic. */
	void (*on_data)(const uint8_t *data, size_t len, void *user);
	/* Peer wrote to a user-defined characteristic. `chr` is the flat 0-based
	 * index in declaration order across config.services[].*.chars[]. */
	void (*on_write)(uint16_t chr, const uint8_t *data, size_t len, void *user);

	/* ---- Central / GATT client (lib built with the central role) ---- */
	/* A scan advertising report (addr is 6 bytes, LSB first). */
	void (*on_scan_result)(const uint8_t *addr, int8_t rssi,
			       const uint8_t *adv, size_t adv_len, void *user);
	/* A characteristic found by runtime_ble_client_discover() (uuid is LE). */
	void (*on_discovered)(uint16_t handle, const uint8_t *uuid, uint8_t uuid_len,
			      uint16_t props, void *user);
	/* Value returned by runtime_ble_client_read(). */
	void (*on_read)(uint16_t handle, const uint8_t *data, size_t len, void *user);
	/* A notification/indication from a subscribed characteristic. */
	void (*on_notification)(uint16_t handle, const uint8_t *data, size_t len, void *user);

	/* Optional text log line (NUL-terminated) for the app's console. */
	void (*on_log)(const char *line, void *user);

	/* ---- L2CAP CoC (lib built with the l2cap feature) ---- */
	/* The L2CAP channel (config.l2cap_psm) was established. */
	void (*on_l2cap_connected)(void *user);
	/* An SDU was received on the L2CAP channel. */
	void (*on_l2cap_data)(const uint8_t *data, size_t len, void *user);
	/* The L2CAP channel closed. */
	void (*on_l2cap_disconnected)(void *user);
} runtime_ble_callbacks_t;

/*
 * Advertising / GAP configuration. Every field is optional — a zeroed struct
 * gives sensible defaults (connectable, general-discoverable, 30-60 ms, name
 * "RUNTIME-BLE", random-static address derived from hwinfo). Pointed-to data
 * (device_name, manufacturer_data, address) must outlive the BLE session — use
 * static storage.
 *
 * The GATT layout is user-defined: set `services`/`num_services` to build your
 * own services + characteristics at load time (no Rust rebuild needed). If left
 * NULL/0 a built-in Nordic UART Service is used (RX 6e400002 write, TX 6e400003
 * notify — see `on_data` + `runtime_ble_send`).
 */
typedef struct {
	const char             *device_name;          /* adv name; NULL -> "RUNTIME-BLE"      */
	uint16_t                manufacturer_id;      /* company ID for the manufacturer AD   */
	const uint8_t          *manufacturer_data;    /* bytes after company ID; NULL -> none */
	uint16_t                manufacturer_data_len;
	uint16_t                adv_interval_min_ms;  /* 0 -> 30 ms                            */
	uint16_t                adv_interval_max_ms;  /* 0 -> 60 ms                            */
	uint8_t                 discoverable;         /* 0 general (default), 1 limited, 2 none */
	const uint8_t          *address;              /* optional 6-byte static-random addr;   */
	                                              /* NULL -> hwinfo-derived                */
	/* User-defined GATT. NULL/0 -> built-in NUS. Otherwise built at load time;
	 * use on_write + runtime_ble_notify() with the flat characteristic index.
	 * The array + its uuid buffers must outlive the BLE session (static). */
	const runtime_ble_service_def_t *services;
	uint8_t                          num_services;

	/* ---- Role ---- */
	uint8_t                 role;                 /* RUNTIME_BLE_ROLE_* (0 peripheral, default) */
	/* Central only: optional 6-byte peer (LSB first) to auto-connect on load;
	 * NULL -> none (use runtime_ble_scan_start + runtime_ble_connect). */
	const uint8_t          *peer_address;
	/* L2CAP connection-oriented channel PSM (0 = disabled). Once connected, a
	 * peripheral listens on it and a central opens it. Needs a l2cap-capable
	 * lib (CONFIG_RUNTIME_BLE_L2CAP=y). */
	uint16_t                l2cap_psm;

	runtime_ble_callbacks_t callbacks;
	void                   *user;                 /* opaque, passed back to callbacks      */
} runtime_ble_config_t;

/* ---- Public API ---- */

/* Configure the library (copies cfg). Does NOT touch the radio. Call once,
 * before runtime_ble_load(). Returns RUNTIME_BLE_OK. */
int runtime_ble_init(const runtime_ble_config_t *cfg);

/* Bring BLE up: allocate a thread stack + session state from the heap and start
 * advertising. Idempotent. */
int runtime_ble_load(void);

/* Tear BLE down: signal teardown, join the thread, free its stack and session
 * state — all BLE RAM returns to the Zephyr heap. Idempotent. */
int runtime_ble_unload(void);

/* Queue one notification (up to ~244 bytes) to send to the connected central.
 * For the built-in NUS this notifies the TX characteristic. */
int runtime_ble_send(const uint8_t *data, size_t len);

/* Notify/indicate a user-defined characteristic `chr` (flat 0-based index in
 * declaration order across config.services). The characteristic must have
 * RUNTIME_BLE_PROP_NOTIFY or _INDICATE. Returns RUNTIME_BLE_OK or an error. */
int runtime_ble_notify(uint16_t chr, const uint8_t *data, size_t len);

/* The per-device BLE address as 6 bytes, out[0]=LSB. Stable across re-flashes
 * (derived from hwinfo); usable e.g. to build a per-device advertising name. */
void runtime_ble_addr(uint8_t out[6]);

/* ---- Central / GATT client API ----
 * Available when the application sets config.role = RUNTIME_BLE_ROLE_CENTRAL and
 * links a central-capable staticlib (CONFIG_RUNTIME_BLE_CENTRAL=y; otherwise
 * these return RUNTIME_BLE_ERR_INVALID). Calls are queued to the runtime thread;
 * results arrive via the callbacks above. One central link at a time. */

/* Start/stop scanning. Each advertising report is delivered to on_scan_result. */
int runtime_ble_scan_start(void);
int runtime_ble_scan_stop(void);

/* Connect to a peer by address (6 bytes, LSB first). on_connected fires on success. */
int runtime_ble_connect(const uint8_t addr[6]);

/* Disconnect the current central link. */
int runtime_ble_disconnect(void);

/* Discover the characteristics of a service (16- or 128-bit UUID, LE bytes).
 * Each characteristic found is reported via on_discovered. */
int runtime_ble_client_discover(const uint8_t *svc_uuid, uint8_t uuid_len);

/* Read a characteristic by attribute handle; the value arrives via on_read. */
int runtime_ble_client_read(uint16_t handle);

/* Write a characteristic by attribute handle (with response). */
int runtime_ble_client_write(uint16_t handle, const uint8_t *data, size_t len);

/* Subscribe to a characteristic (enable notify/indicate); incoming values arrive
 * via on_notification. */
int runtime_ble_client_subscribe(uint16_t handle);

/* ---- L2CAP API ----
 * Available with a l2cap-capable lib (CONFIG_RUNTIME_BLE_L2CAP=y) and
 * config.l2cap_psm != 0; otherwise returns RUNTIME_BLE_ERR_INVALID. The channel
 * opens automatically once connected (on_l2cap_connected); received SDUs arrive
 * via on_l2cap_data. */

/* Queue one SDU to send on the open L2CAP channel (<= the negotiated MTU). */
int runtime_ble_l2cap_send(const uint8_t *data, size_t len);

/* ---- Internal (glue <-> staticlib); do not call from the application ---- */
void runtime_ble_run(int mode);
void runtime_ble_signal_unload(void);

#ifdef __cplusplus
}
#endif

#endif /* RUNTIME_BLE_H_ */
