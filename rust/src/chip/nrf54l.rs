//! nRF54L radio bring-up (features `nrf54l15` / `nrf54l10` / `nrf54l05` /
//! `nrf54lm20` — all share the same nRF54L peripherals/interrupts).
//!
//! Builds MPSL + the SoftDevice Controller on the Zephyr heap, wires the
//! MPSL/SDC interrupts (the C shims below are connected via IRQ_CONNECT in
//! glue/glue.c), then hands the controller to the shared session runner.

use alloc::boxed::Box;
use core::ffi::c_int;

use embassy_nrf::interrupt::typelevel::{self, Binding, Handler};
use embassy_nrf::{cracen, mode::Blocking};
use nrf_sdc::mpsl::MultiprotocolServiceLayer;
use nrf_sdc::{self as sdc, mpsl};
use trouble_host::prelude::*;

use super::{device_address, log, serve_session, Resources};
use crate::RuntimeCfg;

const L2CAP_TXQ: u8 = 4;
const L2CAP_RXQ: u8 = 4;

// ---------------------------------------------------------------------------
// Interrupts: `Irqs` is a type-level promise; the real ISRs are the C-callable
// shims below, connected from Zephyr via IRQ_CONNECT in glue.c.
// ---------------------------------------------------------------------------
#[derive(Clone, Copy)]
pub(super) struct Irqs;
unsafe impl Binding<typelevel::SWI00, mpsl::LowPrioInterruptHandler> for Irqs {}
unsafe impl Binding<typelevel::CLOCK_POWER, mpsl::ClockInterruptHandler> for Irqs {}
unsafe impl Binding<typelevel::RADIO_0, mpsl::HighPrioInterruptHandler> for Irqs {}
unsafe impl Binding<typelevel::TIMER10, mpsl::HighPrioInterruptHandler> for Irqs {}
unsafe impl Binding<typelevel::GRTC_3, mpsl::HighPrioInterruptHandler> for Irqs {}

#[no_mangle]
pub extern "C" fn runtime_irq_radio() {
    unsafe { <mpsl::HighPrioInterruptHandler as Handler<typelevel::RADIO_0>>::on_interrupt() }
}
#[no_mangle]
pub extern "C" fn runtime_irq_timer10() {
    unsafe { <mpsl::HighPrioInterruptHandler as Handler<typelevel::TIMER10>>::on_interrupt() }
}
#[no_mangle]
pub extern "C" fn runtime_irq_grtc3() {
    unsafe { <mpsl::HighPrioInterruptHandler as Handler<typelevel::GRTC_3>>::on_interrupt() }
}
#[no_mangle]
pub extern "C" fn runtime_irq_clock_power() {
    unsafe { <mpsl::ClockInterruptHandler as Handler<typelevel::CLOCK_POWER>>::on_interrupt() }
}
#[no_mangle]
pub extern "C" fn runtime_irq_swi00() {
    unsafe { <mpsl::LowPrioInterruptHandler as Handler<typelevel::SWI00>>::on_interrupt() }
}

fn build_sdc<'d, const N: usize>(
    p: nrf_sdc::Peripherals<'d>,
    rng: &'d mut cracen::Cracen<'static, Blocking>,
    mpsl: &'d MultiprotocolServiceLayer,
    mem: &'d mut sdc::Mem<N>,
) -> Result<nrf_sdc::SoftdeviceController<'d>, nrf_sdc::Error> {
    sdc::Builder::new()?
        .support_adv()
        .support_peripheral()
        .support_le_2m_phy()
        .support_phy_update_peripheral()
        .support_dle_peripheral()
        .peripheral_count(1)?
        .buffer_cfg(
            DefaultPacketPool::MTU as u16,
            DefaultPacketPool::MTU as u16,
            L2CAP_TXQ,
            L2CAP_RXQ,
        )?
        .build(p, rng, mpsl, mem)
}

/// One load->unload session. Allocates MPSL/SDC/host resources on the heap,
/// runs until unload, then frees everything.
pub(crate) fn run(cfg: Option<&'static RuntimeCfg>, _mode: c_int) {
    let cfg: &'static RuntimeCfg = match cfg {
        Some(c) => c,
        None => return,
    };
    // Zephyr already configured clocks/regulators; steal the peripherals.
    let p = unsafe { embassy_nrf::Peripherals::steal() };

    let mpsl_p = mpsl::Peripherals::new(
        p.GRTC_CH7, p.GRTC_CH8, p.GRTC_CH9, p.GRTC_CH10, p.GRTC_CH11, p.TIMER10, p.TIMER20, p.TEMP,
        p.PPI10_CH0, p.PPI20_CH1, p.PPIB11_CH0, p.PPIB21_CH0,
    );
    let lfclk_cfg = mpsl::raw::mpsl_clock_lfclk_cfg_t {
        source: mpsl::raw::MPSL_CLOCK_LF_SRC_XTAL as u8,
        rc_ctiv: 0,
        rc_temp_ctiv: 0,
        accuracy_ppm: 50,
        skip_wait_lfclk_started: false,
    };
    let mpsl_ptr = Box::into_raw(Box::new(
        mpsl::MultiprotocolServiceLayer::new(mpsl_p, Irqs, lfclk_cfg).unwrap(),
    ));
    let mpsl: &'static MultiprotocolServiceLayer = unsafe { &*mpsl_ptr };

    let sdc_p = sdc::Peripherals::new(
        p.PPI00_CH1, p.PPI00_CH3, p.PPI10_CH1, p.PPI10_CH2, p.PPI10_CH3, p.PPI10_CH4, p.PPI10_CH5,
        p.PPI10_CH6, p.PPI10_CH7, p.PPI10_CH8, p.PPI10_CH9, p.PPI10_CH10, p.PPI10_CH11, p.PPIB00_CH1,
        p.PPIB00_CH2, p.PPIB00_CH3, p.PPIB10_CH1, p.PPIB10_CH2, p.PPIB10_CH3,
    );
    let rng_ptr = Box::into_raw(Box::new(cracen::Cracen::new_blocking(p.CRACEN)));
    let mem_ptr = Box::into_raw(Box::new(sdc::Mem::<11264>::new()));
    let sdc = build_sdc(sdc_p, unsafe { &mut *rng_ptr }, mpsl, unsafe { &mut *mem_ptr }).unwrap();

    let res_ptr: *mut Resources = Box::into_raw(Box::new(HostResources::new()));
    let stack = trouble_host::new(sdc, unsafe { &mut *res_ptr })
        .set_random_address(device_address(cfg))
        .set_io_capabilities(IoCapabilities::NoInputNoOutput)
        .build();

    log(cfg, c"[runtime-ble] loaded on heap; advertising");
    serve_session(mpsl, &stack, cfg);

    // Teardown: drop the Stack first (SoftdeviceController Drop -> sdc_disable),
    // then reclaim every per-session allocation.
    drop(stack);
    unsafe {
        let _ = Box::from_raw(res_ptr);
        let _ = Box::from_raw(mem_ptr);
        let _ = Box::from_raw(rng_ptr);
        let _ = Box::from_raw(mpsl_ptr);
    }
    log(cfg, c"[runtime-ble] unloaded; heap freed");
}
