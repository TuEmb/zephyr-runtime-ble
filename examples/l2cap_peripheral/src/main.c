/*
 * runtime-ble L2CAP echo peripheral.
 *
 * Advertises and listens for an L2CAP connection-oriented channel on
 * config.l2cap_psm; whatever an SDU it receives, it echoes back. Pair it with
 * the L2CAP central example.
 *
 * Requires a l2cap-capable build: CONFIG_RUNTIME_BLE_L2CAP=y.
 */
#include <zephyr/kernel.h>
#include <zephyr/sys/printk.h>
#include "runtime_ble.h"

#define L2CAP_PSM 0x0080

static void on_log(const char *l, void *u)
{
	ARG_UNUSED(u);
	printk("%s\n", l);
}
static void on_l2cap_connected(void *u)
{
	ARG_UNUSED(u);
	printk("[app] L2CAP channel opened\n");
}
static void on_l2cap_disconnected(void *u)
{
	ARG_UNUSED(u);
	printk("[app] L2CAP channel closed\n");
}
static void on_l2cap_data(const uint8_t *d, size_t n, void *u)
{
	ARG_UNUSED(u);
	printk("[app] L2CAP rx len=%u: ", (unsigned int)n);
	for (size_t i = 0; i < n; i++) {
		printk("%c", d[i]);
	}
	printk("  -> echo\n");
	(void)runtime_ble_l2cap_send(d, n); /* echo it back */
}

int main(void)
{
	static const runtime_ble_config_t cfg = {
		.device_name = "RTBLE-L2CAP",
		.manufacturer_id = 0xFFFF,
		.l2cap_psm = L2CAP_PSM,
		.l2cap_mtu = 128,
		.l2cap_mps = 64,
		.l2cap_initial_credits = 4,
		.l2cap_credit_policy = RUNTIME_BLE_L2CAP_CREDITS_EVERY,
		.l2cap_credit_policy_value = 2,
		.callbacks = {
			.on_l2cap_connected = on_l2cap_connected,
			.on_l2cap_disconnected = on_l2cap_disconnected,
			.on_l2cap_data = on_l2cap_data,
			.on_log = on_log,
		},
	};
	uint8_t addr[6];

	runtime_ble_addr(addr);
	printk("\n[app] L2CAP echo peripheral, PSM 0x%04x\n", L2CAP_PSM);
	printk("[app] addr %02x:%02x:%02x:%02x:%02x:%02x\n",
	       addr[5], addr[4], addr[3], addr[2], addr[1], addr[0]);
	runtime_ble_init(&cfg);
	if (runtime_ble_load() != RUNTIME_BLE_OK) {
		printk("[app] load failed\n");
		return 0;
	}
	printk("[app] advertising + listening for L2CAP on PSM 0x%04x\n", L2CAP_PSM);
	return 0;
}
