/*
 * glue.c — Zephyr side of the trouble-based loadable BLE runtime.
 *
 * Provides to the Rust staticlib: the heap allocator (Zephyr's k_aligned_alloc/
 * k_free are used directly), a fatal handler, an embassy-time alarm backed by a
 * k_timer, a park/wake semaphore for the custom block_on executor, the per-device
 * BLE address (hwinfo), and the MPSL/SDC interrupt vector entries (IRQ_CONNECT).
 *
 * The library owns the radio: build the app with CONFIG_BT=n / CONFIG_MPSL=n.
 */
#include <zephyr/kernel.h>
#include <zephyr/irq.h>
#include <zephyr/drivers/hwinfo.h>
#include <string.h>
#include <stdint.h>
#include "runtime_ble.h"

#define RUNTIME_BLE_STACK_SIZE  CONFIG_RUNTIME_BLE_THREAD_STACK_SIZE
#define RUNTIME_BLE_THREAD_PRIO CONFIG_RUNTIME_BLE_THREAD_PRIORITY

/* ---- symbols the Rust lib imports ---- */
int64_t runtime_uptime_ms(void)
{
	return k_uptime_get();
}

/* A stable per-device 6-byte BLE static-random address from the SoC unique id
 * (hwinfo). Portable across SoCs and stable across re-flashes. */
void runtime_ble_addr(uint8_t out[6])
{
	uint8_t hwid[8] = {0};
	ssize_t len = hwinfo_get_device_id(hwid, sizeof(hwid));
	static const uint8_t fallback[6] = {0x52, 0x55, 0x4e, 0x42, 0x4c, 0x45};
	const uint8_t *src = (len >= 6) ? hwid : fallback;

	for (int i = 0; i < 6; i++) {
		out[i] = src[i];
	}
	out[5] = (uint8_t)((out[5] & 0x3Fu) | 0xC0u); /* top 2 bits = static random */
}

void runtime_ble_fatal(const char *msg)
{
	printk("runtime-ble FATAL: %s\n", msg ? msg : "?");
	k_panic();
	for (;;) {
	}
}

/* block_on park: binary semaphore (give saturates -> one re-poll catches all). */
static struct k_sem runtime_sem;
void runtime_ble_wait(void)
{
	k_sem_take(&runtime_sem, K_FOREVER);
}
void runtime_ble_wake(void)
{
	k_sem_give(&runtime_sem);
}

/* embassy-time alarm */
extern void runtime_alarm_fired(void);
static struct k_timer runtime_timer;
static void runtime_timer_cb(struct k_timer *t)
{
	ARG_UNUSED(t);
	runtime_alarm_fired();
}
void runtime_alarm_set(uint64_t at_ms)
{
	int64_t d = (int64_t)at_ms - k_uptime_get();

	if (d < 0) {
		d = 0;
	}
	k_timer_start(&runtime_timer, K_MSEC(d), K_NO_WAIT);
}

/* ---- dynamic BLE thread lifecycle ---- */
static struct k_thread runtime_thread;
static k_thread_stack_t *runtime_stack;
static bool runtime_loaded;

static void runtime_entry(void *a, void *b, void *c)
{
	ARG_UNUSED(a);
	ARG_UNUSED(b);
	ARG_UNUSED(c);
	runtime_ble_run(0); /* returns on unload */
}

int runtime_ble_load(void)
{
	if (runtime_loaded) {
		return RUNTIME_BLE_OK;
	}
	runtime_stack = k_thread_stack_alloc(RUNTIME_BLE_STACK_SIZE, 0);
	if (runtime_stack == NULL) {
		printk("[runtime-ble] stack alloc failed\n");
		return RUNTIME_BLE_ERR_NO_MEM;
	}
	k_sem_reset(&runtime_sem);

#if defined(CONFIG_SOC_SERIES_NRF54L) || defined(CONFIG_SOC_SERIES_NRF54LX) || \
	defined(CONFIG_SOC_SERIES_NRF54LM20)
	/* Enable the MPSL IRQs only for the lifetime of a session. */
	irq_enable(RADIO_0_IRQn);
	irq_enable(TIMER10_IRQn);
	irq_enable(GRTC_3_IRQn);
	irq_enable(SWI00_IRQn);
#elif defined(CONFIG_SOC_SERIES_NRF52) || defined(CONFIG_SOC_SERIES_NRF52X)
	irq_enable(RADIO_IRQn);
	irq_enable(TIMER0_IRQn);
	irq_enable(RTC0_IRQn);
	irq_enable(SWI0_EGU0_IRQn);
#else
#error "runtime-ble: unsupported SoC. Add the MPSL/SDC IRQ wiring for your chip."
#endif

	k_thread_create(&runtime_thread, runtime_stack, RUNTIME_BLE_STACK_SIZE,
			runtime_entry, NULL, NULL, NULL,
			K_PRIO_PREEMPT(RUNTIME_BLE_THREAD_PRIO), 0, K_NO_WAIT);
	runtime_loaded = true;
	return RUNTIME_BLE_OK;
}

int runtime_ble_unload(void)
{
	if (!runtime_loaded) {
		return RUNTIME_BLE_OK;
	}
	runtime_ble_signal_unload();
	runtime_ble_wake(); /* unblock block_on so it re-polls and sees the unload */
	k_thread_join(&runtime_thread, K_FOREVER);

#if defined(CONFIG_SOC_SERIES_NRF54L) || defined(CONFIG_SOC_SERIES_NRF54LX) || \
	defined(CONFIG_SOC_SERIES_NRF54LM20)
	irq_disable(RADIO_0_IRQn);
	irq_disable(TIMER10_IRQn);
	irq_disable(GRTC_3_IRQn);
	irq_disable(SWI00_IRQn);
#elif defined(CONFIG_SOC_SERIES_NRF52) || defined(CONFIG_SOC_SERIES_NRF52X)
	irq_disable(RADIO_IRQn);
	irq_disable(TIMER0_IRQn);
	irq_disable(RTC0_IRQn);
	irq_disable(SWI0_EGU0_IRQn);
#endif

	k_thread_stack_free(runtime_stack);
	runtime_stack = NULL;
	runtime_loaded = false;
	return RUNTIME_BLE_OK;
}

/* radio/MPSL interrupt shims (defined in Rust, per chip) */
#if defined(CONFIG_SOC_SERIES_NRF54L) || defined(CONFIG_SOC_SERIES_NRF54LX) || \
	defined(CONFIG_SOC_SERIES_NRF54LM20)
extern void runtime_irq_radio(void);
extern void runtime_irq_timer10(void);
extern void runtime_irq_grtc3(void);
extern void runtime_irq_swi00(void);
#elif defined(CONFIG_SOC_SERIES_NRF52) || defined(CONFIG_SOC_SERIES_NRF52X)
extern void runtime_irq_radio(void);
extern void runtime_irq_timer0(void);
extern void runtime_irq_rtc0(void);
extern void runtime_irq_egu0_swi0(void);
#endif

static int runtime_glue_init(void)
{
	k_sem_init(&runtime_sem, 0, 1);
	k_timer_init(&runtime_timer, runtime_timer_cb, NULL);

#if defined(CONFIG_SOC_SERIES_NRF54L) || defined(CONFIG_SOC_SERIES_NRF54LX) || \
	defined(CONFIG_SOC_SERIES_NRF54LM20)
	/* CLOCK_POWER is owned by Zephyr's clock driver — ceded. Static vector
	 * entries only; enabled per-session in runtime_ble_load. */
	IRQ_CONNECT(RADIO_0_IRQn, 0, runtime_irq_radio, NULL, 0);
	IRQ_CONNECT(TIMER10_IRQn, 0, runtime_irq_timer10, NULL, 0);
	IRQ_CONNECT(GRTC_3_IRQn, 0, runtime_irq_grtc3, NULL, 0);
	IRQ_CONNECT(SWI00_IRQn, 4, runtime_irq_swi00, NULL, 0);
#elif defined(CONFIG_SOC_SERIES_NRF52) || defined(CONFIG_SOC_SERIES_NRF52X)
	/* CLOCK_POWER (POWER_CLOCK) is owned by Zephyr's clock driver — ceded. */
	IRQ_CONNECT(RADIO_IRQn, 0, runtime_irq_radio, NULL, 0);
	IRQ_CONNECT(TIMER0_IRQn, 0, runtime_irq_timer0, NULL, 0);
	IRQ_CONNECT(RTC0_IRQn, 0, runtime_irq_rtc0, NULL, 0);
	IRQ_CONNECT(SWI0_EGU0_IRQn, 4, runtime_irq_egu0_swi0, NULL, 0);
#endif
	return 0;
}
SYS_INIT(runtime_glue_init, APPLICATION, 90);
