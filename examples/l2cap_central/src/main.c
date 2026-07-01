/*
 * runtime-ble L2CAP central example.
 *
 * Connects to a peer (config.role = CENTRAL, config.peer_address), opens an
 * L2CAP connection-oriented channel on config.l2cap_psm, sends a message, and
 * prints the echo. Pair it with the L2CAP echo peripheral; set PEER_* to that
 * board's address.
 *
 * Requires a central+l2cap build: CONFIG_RUNTIME_BLE_CENTRAL=y +
 * CONFIG_RUNTIME_BLE_L2CAP=y (links libruntime_ble_central_l2cap.a).
 */
#include <zephyr/kernel.h>
#include <zephyr/sys/printk.h>
#include <string.h>
#include "runtime_ble.h"

#define L2CAP_PSM 0x0080

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

static volatile int l2cap_up, echo_rx;

static void on_log(const char *l, void *u)
{
	ARG_UNUSED(u);
	printk("%s\n", l);
}
static void on_connected(void *u)
{
	ARG_UNUSED(u);
	printk("[app] connected\n");
}
static void on_disconnected(uint8_t r, void *u)
{
	ARG_UNUSED(u);
	printk("[app] disconnected (reason 0x%02x)\n", r);
}
static void on_l2cap_connected(void *u)
{
	ARG_UNUSED(u);
	l2cap_up = 1;
	printk("[app] L2CAP channel opened\n");
}
static void on_l2cap_disconnected(void *u)
{
	ARG_UNUSED(u);
	l2cap_up = 0;
	printk("[app] L2CAP channel closed\n");
}
static void on_l2cap_data(const uint8_t *d, size_t n, void *u)
{
	ARG_UNUSED(u);
	echo_rx = 1;
	printk("[app] L2CAP echo rx len=%u: ", (unsigned int)n);
	for (size_t i = 0; i < n; i++) {
		printk("%c", d[i]);
	}
	printk("\n");
}

int main(void)
{
	static const runtime_ble_config_t cfg = {
		.abi_version = RUNTIME_BLE_ABI_VERSION,
		.device_name = "RTBLE-L2C-CENT",
		.role = RUNTIME_BLE_ROLE_CENTRAL,
		.central = {
			.peer_address = peer,
			.peer_address_kind = RUNTIME_BLE_ADDR_RANDOM,
		},
		.l2cap = {
			.psm = L2CAP_PSM,
			.mtu = 128,
			.mps = 64,
			.initial_credits = 4,
			.credit_policy = RUNTIME_BLE_L2CAP_CREDITS_EVERY,
			.credit_policy_value = 2,
		},
		.callbacks = {
			.on_connected = on_connected,
			.on_disconnected = on_disconnected,
			.on_l2cap_connected = on_l2cap_connected,
			.on_l2cap_disconnected = on_l2cap_disconnected,
			.on_l2cap_data = on_l2cap_data,
			.on_log = on_log,
		},
	};

	printk("\n[app] L2CAP central; connecting to "
	       "%02x:%02x:%02x:%02x:%02x:%02x, PSM 0x%04x\n",
	       peer[5], peer[4], peer[3], peer[2], peer[1], peer[0], L2CAP_PSM);
	runtime_ble_init(&cfg);
	if (runtime_ble_load() != RUNTIME_BLE_OK) {
		printk("[app] load failed\n");
		return 0;
	}

	while (!l2cap_up) {
		k_sleep(K_MSEC(100));
	}
	k_sleep(K_MSEC(300));
	const char *msg = "L2CAP-ping";

	printk("[app] sending '%s' over L2CAP\n", msg);
	runtime_ble_l2cap_send((const uint8_t *)msg, strlen(msg));
	k_sleep(K_MSEC(1500)); /* expect the echo back */
	printk("[app] closing L2CAP channel\n");
	runtime_ble_l2cap_disconnect();
	k_sleep(K_MSEC(500));
	printk("[app] L2CAP central done (echo_received=%d)\n", echo_rx);
	return 0;
}
