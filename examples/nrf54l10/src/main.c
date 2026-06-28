/*
 * runtime-ble echo example.
 *
 * Brings up the loadable BLE runtime (trouble + SoftDevice Controller, the
 * radio is owned by the prebuilt Rust staticlib — note CONFIG_BT=n) and echoes
 * whatever a central writes to the RX characteristic back on TX (notify).
 *
 * Test with the nRF Connect mobile app: scan for "RUNTIME-BLE", connect, find
 * the Nordic UART Service (6e400001-...), enable notifications on TX
 * (6e400003), then write bytes to RX (6e400002) — they come back on TX.
 */
#include <zephyr/kernel.h>
#include <zephyr/sys/printk.h>
#include "runtime_ble.h"

static void on_log(const char *line, void *user)
{
	ARG_UNUSED(user);
	printk("%s\n", line);
}

static void on_connected(void *user)
{
	ARG_UNUSED(user);
	printk("[app] central connected\n");
}

static void on_disconnected(uint8_t reason, void *user)
{
	ARG_UNUSED(user);
	printk("[app] central disconnected (reason 0x%02x)\n", reason);
}

static void on_data(const uint8_t *data, size_t len, void *user)
{
	ARG_UNUSED(user);
	printk("[app] rx %u bytes -> echo\n", (unsigned int)len);
	/* Echo back. Queued here on the BLE thread; the runtime flushes it as a
	 * TX notification while still handling this same GATT write. */
	(void)runtime_ble_send(data, len);
}

int main(void)
{
	/* Demo manufacturer-specific advertising payload (after the company id). */
	static const uint8_t mfg_data[] = {0x52, 0x42, 0x01};
	static const runtime_ble_config_t cfg = {
		.device_name = "RUNTIME-BLE",
		.manufacturer_id = 0xFFFF,
		.manufacturer_data = mfg_data,
		.manufacturer_data_len = sizeof(mfg_data),
		.adv_interval_min_ms = 30,
		.adv_interval_max_ms = 60,
		.discoverable = 0, /* general-discoverable */
		.address = NULL,   /* hwinfo-derived static-random address */
		.callbacks = {
			.on_connected = on_connected,
			.on_disconnected = on_disconnected,
			.on_data = on_data,
			.on_log = on_log,
		},
		.user = NULL,
	};

	uint8_t addr[6];

	runtime_ble_addr(addr);
	printk("\n[app] runtime-ble echo example\n");
	printk("[app] addr %02x:%02x:%02x:%02x:%02x:%02x\n",
	       addr[5], addr[4], addr[3], addr[2], addr[1], addr[0]);

	runtime_ble_init(&cfg);
	if (runtime_ble_load() != RUNTIME_BLE_OK) {
		printk("[app] runtime_ble_load failed\n");
		return 0;
	}
	printk("[app] loaded; advertising as \"RUNTIME-BLE\"\n");
	return 0;
}
