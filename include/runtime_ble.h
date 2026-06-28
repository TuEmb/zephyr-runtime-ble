/*
 * runtime_ble.h — C ABI for the trouble-based loadable BLE runtime.
 *
 * Ownership: this library OWNS the radio (nrf-sdc + MPSL). The application MUST
 * build with CONFIG_BT=n and CONFIG_MPSL=n — only one stack can own the radio
 * (enforced by `depends on !BT` in Kconfig). Runtime state is allocated from the
 * Zephyr heap on runtime_ble_load() and freed on runtime_ble_unload(), so it
 * costs ~no RAM until loaded.
 *
 * It exposes a generic Nordic-UART-style GATT peripheral:
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

/* Application callbacks. Invoked from the BLE runtime thread — keep them short. */
typedef struct {
	void (*on_connected)(void *user);
	void (*on_disconnected)(uint8_t reason, void *user);
	/* Bytes written by the peer to the RX characteristic. */
	void (*on_data)(const uint8_t *data, size_t len, void *user);
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
 * Note: the GATT layout (services/characteristics) is defined in the Rust crate
 * (rust/src/radio.rs, the `#[gatt_service]` block) — to add/replace services,
 * edit that one place and rebuild the staticlib (scripts/build_lib.ps1). This is
 * a constraint of TrouBLE's compile-time GATT model; the knobs below cover the
 * runtime-configurable GAP/advertising surface.
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

/* Queue one notification (up to ~244 bytes) to send to the connected central. */
int runtime_ble_send(const uint8_t *data, size_t len);

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
