// SPDX-License-Identifier: Apache-2.0
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

#[cfg(feature = "central")]
use super::serve_central;
use super::{device_address, log, log_str, serve_session, Resources, EXT_ADV_DATA_MAX};
use crate::RuntimeCfg;

const L2CAP_TXQ: u8 = 4;
const L2CAP_RXQ: u8 = 4;

// SoftDevice Controller memory pool. The central role needs more (scan + central
// link state), so size it up when that feature is compiled in. The lean variant
// drops ext/periodic/coded/subrating/frame-space (peripheral-only), so its
// controller fits a smaller pool.
// Right-sized to the lean feature set: measured `required_memory()` = 5688 B on
// nRF54L15 (logged at load), rounded up with a small margin.
#[cfg(feature = "lean")]
const SDC_MEM: usize = 6144;
#[cfg(all(not(feature = "central"), not(feature = "lean")))]
const SDC_MEM: usize = 18432;
#[cfg(all(feature = "central", not(feature = "lean")))]
const SDC_MEM: usize = 24576;

fn io_capability_from_c(capability: u8) -> IoCapabilities {
    match capability {
        1 => IoCapabilities::DisplayOnly,
        2 => IoCapabilities::DisplayYesNo,
        3 => IoCapabilities::KeyboardOnly,
        5 => IoCapabilities::KeyboardDisplay,
        _ => IoCapabilities::NoInputNoOutput,
    }
}

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
    cfg: &RuntimeCfg,
    p: nrf_sdc::Peripherals<'d>,
    rng: &'d mut cracen::Cracen<'static, Blocking>,
    mpsl: &'d MultiprotocolServiceLayer,
    mem: &'d mut sdc::Mem<N>,
) -> Result<nrf_sdc::SoftdeviceController<'d>, nrf_sdc::Error> {
    // The lean variant hard-disables the heavy optional features at compile time
    // so its controller fits the smaller SDC_MEM pool (see above).
    #[cfg(not(feature = "lean"))]
    let dis = cfg.sdc_disable;
    #[cfg(feature = "lean")]
    let dis = cfg.sdc_disable
        | crate::SDC_DISABLE_CODED_PHY
        | crate::SDC_DISABLE_SUBRATING
        | crate::SDC_DISABLE_FRAME_SPACE;

    // A feature the app actually configures is kept regardless of the disable mask.
    #[cfg(not(feature = "lean"))]
    let want_ext_adv = cfg.adv_extended != 0
        || cfg.periodic_adv != 0
        || (dis & crate::SDC_DISABLE_EXT_ADV) == 0;
    #[cfg(not(feature = "lean"))]
    let want_periodic = cfg.periodic_adv != 0 || (dis & crate::SDC_DISABLE_PERIODIC_ADV) == 0;
    #[cfg(feature = "lean")]
    let want_ext_adv = false;
    #[cfg(feature = "lean")]
    let want_periodic = false;

    let mut b = sdc::Builder::new()?.support_adv().support_peripheral();
    if want_ext_adv {
        b = b.support_ext_adv();
    }
    if (dis & crate::SDC_DISABLE_2M_PHY) == 0 {
        b = b.support_le_2m_phy();
    }
    if (dis & crate::SDC_DISABLE_CODED_PHY) == 0 {
        b = b.support_le_coded_phy();
    }
    if want_periodic {
        b = b.support_le_periodic_adv();
    }
    b = b.support_phy_update_peripheral();
    if (dis & crate::SDC_DISABLE_DLE) == 0 {
        b = b.support_dle_peripheral();
    }
    if (dis & crate::SDC_DISABLE_FRAME_SPACE) == 0 {
        b = b.support_frame_space_update_peripheral();
    }
    // The lean variant drops the LE extended feature-set exchange too (a basic
    // peripheral does not need it); keep it in the full builds.
    #[cfg(not(feature = "lean"))]
    {
        b = b.support_extended_feature_set();
    }
    if (dis & crate::SDC_DISABLE_SUBRATING) == 0 {
        b = b.support_connection_subrating_peripheral();
    }
    #[cfg(feature = "central")]
    {
        b = b
            .support_scan()
            .support_ext_scan()
            .support_central()
            .support_ext_central()
            .support_phy_update_central();
        if (dis & crate::SDC_DISABLE_DLE) == 0 {
            b = b.support_dle_central();
        }
        if (dis & crate::SDC_DISABLE_FRAME_SPACE) == 0 {
            b = b.support_frame_space_update_central();
        }
        if (dis & crate::SDC_DISABLE_SUBRATING) == 0 {
            b = b.support_connection_subrating_central();
        }
    }
    let adv_buf = (if want_ext_adv { EXT_ADV_DATA_MAX } else { 31 }) as u16;
    b = b.peripheral_count(1)?.adv_count(1)?.adv_buffer_cfg(adv_buf)?;
    if want_periodic {
        b = b.periodic_adv_count(1)?;
    }
    #[cfg(feature = "central")]
    {
        b = b.central_count(1)?;
    }
    let b = b.buffer_cfg(
        DefaultPacketPool::MTU as u16,
        DefaultPacketPool::MTU as u16,
        L2CAP_TXQ,
        L2CAP_RXQ,
    )?;
    // Report the exact controller memory this feature set needs vs the reserved
    // SDC_MEM pool, so a lib builder can right-size SDC_MEM (see rust/README.md).
    log_required_mem(cfg, b.required_memory(), N);
    b.build(p, rng, mpsl, mem)
}

/// Log "[runtime-ble] sdc mem: need <req> have <pool>" via the app's on_log.
fn log_required_mem(cfg: &RuntimeCfg, req: Result<usize, nrf_sdc::Error>, pool: usize) {
    use core::fmt::Write;
    let mut s: heapless::String<56> = heapless::String::new();
    let _ = match req {
        Ok(r) => write!(s, "[runtime-ble] sdc mem: need {r} have {pool}\0"),
        Err(_) => write!(s, "[runtime-ble] sdc required_memory query failed\0"),
    };
    log_str(cfg, &s);
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
        p.GRTC_CH7,
        p.GRTC_CH8,
        p.GRTC_CH9,
        p.GRTC_CH10,
        p.GRTC_CH11,
        p.TIMER10,
        p.TIMER20,
        p.TEMP,
        p.PPI10_CH0,
        p.PPI20_CH1,
        p.PPIB11_CH0,
        p.PPIB21_CH0,
    );
    let lfclk_cfg = mpsl::raw::mpsl_clock_lfclk_cfg_t {
        source: mpsl::raw::MPSL_CLOCK_LF_SRC_XTAL as u8,
        rc_ctiv: 0,
        rc_temp_ctiv: 0,
        accuracy_ppm: 50,
        skip_wait_lfclk_started: false,
    };
    let mpsl_ptr = match mpsl::MultiprotocolServiceLayer::new(mpsl_p, Irqs, lfclk_cfg) {
        Ok(m) => Box::into_raw(Box::new(m)),
        Err(_) => {
            log(cfg, c"[runtime-ble] MPSL init failed; aborting load");
            return;
        }
    };
    let mpsl: &'static MultiprotocolServiceLayer = unsafe { &*mpsl_ptr };

    let sdc_p = sdc::Peripherals::new(
        p.PPI00_CH1,
        p.PPI00_CH3,
        p.PPI10_CH1,
        p.PPI10_CH2,
        p.PPI10_CH3,
        p.PPI10_CH4,
        p.PPI10_CH5,
        p.PPI10_CH6,
        p.PPI10_CH7,
        p.PPI10_CH8,
        p.PPI10_CH9,
        p.PPI10_CH10,
        p.PPI10_CH11,
        p.PPIB00_CH1,
        p.PPIB00_CH2,
        p.PPIB00_CH3,
        p.PPIB10_CH1,
        p.PPIB10_CH2,
        p.PPIB10_CH3,
    );
    let rng_ptr = Box::into_raw(Box::new(cracen::Cracen::new_blocking(p.CRACEN)));
    let mem_ptr = Box::into_raw(Box::new(sdc::Mem::<SDC_MEM>::new()));
    let sdc = match build_sdc(cfg, sdc_p, unsafe { &mut *rng_ptr }, mpsl, unsafe {
        &mut *mem_ptr
    }) {
        Ok(s) => s,
        Err(_) => {
            log(cfg, c"[runtime-ble] SDC init failed; aborting load");
            unsafe {
                let _ = Box::from_raw(mem_ptr);
                let _ = Box::from_raw(rng_ptr);
                let _ = Box::from_raw(mpsl_ptr);
            }
            return;
        }
    };

    let res_ptr: *mut Resources = Box::into_raw(Box::new(HostResources::new()));
    let builder = trouble_host::new(sdc, unsafe { &mut *res_ptr })
        .set_random_address(device_address(cfg))
        .set_io_capabilities(io_capability_from_c(cfg.security_io_capability));
    #[cfg(feature = "l2cap")]
    let builder = if cfg.l2cap_psm != 0 {
        builder.register_l2cap_spsm(cfg.l2cap_psm)
    } else {
        builder
    };
    let stack = builder.build();

    log(cfg, c"[runtime-ble] loaded on heap");
    #[cfg(feature = "central")]
    if cfg.role == 1 {
        serve_central(mpsl, &stack, cfg);
    } else {
        serve_session(mpsl, &stack, cfg);
    }
    #[cfg(not(feature = "central"))]
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
