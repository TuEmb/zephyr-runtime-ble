/*
 * runtime-ble central / GATT-client example.
 *
 * Connects to a peer (config.role = CENTRAL, config.peer_address), discovers a
 * vendor service, subscribes to its notify characteristic, writes to its write
 * characteristic, and prints the notification the peer sends back.
 *
 * Pair it with the peripheral echo example (examples/gatt_server): set PEER_*
 * below to that board's BLE address (printed in its boot log).
 *
 * Requires a central-capable build: CONFIG_RUNTIME_BLE_CENTRAL=y (links
 * libruntime_ble_central.a).
 */
#include <zephyr/kernel.h>
#include <zephyr/sys/printk.h>
#include "runtime_ble.h"

/* Vendor service to discover (matches the peripheral echo example). */
static const uint8_t svc_uuid[16] = {0x9e, 0xca, 0xdc, 0x24, 0x0e, 0xe5, 0xa9, 0xe0,
				     0x93, 0xf3, 0xa3, 0xb5, 0x01, 0x00, 0x4c, 0xe5};

/* Peer BLE address, LSB first. Override with the peripheral's address. */
#ifndef PEER0
#define PEER0 0x00
#define PEER1 0x00
#define PEER2 0x00
#define PEER3 0x00
#define PEER4 0x00
#define PEER5 0xC0
#endif
static const uint8_t peer[6] = {PEER0, PEER1, PEER2, PEER3, PEER4, PEER5};

static volatile uint16_t rx_handle, tx_handle;
static volatile int discovered;
static volatile int connected;

static void on_log(const char *l, void *u)
{
	ARG_UNUSED(u);
	printk("%s\n", l);
}
static void on_connected(void *u)
{
	ARG_UNUSED(u);
	connected = 1;
	printk("[app] connected\n");
}
static void on_disconnected(uint8_t r, void *u)
{
	ARG_UNUSED(u);
	connected = 0;
	printk("[app] disconnected (reason 0x%02x)\n", r);
}
static void on_discovered(uint16_t h, const uint8_t *uuid, uint8_t ul, uint16_t props, void *u)
{
	ARG_UNUSED(u);
	ARG_UNUSED(uuid);
	ARG_UNUSED(ul);
	ARG_UNUSED(props);
	if (discovered == 0) {
		rx_handle = h; /* first declared characteristic (RX, write) */
	} else if (discovered == 1) {
		tx_handle = h; /* second (TX, notify) */
	}
	discovered++;
	printk("[app] discovered characteristic #%d handle=%u\n", discovered, h);
}
static void on_notification(uint16_t h, const uint8_t *d, size_t n, void *u)
{
	ARG_UNUSED(u);
	printk("[app] NOTIFY handle=%u len=%u: ", h, (unsigned int)n);
	for (size_t i = 0; i < n; i++) {
		printk("%c", d[i]);
	}
	printk("\n");
}
static void on_read(uint16_t h, const uint8_t *d, size_t n, void *u)
{
	ARG_UNUSED(u);
	ARG_UNUSED(d);
	printk("[app] READ handle=%u len=%u\n", h, (unsigned int)n);
}

int main(void)
{
	static const runtime_ble_config_t cfg = {
		.device_name = "RTBLE-CENTRAL",
		.role = RUNTIME_BLE_ROLE_CENTRAL,
		.peer_address = peer,
		.callbacks = {
			.on_connected = on_connected,
			.on_disconnected = on_disconnected,
			.on_discovered = on_discovered,
			.on_notification = on_notification,
			.on_read = on_read,
			.on_log = on_log,
		},
	};

	printk("\n[app] runtime-ble central demo; connecting to "
	       "%02x:%02x:%02x:%02x:%02x:%02x\n",
	       peer[5], peer[4], peer[3], peer[2], peer[1], peer[0]);
	runtime_ble_init(&cfg);
	if (runtime_ble_load() != RUNTIME_BLE_OK) {
		printk("[app] load failed\n");
		return 0;
	}

	while (!connected) {
		k_sleep(K_MSEC(100));
	}
	k_sleep(K_MSEC(500));
	printk("[app] discovering vendor service...\n");
	runtime_ble_client_discover(svc_uuid, 16);
	k_sleep(K_MSEC(1000));

	if (discovered >= 2) {
		printk("[app] subscribe TX handle=%u\n", tx_handle);
		runtime_ble_client_subscribe(tx_handle);
		k_sleep(K_MSEC(500));
		const uint8_t msg[] = {'p', 'i', 'n', 'g'};

		printk("[app] write RX handle=%u 'ping'\n", rx_handle);
		runtime_ble_client_write(rx_handle, msg, sizeof(msg));
		k_sleep(K_MSEC(1500)); /* expect the echo back as a NOTIFY */
	} else {
		printk("[app] discovery incomplete (%d chars)\n", discovered);
	}
	printk("[app] central demo done\n");
	return 0;
}
