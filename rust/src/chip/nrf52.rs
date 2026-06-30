// SPDX-License-Identifier: Apache-2.0
//! nRF52 radio bring-up (features `nrf52840` / `nrf52833` / `nrf52832`).
//!
//! Same shape as `chip/nrf54l.rs`; only the MPSL/SDC peripherals, the RNG
//! source (the RNG peripheral instead of CRACEN) and the interrupt set differ
//! (RADIO / TIMER0 / RTC0 / EGU0_SWI0 / CLOCK_POWER). The C-callable shims below
//! are connected via IRQ_CONNECT in glue.c.
//!
//! Build-only / untested: no nRF52 hardware on the test rig. The peripheral and
//! interrupt wiring follows the upstream nrf-sdc nRF52 example.

use alloc::boxed::Box;
use core::ffi::c_int;

use embassy_nrf::interrupt::typelevel::{self, Binding, Handler};
use embassy_nrf::mode::Blocking;
use embassy_nrf::rng;
use nrf_sdc::mpsl::MultiprotocolServiceLayer;
use nrf_sdc::{self as sdc, mpsl};
use trouble_host::prelude::*;

#[cfg(feature = "central")]
use super::serve_central;
use super::{device_address, log, serve_session, Resources, EXT_ADV_DATA_MAX};
use crate::RuntimeCfg;

const L2CAP_TXQ: u8 = 4;
const L2CAP_RXQ: u8 = 4;

// SoftDevice Controller memory pool; the central role needs more.
#[cfg(not(feature = "central"))]
const SDC_MEM: usize = 16384;
#[cfg(feature = "central")]
const SDC_MEM: usize = 20480;

fn io_capability_from_c(capability: u8) -> IoCapabilities {
    match capability {
        1 => IoCapabilities::DisplayOnly,
        2 => IoCapabilities::DisplayYesNo,
        3 => IoCapabilities::KeyboardOnly,
        5 => IoCapabilities::KeyboardDisplay,
        _ => IoCapabilities::NoInputNoOutput,
    }
}

#[derive(Clone, Copy)]
pub(super) struct Irqs;
unsafe impl Binding<typelevel::EGU0_SWI0, mpsl::LowPrioInterruptHandler> for Irqs {}
unsafe impl Binding<typelevel::CLOCK_POWER, mpsl::ClockInterruptHandler> for Irqs {}
unsafe impl Binding<typelevel::RADIO, mpsl::HighPrioInterruptHandler> for Irqs {}
unsafe impl Binding<typelevel::TIMER0, mpsl::HighPrioInterruptHandler> for Irqs {}
unsafe impl Binding<typelevel::RTC0, mpsl::HighPrioInterruptHandler> for Irqs {}

#[no_mangle]
pub extern "C" fn runtime_irq_radio() {
    unsafe { <mpsl::HighPrioInterruptHandler as Handler<typelevel::RADIO>>::on_interrupt() }
}
#[no_mangle]
pub extern "C" fn runtime_irq_timer0() {
    unsafe { <mpsl::HighPrioInterruptHandler as Handler<typelevel::TIMER0>>::on_interrupt() }
}
#[no_mangle]
pub extern "C" fn runtime_irq_rtc0() {
    unsafe { <mpsl::HighPrioInterruptHandler as Handler<typelevel::RTC0>>::on_interrupt() }
}
#[no_mangle]
pub extern "C" fn runtime_irq_egu0_swi0() {
    unsafe { <mpsl::LowPrioInterruptHandler as Handler<typelevel::EGU0_SWI0>>::on_interrupt() }
}
#[no_mangle]
pub extern "C" fn runtime_irq_clock_power() {
    unsafe { <mpsl::ClockInterruptHandler as Handler<typelevel::CLOCK_POWER>>::on_interrupt() }
}

fn build_sdc<'d, const N: usize>(
    cfg: &RuntimeCfg,
    p: nrf_sdc::Peripherals<'d>,
    rng: &'d mut rng::Rng<'static, Blocking>,
    mpsl: &'d MultiprotocolServiceLayer,
    mem: &'d mut sdc::Mem<N>,
) -> Result<nrf_sdc::SoftdeviceController<'d>, nrf_sdc::Error> {
    let dis = cfg.sdc_disable;
    // A feature the app actually configures is kept regardless of the disable mask.
    let want_ext_adv = cfg.adv_extended != 0
        || cfg.periodic_adv != 0
        || (dis & crate::SDC_DISABLE_EXT_ADV) == 0;
    let want_periodic = cfg.periodic_adv != 0 || (dis & crate::SDC_DISABLE_PERIODIC_ADV) == 0;

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
    b = b.support_extended_feature_set();
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
    b.buffer_cfg(
        DefaultPacketPool::MTU as u16,
        DefaultPacketPool::MTU as u16,
        L2CAP_TXQ,
        L2CAP_RXQ,
    )?
    .build(p, rng, mpsl, mem)
}

pub(crate) fn run(cfg: Option<&'static RuntimeCfg>, _mode: c_int) {
    let cfg: &'static RuntimeCfg = match cfg {
        Some(c) => c,
        None => return,
    };
    let p = unsafe { embassy_nrf::Peripherals::steal() };

    let mpsl_p =
        mpsl::Peripherals::new(p.RTC0, p.TIMER0, p.TEMP, p.PPI_CH19, p.PPI_CH30, p.PPI_CH31);
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
        p.PPI_CH17, p.PPI_CH18, p.PPI_CH20, p.PPI_CH21, p.PPI_CH22, p.PPI_CH23, p.PPI_CH24,
        p.PPI_CH25, p.PPI_CH26, p.PPI_CH27, p.PPI_CH28, p.PPI_CH29,
    );
    let rng_ptr = Box::into_raw(Box::new(rng::Rng::new_blocking(p.RNG)));
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

    drop(stack);
    unsafe {
        let _ = Box::from_raw(res_ptr);
        let _ = Box::from_raw(mem_ptr);
        let _ = Box::from_raw(rng_ptr);
        let _ = Box::from_raw(mpsl_ptr);
    }
    log(cfg, c"[runtime-ble] unloaded; heap freed");
}
