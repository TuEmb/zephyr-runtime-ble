/*
 * Lifecycle tests: the normal init -> load -> unload paths, idempotency, and
 * re-loadability (a session can be torn down and brought back up).
 */
#include <zephyr/ztest.h>
#include <zephyr/kernel.h>

#include "runtime_ble.h"
#include "test_support.h"

static void *lifecycle_setup(void)
{
	zassert_equal(runtime_ble_init(test_base_cfg()), RUNTIME_BLE_OK, "init failed");
	return NULL;
}

ZTEST_SUITE(runtime_ble_lifecycle, NULL, lifecycle_setup, NULL, NULL, NULL);

/* The per-device address must be a BLE static-random address (top two bits set). */
ZTEST(runtime_ble_lifecycle, test_addr_is_static_random)
{
	uint8_t a[6] = {0};

	runtime_ble_addr(a);
	zassert_equal(a[5] & 0xC0, 0xC0, "addr 0x..%02x is not a static-random address", a[5]);
}

ZTEST(runtime_ble_lifecycle, test_load_then_unload)
{
	zassert_equal(runtime_ble_load(), RUNTIME_BLE_OK, "load failed");
	k_sleep(K_MSEC(400));
	zassert_equal(runtime_ble_unload(), RUNTIME_BLE_OK, "unload failed");
}

/* Loading twice without an intervening unload is a no-op OK (one session), and
 * a single unload tears that one session down. */
ZTEST(runtime_ble_lifecycle, test_double_load_is_idempotent)
{
	zassert_equal(runtime_ble_load(), RUNTIME_BLE_OK, "1st load failed");
	zassert_equal(runtime_ble_load(), RUNTIME_BLE_OK, "2nd load should be a no-op OK");
	k_sleep(K_MSEC(300));
	zassert_equal(runtime_ble_unload(), RUNTIME_BLE_OK, "unload failed");
}

/* A session can be unloaded and loaded again (the whole point of "loadable"). */
ZTEST(runtime_ble_lifecycle, test_reload_after_unload)
{
	for (int i = 0; i < 2; i++) {
		zassert_equal(runtime_ble_load(), RUNTIME_BLE_OK, "reload %d failed", i);
		k_sleep(K_MSEC(300));
		zassert_equal(runtime_ble_unload(), RUNTIME_BLE_OK, "unload %d failed", i);
	}
}
