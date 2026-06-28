/*
 * runtime-ble DUAL role: GATT server AND GATT client at the same time.
 *
 * config.role = RUNTIME_BLE_ROLE_DUAL makes the device do both GAP roles on two
 * simultaneous links:
 *   - peripheral: advertises "RTBLE-DUAL" with a vendor service + echoes writes
 *     (so another central can connect to it and use it as a server);
 *   - central: connects to config.peer_address and acts as a GATT client
 *     (discovers / subscribes / writes), printing the echo it gets back.
 *
 * Requires a central-capable build: CONFIG_RUNTIME_BLE_CENTRAL=y (links
 * libruntime_ble_central.a, which raises CONNECTIONS_MAX to 2). Pair the client
 * side with the gatt_server example; set PEER_* to that board's address.
 */
#include <zephyr/kernel.h>
#include <zephyr/sys/printk.h>
#include "runtime_ble.h"

/* Our own GATT server: a vendor service others can connect to and use. */
static const uint8_t svc_uuid[16] = {0x9e, 0xca, 0xdc, 0x24, 0x0e, 0xe5, 0xa9, 0xe0,
				     0x93, 0xf3, 0xa3, 0xb5, 0x01, 0x00, 0x4c, 0xe5};
static const uint8_t rx_uuid[16]  = {0x9e, 0xca, 0xdc, 0x24, 0x0e, 0xe5, 0xa9, 0xe0,
				     0x93, 0xf3, 0xa3, 0xb5, 0x02, 0x00, 0x4c, 0xe5};
static const uint8_t tx_uuid[16]  = {0x9e, 0xca, 0xdc, 0x24, 0x0e, 0xe5, 0xa9, 0xe0,
				     0x93, 0xf3, 0xa3, 0xb5, 0x03, 0x00, 0x4c, 0xe5};
#define CHR_RX 0
#define CHR_TX 1
static const runtime_ble_char_def_t my_chars[] = {
	{ .uuid = rx_uuid, .uuid_len = 16,
	  .props = RUNTIME_BLE_PROP_WRITE | RUNTIME_BLE_PROP_WRITE_NR, .max_len = 244 },
	{ .uuid = tx_uuid, .uuid_len = 16, .props = RUNTIME_BLE_PROP_NOTIFY, .max_len = 244 },
};
static const runtime_ble_service_def_t my_services[] = {
	{ .uuid = svc_uuid, .uuid_len = 16, .chars = my_chars, .num_chars = 2 },
};

/* Peer to connect to as a client (LSB first). Override with its address. */
#ifndef PEER0
#define PEER0 0x00
#define PEER1 0x00
#define PEER2 0x00
#define PEER3 0x00
#define PEER4 0x00
#define PEER5 0xC0
#endif
static const uint8_t peer[6] = {PEER0, PEER1, PEER2, PEER3, PEER4, PEER5};

static volatile uint16_t c_rx, c_tx;
static volatile int c_disc, linked;

static void on_log(const char *l, void *u)
{
	ARG_UNUSED(u);
	printk("%s\n", l);
}
/* --- server side --- */
static void on_write(uint16_t chr, const uint8_t *d, size_t n, void *u)
{
	ARG_UNUSED(u);
	printk("[srv] write chr=%u len=%u -> echo\n", chr, (unsigned int)n);
	if (chr == CHR_RX) {
		(void)runtime_ble_notify(CHR_TX, d, n);
	}
}
/* --- shared / client side --- */
static void on_connected(void *u)
{
	ARG_UNUSED(u);
	linked = 1;
	printk("[app] a link is up\n");
}
static void on_disconnected(uint8_t r, void *u)
{
	ARG_UNUSED(u);
	printk("[app] a link is down (reason 0x%02x)\n", r);
}
static void on_discovered(uint16_t h, const uint8_t *uu, uint8_t ul, uint16_t p, void *u)
{
	ARG_UNUSED(u); ARG_UNUSED(uu); ARG_UNUSED(ul); ARG_UNUSED(p);
	if (c_disc == 0) {
		c_rx = h;
	} else if (c_disc == 1) {
		c_tx = h;
	}
	c_disc++;
	printk("[cli] discovered characteristic #%d handle=%u\n", c_disc, h);
}
static void on_notification(uint16_t h, const uint8_t *d, size_t n, void *u)
{
	ARG_UNUSED(u);
	printk("[cli] NOTIFY handle=%u len=%u: ", h, (unsigned int)n);
	for (size_t i = 0; i < n; i++) {
		printk("%c", d[i]);
	}
	printk("\n");
}

int main(void)
{
	static const runtime_ble_config_t cfg = {
		.device_name = "RTBLE-DUAL",
		.manufacturer_id = 0xFFFF,
		.role = RUNTIME_BLE_ROLE_DUAL,
		.peer_address = peer,
		.services = my_services,
		.num_services = 1,
		.callbacks = {
			.on_connected = on_connected,
			.on_disconnected = on_disconnected,
			.on_write = on_write,
			.on_discovered = on_discovered,
			.on_notification = on_notification,
			.on_log = on_log,
		},
	};
	uint8_t a[6];

	runtime_ble_addr(a);
	printk("\n[app] DUAL (server + client). my addr %02x:%02x:%02x:%02x:%02x:%02x\n",
	       a[5], a[4], a[3], a[2], a[1], a[0]);
	printk("[app] client -> peer %02x:%02x:%02x:%02x:%02x:%02x\n",
	       peer[5], peer[4], peer[3], peer[2], peer[1], peer[0]);
	runtime_ble_init(&cfg);
	if (runtime_ble_load() != RUNTIME_BLE_OK) {
		printk("[app] load failed\n");
		return 0;
	}
	printk("[app] advertising 'RTBLE-DUAL' (server) + connecting to peer (client)\n");

	/* Client flow: once a link is up, discover the peer's service and use it. */
	while (!linked) {
		k_sleep(K_MSEC(100));
	}
	k_sleep(K_MSEC(700));
	printk("[cli] discovering peer service...\n");
	runtime_ble_client_discover(svc_uuid, 16);
	k_sleep(K_MSEC(1200));
	if (c_disc >= 2) {
		runtime_ble_client_subscribe(c_tx);
		k_sleep(K_MSEC(400));
		const uint8_t msg[] = {'d', 'u', 'a', 'l', '-', 'p', 'i', 'n', 'g'};

		printk("[cli] write peer RX handle=%u 'dual-ping'\n", c_rx);
		runtime_ble_client_write(c_rx, msg, sizeof(msg));
		k_sleep(K_MSEC(1500));
	}
	printk("[app] dual demo: client done; still advertising as a server\n");
	return 0;
}
