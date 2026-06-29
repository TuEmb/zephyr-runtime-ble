/*
 * runtime-ble beacon example.
 *
 * Broadcasts legacy non-connectable advertising with a local name, Service Data,
 * Appearance, TX Power Level, and manufacturer payload in scan response. Scan
 * with a BLE scanner app; this device will not accept connections.
 */
#include <zephyr/kernel.h>
#include <zephyr/sys/printk.h>
#include "runtime_ble.h"

static const uint8_t mfg_data[] = {
	0x08, 0xff,       /* 7 bytes of manufacturer-specific data follow */
	0xff, 0xff,       /* demo company ID, little-endian */
	0x52, 0x42, 0x02, /* "RB", beacon demo version */
	0x00, 0x00        /* app-defined payload bytes */
};
static const uint8_t svc_data_uuid[2] = { 0xF0, 0xFE };
static const uint8_t svc_data[] = {
	0x01, 0x64, 0x2a /* demo frame type, battery %, app sample */
};

static void on_log(const char *line, void *user)
{
	ARG_UNUSED(user);
	printk("%s\n", line);
}

int main(void)
{
	static const runtime_ble_config_t cfg = {
		.device_name = "RTBLE-BEACON",
		.adv_service_data_uuid = svc_data_uuid,
		.adv_service_data_uuid_len = sizeof(svc_data_uuid),
		.adv_service_data = svc_data,
		.adv_service_data_len = sizeof(svc_data),
		.appearance = 0x0540, /* Generic sensor */
		.adv_appearance = 1,
		.adv_tx_power_dbm = 0,
		.adv_tx_power_present = 1,
		.scan_response_data = mfg_data,
		.scan_response_data_len = sizeof(mfg_data),
		.nonconnectable = 1,
		.adv_interval_min_ms = 100,
		.adv_interval_max_ms = 250,
		.adv_channel_map = RUNTIME_BLE_ADV_CH_ALL,
		.discoverable = 1, /* limited-discoverable beacon */
		.callbacks = {
			.on_log = on_log,
		},
	};

	uint8_t addr[6];

	runtime_ble_addr(addr);
	printk("\n[app] runtime-ble beacon example\n");
	printk("[app] addr %02x:%02x:%02x:%02x:%02x:%02x\n",
	       addr[5], addr[4], addr[3], addr[2], addr[1], addr[0]);

	runtime_ble_init(&cfg);
	if (runtime_ble_load() != RUNTIME_BLE_OK) {
		printk("[app] runtime_ble_load failed\n");
		return 0;
	}
	printk("[app] broadcasting limited-discoverable beacon \"RTBLE-BEACON\"\n");
	return 0;
}
