/*
 * runtime-ble central / GATT-client example.
 *
 * Scans briefly, connects to a peer (config.role = CENTRAL + runtime_ble_connect_addr),
 * discovers a vendor service, subscribes to its notify characteristic, writes to
 * its write characteristic, and prints the notification the peer sends back.
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
static volatile int scan_printed;

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
	(void)runtime_ble_read_phy();
	(void)runtime_ble_update_frame_space(0, 0, RUNTIME_BLE_PHY_MASK_1M,
					     RUNTIME_BLE_FRAME_SPACE_ACL_CP |
					     RUNTIME_BLE_FRAME_SPACE_ACL_PC);
	(void)runtime_ble_set_phy(RUNTIME_BLE_PHY_CODED);
	(void)runtime_ble_request_connection_rate(0, 0, 1, 1, 0, 0, 0);
}
static void on_disconnected(uint8_t r, void *u)
{
	ARG_UNUSED(u);
	connected = 0;
	printk("[app] disconnected (reason 0x%02x)\n", r);
}
static void on_scan_result_ext(const uint8_t *addr, uint8_t kind, int8_t rssi, const uint8_t *adv,
			       size_t adv_len, void *u)
{
	ARG_UNUSED(u);
	ARG_UNUSED(adv);
	if (scan_printed < 8) {
		printk("[scan] %s %02x:%02x:%02x:%02x:%02x:%02x rssi=%d adv_len=%u\n",
		       kind == RUNTIME_BLE_ADDR_PUBLIC ? "public" : "random",
		       addr[5], addr[4], addr[3], addr[2], addr[1], addr[0],
		       rssi, (unsigned int)adv_len);
		scan_printed++;
	}
}
static void on_scan_result_meta(const uint8_t *addr, uint8_t kind, int8_t rssi,
				const uint8_t *adv, size_t adv_len, uint16_t flags,
				uint8_t primary_phy, uint8_t secondary_phy,
				int8_t tx_power_dbm, uint8_t sid, void *u)
{
	ARG_UNUSED(u);
	ARG_UNUSED(adv);
	if (scan_printed < 8) {
		printk("[scan] %s %02x:%02x:%02x:%02x:%02x:%02x rssi=%d len=%u "
		       "flags=0x%04x phy=%u/%u tx=%d sid=0x%02x\n",
		       kind == RUNTIME_BLE_ADDR_PUBLIC ? "public" : "random",
		       addr[5], addr[4], addr[3], addr[2], addr[1], addr[0],
		       rssi, (unsigned int)adv_len, flags, primary_phy,
		       secondary_phy, tx_power_dbm, sid);
		scan_printed++;
	}
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
static void on_descriptor(uint16_t h, const uint8_t *uuid, uint8_t ul, void *u)
{
	ARG_UNUSED(u);
	printk("[app] descriptor handle=%u uuid_len=%u uuid0=0x%02x\n",
	       h, ul, ul > 0 ? uuid[0] : 0);
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
static void on_att_mtu(uint16_t att_mtu, void *u)
{
	ARG_UNUSED(u);
	printk("[app] ATT MTU %u\n", att_mtu);
}
static void on_frame_space(uint32_t frame_space_us, void *u)
{
	ARG_UNUSED(u);
	printk("[app] frame space %u us\n", frame_space_us);
}
static void on_connection_rate(uint16_t interval_ms, uint16_t subrate_factor, uint16_t latency,
			       uint16_t continuation_number, uint16_t timeout_ms, void *u)
{
	ARG_UNUSED(u);
	printk("[app] connection rate interval=%u subrate=%u latency=%u cont=%u timeout=%u\n",
	       interval_ms, subrate_factor, latency, continuation_number, timeout_ms);
}

int main(void)
{
	static const runtime_ble_config_t cfg = {
		.device_name = "RTBLE-CENTRAL",
		.role = RUNTIME_BLE_ROLE_CENTRAL,
		.central_conn_min_interval_ms = 50,
		.central_conn_max_interval_ms = 90,
		.central_conn_timeout_ms = 8000,
		.callbacks = {
			.on_connected = on_connected,
			.on_disconnected = on_disconnected,
			.on_scan_result_ext = on_scan_result_ext,
			.on_scan_result_meta = on_scan_result_meta,
			.on_discovered = on_discovered,
			.on_descriptor = on_descriptor,
			.on_notification = on_notification,
			.on_read = on_read,
			.on_att_mtu = on_att_mtu,
			.on_frame_space = on_frame_space,
			.on_connection_rate = on_connection_rate,
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

	printk("[app] active scan for 2 seconds with duplicate filtering...\n");
	runtime_ble_scan_start_ex(1, 100, 50, 2000,
				  RUNTIME_BLE_SCAN_OPT_FILTER_DUPLICATES |
				  RUNTIME_BLE_SCAN_OPT_PHY_1M |
				  RUNTIME_BLE_SCAN_OPT_PHY_CODED,
				  NULL, 0);
	k_sleep(K_MSEC(2300));
	runtime_ble_scan_stop();

	printk("[app] connecting to %02x:%02x:%02x:%02x:%02x:%02x\n",
	       peer[5], peer[4], peer[3], peer[2], peer[1], peer[0]);
	runtime_ble_connect_addr(peer, RUNTIME_BLE_ADDR_RANDOM);
	while (!connected) {
		k_sleep(K_MSEC(100));
	}
	k_sleep(K_MSEC(500));
	printk("[app] discovering vendor service...\n");
	runtime_ble_client_discover(svc_uuid, 16);
	k_sleep(K_MSEC(1000));

	if (discovered >= 2) {
		printk("[app] discovering TX descriptors near handle=%u\n", tx_handle);
		runtime_ble_client_discover_descriptors(tx_handle + 1, tx_handle + 4);
		k_sleep(K_MSEC(500));
		printk("[app] subscribe TX handle=%u\n", tx_handle);
		runtime_ble_client_subscribe(tx_handle);
		k_sleep(K_MSEC(500));
		const uint8_t msg[] = {'p', 'i', 'n', 'g'};

		printk("[app] write-no-rsp RX handle=%u 'ping'\n", rx_handle);
		runtime_ble_client_write_no_rsp(rx_handle, msg, sizeof(msg));
		k_sleep(K_MSEC(1500)); /* expect the echo back as a NOTIFY */
	} else {
		printk("[app] discovery incomplete (%d chars)\n", discovered);
	}
	printk("[app] central demo done\n");
	return 0;
}
