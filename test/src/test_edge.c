/*
 * Edge cases: API calls made out of the expected order or with bad arguments
 * must fail gracefully (a documented error code) and never crash the device.
 *
 * Precondition note: runtime_ble_load() reads the config captured by
 * runtime_ble_init(); calling load() with no prior init is undefined (it
 * faults). Every suite therefore inits in setup, so "load before init" is not
 * exercised here — it is a documented precondition, not a supported path.
 */
#include <zephyr/ztest.h>
#include <zephyr/kernel.h>

#include "runtime_ble.h"
#include "test_support.h"

static void *edge_setup(void)
{
	zassert_equal(runtime_ble_init(test_base_cfg()), RUNTIME_BLE_OK, "init failed");
	return NULL;
}

ZTEST_SUITE(runtime_ble_edge, NULL, edge_setup, NULL, NULL, NULL);

ZTEST(runtime_ble_edge, test_init_null_is_rejected)
{
	zassert_equal(runtime_ble_init(NULL), RUNTIME_BLE_ERR_INVALID, "NULL cfg must be rejected");
	/* Restore a valid config for the rest of the suite. */
	zassert_equal(runtime_ble_init(test_base_cfg()), RUNTIME_BLE_OK, "re-init failed");
}

/* Unloading when nothing is loaded is a no-op OK. */
ZTEST(runtime_ble_edge, test_unload_without_load)
{
	zassert_equal(runtime_ble_unload(), RUNTIME_BLE_OK, "unload with nothing loaded must be OK");
}

/* Unloading twice is a no-op OK on the second call. */
ZTEST(runtime_ble_edge, test_double_unload)
{
	test_load_settled();
	zassert_equal(runtime_ble_unload(), RUNTIME_BLE_OK, "1st unload failed");
	zassert_equal(runtime_ble_unload(), RUNTIME_BLE_OK, "2nd unload must be a no-op OK");
}

/* Bad send arguments are rejected without touching the queue. */
ZTEST(runtime_ble_edge, test_send_argument_validation)
{
	const uint8_t buf[4] = {1, 2, 3, 4};

	zassert_equal(runtime_ble_send(NULL, 4), RUNTIME_BLE_ERR_INVALID, "NULL data");
	zassert_equal(runtime_ble_send(buf, 0), RUNTIME_BLE_ERR_INVALID, "zero length");
	zassert_equal(runtime_ble_send(buf, 100000), RUNTIME_BLE_ERR_INVALID, "oversized length");
}

ZTEST(runtime_ble_edge, test_indicate_argument_validation)
{
	const uint8_t buf[4] = {1, 2, 3, 4};

	zassert_equal(runtime_ble_indicate(0, NULL, 4), RUNTIME_BLE_ERR_INVALID, "NULL data");
	zassert_equal(runtime_ble_indicate(0, buf, 0), RUNTIME_BLE_ERR_INVALID, "zero length");
	zassert_equal(runtime_ble_indicate(0, buf, 100000), RUNTIME_BLE_ERR_INVALID,
		      "oversized length");
}

ZTEST(runtime_ble_edge, test_security_argument_validation)
{
	zassert_equal(runtime_ble_passkey_input(1000000), RUNTIME_BLE_ERR_INVALID,
		      "passkey must be a 6-digit value");
}

ZTEST(runtime_ble_edge, test_central_indicate_subscribe_requires_central_lib)
{
	zassert_equal(runtime_ble_client_subscribe_indicate(1), RUNTIME_BLE_ERR_INVALID,
		      "default peripheral lib must reject central indication subscribe");
}

ZTEST(runtime_ble_edge, test_central_read_blob_requires_central_lib)
{
	zassert_equal(runtime_ble_client_read_blob(1, 4), RUNTIME_BLE_ERR_INVALID,
		      "default peripheral lib must reject central read blob");
}

ZTEST(runtime_ble_edge, test_central_discover_all_requires_central_lib)
{
	zassert_equal(runtime_ble_client_discover_all(), RUNTIME_BLE_ERR_INVALID,
		      "default peripheral lib must reject central discover all");
}

ZTEST(runtime_ble_edge, test_central_discover_services_requires_central_lib)
{
	zassert_equal(runtime_ble_client_discover_services(), RUNTIME_BLE_ERR_INVALID,
		      "default peripheral lib must reject central service discovery");
}

ZTEST(runtime_ble_edge, test_oob_security_config_init)
{
	runtime_ble_config_t cfg = *test_base_cfg();

	cfg.security_oob_available = 1;
	cfg.security_request_on_connect = 1;
	zassert_equal(runtime_ble_init(&cfg), RUNTIME_BLE_OK, "OOB security config init failed");
	test_load_settled();
	zassert_equal(runtime_ble_unload(), RUNTIME_BLE_OK, "cleanup unload failed");
	zassert_equal(runtime_ble_init(test_base_cfg()), RUNTIME_BLE_OK, "restore base cfg failed");
}

ZTEST(runtime_ble_edge, test_gatt_permission_flags_init)
{
	static const uint8_t svc_uuid[16] = {0x9e, 0xca, 0xdc, 0x24, 0x0e, 0xe5, 0xa9, 0xe0,
					     0x93, 0xf3, 0xa3, 0xb5, 0x11, 0x00, 0x4c, 0xe5};
	static const uint8_t chr_uuid[16] = {0x9e, 0xca, 0xdc, 0x24, 0x0e, 0xe5, 0xa9, 0xe0,
					     0x93, 0xf3, 0xa3, 0xb5, 0x12, 0x00, 0x4c, 0xe5};
	static const runtime_ble_char_def_t chars[] = {
		{ .uuid = chr_uuid,
		  .uuid_len = sizeof(chr_uuid),
		  .props = RUNTIME_BLE_PROP_READ | RUNTIME_BLE_PROP_WRITE |
			   RUNTIME_BLE_PROP_NOTIFY,
		  .max_len = 32,
		  .permissions = RUNTIME_BLE_PERM_READ_ENCRYPT |
				 RUNTIME_BLE_PERM_WRITE_AUTH |
				 RUNTIME_BLE_PERM_CCCD_ENCRYPT },
	};
	static const runtime_ble_service_def_t services[] = {
		{ .uuid = svc_uuid, .uuid_len = sizeof(svc_uuid), .chars = chars, .num_chars = 1 },
	};
	runtime_ble_config_t cfg = *test_base_cfg();

	cfg.services = services;
	cfg.num_services = 1;
	cfg.security_bondable = 1;
	zassert_equal(runtime_ble_init(&cfg), RUNTIME_BLE_OK, "permission config init failed");
	test_load_settled();
	zassert_equal(runtime_ble_unload(), RUNTIME_BLE_OK, "cleanup unload failed");
	zassert_equal(runtime_ble_init(test_base_cfg()), RUNTIME_BLE_OK, "restore base cfg failed");
}

/* With no session/central, the single outstanding-send slot fills after one
 * queued send; a second send reports the queue is full (no crash, no overwrite).
 * A subsequent load() resets the slot and unload() returns the RAM. */
ZTEST(runtime_ble_edge, test_send_before_load_queues_then_full)
{
	const uint8_t buf[4] = {1, 2, 3, 4};

	zassert_equal(runtime_ble_send(buf, sizeof(buf)), RUNTIME_BLE_OK, "first send should queue");
	zassert_equal(runtime_ble_send(buf, sizeof(buf)), RUNTIME_BLE_ERR_NO_MEM,
		      "second send should report the queue is full");

	test_load_settled();
	zassert_equal(runtime_ble_unload(), RUNTIME_BLE_OK, "cleanup unload failed");
}

/* Notifying a characteristic index that does not exist is accepted at queue
 * time and silently dropped by the session — it must never crash. */
ZTEST(runtime_ble_edge, test_notify_unknown_characteristic)
{
	const uint8_t buf[2] = {0xAB, 0xCD};

	zassert_equal(runtime_ble_notify(0xFFFE, buf, sizeof(buf)), RUNTIME_BLE_OK,
		      "notify to an unknown characteristic should queue without error");

	test_load_settled();
	zassert_equal(runtime_ble_unload(), RUNTIME_BLE_OK, "cleanup unload failed");
}

ZTEST(runtime_ble_edge, test_indicate_unknown_characteristic)
{
	const uint8_t buf[2] = {0xAB, 0xCD};

	zassert_equal(runtime_ble_indicate(0xFFFE, buf, sizeof(buf)), RUNTIME_BLE_OK,
		      "indicate to an unknown characteristic should queue without error");

	test_load_settled();
	zassert_equal(runtime_ble_unload(), RUNTIME_BLE_OK, "cleanup unload failed");
}
