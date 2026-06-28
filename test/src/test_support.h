/*
 * Shared helpers for the runtime-ble on-target test suites.
 */
#ifndef RUNTIME_BLE_TEST_SUPPORT_H_
#define RUNTIME_BLE_TEST_SUPPORT_H_

#include <stddef.h>
#include "runtime_ble.h"

/* A minimal, valid config used by every suite: built-in NUS (services == NULL),
 * a name, and a log callback. Static storage — safe to pass to runtime_ble_init. */
const runtime_ble_config_t *test_base_cfg(void);

/* Free bytes currently available in the Zephyr system heap — the heap the
 * runtime allocates its session (and, with PREFER_ALLOC, its thread stack)
 * from. Requires CONFIG_SYS_HEAP_RUNTIME_STATS=y. */
size_t test_heap_free(void);

/* Load a session and wait long enough for the runtime thread to bring MPSL/SDC
 * up and reach the advertising loop, so a following unload exercises a real
 * teardown (not a race against init). */
void test_load_settled(void);

#endif /* RUNTIME_BLE_TEST_SUPPORT_H_ */
