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
use embassy_nrf::peripherals::RNG;
use embassy_nrf::rng;
use nrf_sdc::mpsl::MultiprotocolServiceLayer;
use nrf_sdc::{self as sdc, mpsl};
use trouble_host::prelude::*;

use super::{device_address, log, serve_session, Resources};
#[cfg(feature = "central")]
use super::serve_central;
use crate::RuntimeCfg;

const L2CAP_TXQ: u8 = 4;
const L2CAP_RXQ: u8 = 4;

// SoftDevice Controller memory pool; the central role needs more.
#[cfg(not(feature = "central"))]
const SDC_MEM: usize = 8192;
#[cfg(feature = "central")]
const SDC_MEM: usize = 12288;

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
    p: nrf_sdc::Peripherals<'d>,
    rng: &'d mut rng::Rng<'static, Blocking>,
    mpsl: &'d MultiprotocolServiceLayer,
    mem: &'d mut sdc::Mem<N>,
) -> Result<nrf_sdc::SoftdeviceController<'d>, nrf_sdc::Error> {
    let b = sdc::Builder::new()?
        .support_adv()
        .support_peripheral()
        .support_le_2m_phy()
        .support_phy_update_peripheral()
        .support_dle_peripheral();
    #[cfg(feature = "central")]
    let b = b
        .support_scan()
        .support_central()
        .support_phy_update_central()
        .support_dle_central();
    let b = b.peripheral_count(1)?;
    #[cfg(feature = "central")]
    let b = b.central_count(1)?;
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

    let mpsl_p = mpsl::Peripherals::new(p.RTC0, p.TIMER0, p.TEMP, p.PPI_CH19, p.PPI_CH30, p.PPI_CH31);
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
        p.PPI_CH17, p.PPI_CH18, p.PPI_CH20, p.PPI_CH21, p.PPI_CH22, p.PPI_CH23, p.PPI_CH24,
        p.PPI_CH25, p.PPI_CH26, p.PPI_CH27, p.PPI_CH28, p.PPI_CH29,
    );
    let rng_ptr = Box::into_raw(Box::new(rng::Rng::new_blocking(p.RNG)));
    let mem_ptr = Box::into_raw(Box::new(sdc::Mem::<SDC_MEM>::new()));
    let sdc = build_sdc(sdc_p, unsafe { &mut *rng_ptr }, mpsl, unsafe { &mut *mem_ptr }).unwrap();

    let res_ptr: *mut Resources = Box::into_raw(Box::new(HostResources::new()));
    let builder = trouble_host::new(sdc, unsafe { &mut *res_ptr })
        .set_random_address(device_address(cfg))
        .set_io_capabilities(IoCapabilities::NoInputNoOutput);
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
