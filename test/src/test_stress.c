/*
 * Stress test: many load/unload cycles must neither crash nor leak heap.
 *
 * "No crash" is verified implicitly — a fault would abort the whole run before
 * the assertions are reached. "No leak" is checked by comparing the system-heap
 * free bytes across the cycles: everything a load() allocates, unload() must
 * return, so free should be unchanged (within allocator alignment slop).
 */
#include <zephyr/ztest.h>
#include <zephyr/kernel.h>
#include <zephyr/sys/printk.h>

#include "runtime_ble.h"
#include "test_support.h"

#define STRESS_CYCLES        10
/* Allowance for heap allocator alignment/fragmentation slop. A real per-load
 * leak grows ~linearly with the cycle count and blows past this immediately. */
#define LEAK_TOLERANCE_BYTES 128

static void *stress_setup(void)
{
	zassert_equal(runtime_ble_init(test_base_cfg()), RUNTIME_BLE_OK, "init failed");
	return NULL;
}

ZTEST_SUITE(runtime_ble_stress, NULL, stress_setup, NULL, NULL, NULL);

ZTEST(runtime_ble_stress, test_repeated_load_unload_no_leak)
{
	/* One warm-up cycle absorbs any one-time/lazy allocations so the
	 * measurement isolates the steady-state per-cycle cost. */
	test_load_settled();
	zassert_equal(runtime_ble_unload(), RUNTIME_BLE_OK, "warm-up unload failed");

	const size_t before = test_heap_free();

	for (int i = 0; i < STRESS_CYCLES; i++) {
		zassert_equal(runtime_ble_load(), RUNTIME_BLE_OK, "load failed at cycle %d", i);
		k_sleep(K_MSEC(300));
		zassert_equal(runtime_ble_unload(), RUNTIME_BLE_OK, "unload failed at cycle %d", i);
	}

	const size_t after = test_heap_free();
	const long leaked = (long)before - (long)after;

	printk("[stress] heap free: before=%zu after=%zu  leaked=%ld over %d cycles (%ld B/cycle)\n",
	       before, after, leaked, STRESS_CYCLES, leaked / STRESS_CYCLES);

	zassert_true(leaked <= LEAK_TOLERANCE_BYTES,
		     "leaked %ld bytes over %d load/unload cycles (~%ld B/cycle)",
		     leaked, STRESS_CYCLES, leaked / STRESS_CYCLES);
}
