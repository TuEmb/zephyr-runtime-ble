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
	/* Optional text log line (NUL-terminated) for the app's console. */
	void (*on_log)(const char *line, void *user);
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

/* ---- Internal (glue <-> staticlib); do not call from the application ---- */
void runtime_ble_run(int mode);
void runtime_ble_signal_unload(void);

#ifdef __cplusplus
}
#endif

#endif /* RUNTIME_BLE_H_ */
