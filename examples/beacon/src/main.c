/*
 * runtime-ble beacon example.
 *
 * Broadcasts legacy non-connectable advertising with a local name, service UUID,
 * Service Data, and manufacturer payload. Scan with a BLE scanner app; this
 * device will not accept connections.
 */
#include <zephyr/kernel.h>
#include <zephyr/sys/printk.h>
#include "runtime_ble.h"

/* Demo 128-bit service UUID, little-endian byte order:
 * e54c00b0-b5a3-f393-e0a9-e50e24dcca9e
 */
static const uint8_t beacon_svc_uuid[16] = {
	0x9e, 0xca, 0xdc, 0x24, 0x0e, 0xe5, 0xa9, 0xe0,
	0x93, 0xf3, 0xa3, 0xb5, 0xb0, 0x00, 0x4c, 0xe5
};

static const uint8_t mfg_data[] = {
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
		.manufacturer_id = 0xFFFF,
		.manufacturer_data = mfg_data,
		.manufacturer_data_len = sizeof(mfg_data),
		.adv_service_uuid = beacon_svc_uuid,
		.adv_service_uuid_len = sizeof(beacon_svc_uuid),
		.adv_service_data_uuid = svc_data_uuid,
		.adv_service_data_uuid_len = sizeof(svc_data_uuid),
		.adv_service_data = svc_data,
		.adv_service_data_len = sizeof(svc_data),
		.nonconnectable = 1,
		.adv_interval_min_ms = 100,
		.adv_interval_max_ms = 250,
		.discoverable = 0,
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
	printk("[app] broadcasting non-connectable beacon \"RTBLE-BEACON\"\n");
	return 0;
}
