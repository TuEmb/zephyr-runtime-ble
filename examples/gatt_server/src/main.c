/*
 * runtime-ble example: a fully user-defined BLE peripheral.
 *
 * Everything is configured from this app — no Rust rebuild:
 *   - advertising: raw ADV payload, scan response name,
 *     interval, discoverable mode
 *   - GATT: a custom 128-bit vendor service with an RX (write) characteristic
 *     and a TX (notify) characteristic.
 *
 * Behaviour: whatever a central writes to RX is echoed back as a notification
 * on TX. Test with the nRF Connect mobile app.
 */
#include <zephyr/kernel.h>
#include <zephyr/sys/printk.h>
#include <string.h>
#include "runtime_ble.h"

/* Custom 128-bit UUIDs, little-endian byte order.
 *   service e54c0001-b5a3-f393-e0a9-e50e24dcca9e
 *   rx      e54c0002-...   (write)
 *   tx      e54c0003-...   (notify)
 */
static const uint8_t svc_uuid[16] = {0x9e, 0xca, 0xdc, 0x24, 0x0e, 0xe5, 0xa9, 0xe0,
				     0x93, 0xf3, 0xa3, 0xb5, 0x01, 0x00, 0x4c, 0xe5};
static const uint8_t rx_uuid[16]  = {0x9e, 0xca, 0xdc, 0x24, 0x0e, 0xe5, 0xa9, 0xe0,
				     0x93, 0xf3, 0xa3, 0xb5, 0x02, 0x00, 0x4c, 0xe5};
static const uint8_t tx_uuid[16]  = {0x9e, 0xca, 0xdc, 0x24, 0x0e, 0xe5, 0xa9, 0xe0,
				     0x93, 0xf3, 0xa3, 0xb5, 0x03, 0x00, 0x4c, 0xe5};

/* Flat characteristic indices, in declaration order below. */
#define CHR_RX 0
#define CHR_TX 1

static const runtime_ble_char_def_t my_chars[] = {
	{ .uuid = rx_uuid, .uuid_len = 16,
	  .props = RUNTIME_BLE_PROP_WRITE | RUNTIME_BLE_PROP_WRITE_NR, .max_len = 244 },
	{ .uuid = tx_uuid, .uuid_len = 16,
	  .props = RUNTIME_BLE_PROP_NOTIFY, .max_len = 244 },
};
static const runtime_ble_service_def_t my_services[] = {
	{ .uuid = svc_uuid, .uuid_len = 16, .chars = my_chars, .num_chars = 2 },
};

/* Raw ADV data: flags + manufacturer data + complete 128-bit service UUID. */
static const uint8_t adv_data[] = {
	2, 0x01, 0x06,
	6, 0xff, 0xff, 0xff, 0x52, 0x42, 0x01,
	17, 0x07, 0x9e, 0xca, 0xdc, 0x24, 0x0e, 0xe5, 0xa9, 0xe0,
	0x93, 0xf3, 0xa3, 0xb5, 0x01, 0x00, 0x4c, 0xe5
};
/* Raw AD structure: Complete Local Name "RUNTIME-BLE" for scan response. */
static const uint8_t scan_rsp[] = {
	12, 0x09, 'R', 'U', 'N', 'T', 'I', 'M', 'E', '-', 'B', 'L', 'E'
};

static uint8_t bond_blob[RUNTIME_BLE_BOND_BLOB_MAX];
static size_t bond_blob_len;

static void on_log(const char *line, void *user)
{
	ARG_UNUSED(user);
	printk("%s\n", line);
}

static void on_connected(void *user)
{
	ARG_UNUSED(user);
	printk("[app] central connected\n");
	(void)runtime_ble_read_rssi();
	(void)runtime_ble_update_frame_space(0, 0, RUNTIME_BLE_PHY_MASK_1M,
					     RUNTIME_BLE_FRAME_SPACE_ACL_CP |
					     RUNTIME_BLE_FRAME_SPACE_ACL_PC);
	(void)runtime_ble_set_phy(RUNTIME_BLE_PHY_CODED);
	(void)runtime_ble_request_connection_rate(0, 0, 1, 1, 0, 0, 0);
}

static void on_disconnected(uint8_t reason, void *user)
{
	ARG_UNUSED(user);
	printk("[app] central disconnected (reason 0x%02x)\n", reason);
}

static void on_write(uint16_t chr, const uint8_t *data, size_t len, void *user)
{
	ARG_UNUSED(user);
	printk("[app] write chr=%u len=%u -> echo on TX\n", chr, (unsigned int)len);
	if (chr == CHR_RX) {
		(void)runtime_ble_notify(CHR_TX, data, len);
	}
}

static void on_subscription(uint16_t chr, uint8_t notify_enabled, uint8_t indicate_enabled,
			    void *user)
{
	ARG_UNUSED(user);
	printk("[app] subscription chr=%u notify=%u indicate=%u\n",
	       chr, notify_enabled, indicate_enabled);
}
static void on_rssi(int8_t rssi, void *user)
{
	ARG_UNUSED(user);
	printk("[app] RSSI %d dBm\n", rssi);
}

static void on_att_mtu(uint16_t att_mtu, void *user)
{
	ARG_UNUSED(user);
	printk("[app] ATT MTU %u\n", att_mtu);
}

static void on_frame_space(uint32_t frame_space_us, void *user)
{
	ARG_UNUSED(user);
	printk("[app] frame space %u us\n", frame_space_us);
}

static void on_connection_rate(uint16_t interval_ms, uint16_t subrate_factor, uint16_t latency,
			       uint16_t continuation_number, uint16_t timeout_ms, void *user)
{
	ARG_UNUSED(user);
	printk("[app] connection rate interval=%u subrate=%u latency=%u cont=%u timeout=%u\n",
	       interval_ms, subrate_factor, latency, continuation_number, timeout_ms);
}

static void on_security_event(uint8_t event, uint8_t level, uint32_t passkey, uint8_t flags,
			      void *user)
{
	ARG_UNUSED(user);
	printk("[app] security event=%u level=%u passkey=%06u flags=0x%02x\n",
	       event, level, passkey, flags);
	if (event == RUNTIME_BLE_SECURITY_PASSKEY_CONFIRM) {
		(void)runtime_ble_passkey_confirm(1);
	}
}

static size_t on_bond_load(uint8_t index, uint8_t *out, size_t max_len, void *user)
{
	ARG_UNUSED(user);
	if (index != 0 || bond_blob_len == 0 || max_len < bond_blob_len) {
		return 0;
	}
	memcpy(out, bond_blob, bond_blob_len);
	printk("[app] restored bond blob len=%u\n", (unsigned int)bond_blob_len);
	return bond_blob_len;
}

static void on_bond_store(uint8_t index, const uint8_t *blob, size_t len, void *user)
{
	ARG_UNUSED(user);
	if (index != 0 || len > sizeof(bond_blob)) {
		return;
	}
	memcpy(bond_blob, blob, len);
	bond_blob_len = len;
	printk("[app] stored bond blob len=%u\n", (unsigned int)len);
}

int main(void)
{
	static const runtime_ble_config_t cfg = {
		.device_name = "RUNTIME-BLE",
		.adv_data = adv_data,
		.adv_data_len = sizeof(adv_data),
		.scan_response_data = scan_rsp,
		.scan_response_data_len = sizeof(scan_rsp),
		.adv_interval_min_ms = 30,
		.adv_interval_max_ms = 60,
		.discoverable = 0, /* general-discoverable */
		.address = NULL,   /* hwinfo-derived static-random address */
		.services = my_services,
		.num_services = 1,
		.security_bondable = 1,
		.bond_slot_count = 1,
		.callbacks = {
			.on_connected = on_connected,
			.on_disconnected = on_disconnected,
			.on_write = on_write,
			.on_subscription = on_subscription,
			.on_rssi = on_rssi,
			.on_att_mtu = on_att_mtu,
			.on_frame_space = on_frame_space,
			.on_connection_rate = on_connection_rate,
			.on_security_event = on_security_event,
			.on_bond_load = on_bond_load,
			.on_bond_store = on_bond_store,
			.on_log = on_log,
		},
		.user = NULL,
	};

	uint8_t addr[6];

	runtime_ble_addr(addr);
	printk("\n[app] runtime-ble custom-GATT example\n");
	printk("[app] addr %02x:%02x:%02x:%02x:%02x:%02x\n",
	       addr[5], addr[4], addr[3], addr[2], addr[1], addr[0]);

	runtime_ble_init(&cfg);
	if (runtime_ble_load() != RUNTIME_BLE_OK) {
		printk("[app] runtime_ble_load failed\n");
		return 0;
	}
	printk("[app] loaded; advertising \"RUNTIME-BLE\" with a custom vendor service\n");
	return 0;
}
