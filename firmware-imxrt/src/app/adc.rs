//! ADC1 on the Arduino A0–A5 pins (`wasm` only): 12-bit, averaged, on its
//! own asynchronous clock (ADACK), calibrated once at the first read.
//! Every step is [`imxrt_base::periph::adc`]'s bounded plan; a conversion
//! that does not complete answers `Transport`.

use embassy_nxp::pac;
use imxrt_base::chip::Profile;
use imxrt_base::periph::adc::{self, CALIBRATION, RUN, Reg};
use middleware::wasm_abi::AbiError;

/// ADC1.
///
/// The HAL has no ADC1 singleton on this chip; the converter is
/// constructed once, in the shell's boot, and only through the PAC.
pub struct Adc {
    calibrated: bool,
}

impl Adc {
    /// The converter, its clock gate (CCGR1 CG8) on.
    pub fn new() -> Self {
        pac::CCM.ccgr1().modify(|w| w.set_cg8(0b11));
        Adc { calibrated: false }
    }

    fn write(reg: Reg, v: u32) {
        let a = pac::ADC1;
        match reg {
            Reg::Cfg => a.cfg().write_value(pac::adc::regs::Cfg(v)),
            Reg::Gc => a.gc().write_value(pac::adc::regs::Gc(v)),
            Reg::Gs => a.gs().write_value(pac::adc::regs::Gs(v)),
        }
    }

    fn calibrate(&mut self) -> Result<(), AbiError> {
        let a = pac::ADC1;
        for (reg, v) in adc::calibration_writes(&CALIBRATION) {
            Self::write(reg, v);
        }
        let mut budget = adc::calibration_budget();
        while !adc::calibration_poll(a.gc().read().0, a.gs().read().0, &mut budget)? {}
        adc::calibration_result(a.gs().read().0, a.hs().read().0)?;
        let _ = a.r(0).read();
        Self::write(Reg::Cfg, RUN.cfg());
        Self::write(Reg::Gc, RUN.gc(false));
        self.calibrated = true;
        Ok(())
    }

    /// One conversion of board `pin`'s ADC1 input.
    pub fn read(&mut self, profile: &Profile, pin: u8) -> Result<u16, AbiError> {
        let hc0 = adc::hc0_for_pin(profile, pin)?;
        if !self.calibrated {
            self.calibrate()?;
        }
        let a = pac::ADC1;
        a.hc(0).write_value(hc0);
        let mut budget = RUN.conversion_budget();
        while !adc::conversion_poll(a.hs().read().0, &mut budget)? {}
        Ok(adc::reading(a.r(0).read()))
    }
}
