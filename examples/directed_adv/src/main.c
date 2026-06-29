/*
 * runtime-ble directed advertising example.
 *
 * Directed advertising is for reconnecting to a known central. Replace
 * peer_addr with the bonded/known central address in little-endian byte order.
 */
#include <zephyr/kernel.h>
#include <zephyr/sys/printk.h>
#include "runtime_ble.h"

/* c0:00:00:00:00:00 as little-endian bytes, static-random address type. */
static const uint8_t peer_addr[6] = {0x00, 0x00, 0x00, 0x00, 0x00, 0xc0};

static void on_log(const char *line, void *user)
{
	ARG_UNUSED(user);
	printk("%s\n", line);
}

static void on_connected(void *user)
{
	ARG_UNUSED(user);
	printk("[app] directed peer connected\n");
}

static void on_disconnected(uint8_t reason, void *user)
{
	ARG_UNUSED(user);
	printk("[app] directed peer disconnected (reason 0x%02x)\n", reason);
}

int main(void)
{
	static const runtime_ble_config_t cfg = {
		.device_name = "RTBLE-DIRECTED",
		.directed_peer_address = peer_addr,
		.directed_peer_address_kind = RUNTIME_BLE_ADDR_RANDOM,
		.directed_high_duty = 0,
		.adv_interval_min_ms = 100,
		.adv_interval_max_ms = 150,
		.callbacks = {
			.on_connected = on_connected,
			.on_disconnected = on_disconnected,
			.on_log = on_log,
		},
	};
	uint8_t addr[6];

	runtime_ble_addr(addr);
	printk("\n[app] runtime-ble directed advertising example\n");
	printk("[app] addr %02x:%02x:%02x:%02x:%02x:%02x\n",
	       addr[5], addr[4], addr[3], addr[2], addr[1], addr[0]);
	printk("[app] directed peer c0:00:00:00:00:00\n");

	runtime_ble_init(&cfg);
	if (runtime_ble_load() != RUNTIME_BLE_OK) {
		printk("[app] runtime_ble_load failed\n");
		return 0;
	}
	printk("[app] low-duty directed advertising started\n");
	return 0;
}

