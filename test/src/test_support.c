/*
 * Shared helpers for the runtime-ble test suites.
 */
#include <zephyr/kernel.h>
#include <zephyr/sys/printk.h>
#include <zephyr/sys/sys_heap.h>

#include "test_support.h"

/* The Zephyr system heap, backing k_aligned_alloc/k_free (what the Rust lib
 * allocates from) and k_thread_stack_alloc (with DYNAMIC_THREAD_PREFER_ALLOC). */
extern struct k_heap _system_heap;

static void on_log(const char *line, void *user)
{
	ARG_UNUSED(user);
	printk("%s\n", line);
}

static void on_subscription(uint16_t chr, uint8_t notify_enabled, uint8_t indicate_enabled,
			    void *user)
{
	ARG_UNUSED(chr);
	ARG_UNUSED(notify_enabled);
	ARG_UNUSED(indicate_enabled);
	ARG_UNUSED(user);
}

static size_t on_bond_load(uint8_t index, uint8_t *out, size_t max_len, void *user)
{
	ARG_UNUSED(index);
	ARG_UNUSED(out);
	ARG_UNUSED(max_len);
	ARG_UNUSED(user);
	return 0;
}

static void on_bond_store(uint8_t index, const uint8_t *blob, size_t len, void *user)
{
	ARG_UNUSED(index);
	ARG_UNUSED(blob);
	ARG_UNUSED(len);
	ARG_UNUSED(user);
}

static uint8_t on_oob_request(uint8_t *local_random, uint8_t *local_confirm, uint8_t *peer_random,
			      uint8_t *peer_confirm, void *user)
{
	ARG_UNUSED(local_random);
	ARG_UNUSED(local_confirm);
	ARG_UNUSED(peer_random);
	ARG_UNUSED(peer_confirm);
	ARG_UNUSED(user);
	return 0;
}

static void on_oob_local_data(const uint8_t *local_random, const uint8_t *local_confirm, void *user)
{
	ARG_UNUSED(local_random);
	ARG_UNUSED(local_confirm);
	ARG_UNUSED(user);
}

static const runtime_ble_config_t cfg = {
	.device_name = "RTBLE-TEST",
	.manufacturer_id = 0xFFFF,
	.adv_interval_min_ms = 30,
	.adv_interval_max_ms = 60,
	/* services == NULL -> built-in Nordic UART Service. */
	.callbacks = {
		.on_subscription = on_subscription,
		.on_bond_load = on_bond_load,
		.on_bond_store = on_bond_store,
		.on_oob_request = on_oob_request,
		.on_oob_local_data = on_oob_local_data,
		.on_log = on_log,
	},
};

const runtime_ble_config_t *test_base_cfg(void)
{
	return &cfg;
}

size_t test_heap_free(void)
{
	struct sys_memory_stats st = {0};

	sys_heap_runtime_stats_get(&_system_heap.heap, &st);
	return st.free_bytes;
}

void test_load_settled(void)
{
	(void)runtime_ble_load();
	k_sleep(K_MSEC(400));
}
