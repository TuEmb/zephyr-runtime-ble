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

ZTEST(runtime_ble_edge, test_security_argument_validation)
{
	zassert_equal(runtime_ble_passkey_input(1000000), RUNTIME_BLE_ERR_INVALID,
		      "passkey must be a 6-digit value");
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
