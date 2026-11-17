//! Pure decision logic behind the payload I2C, SPI and ADC (ABI 2
//! `spi_*`, ABI 3 `i2c_*` and `adc_read`) on the i.MX RT1050.
//!
//! The imperative shell writes the LPI2C1, LPSPI1 and ADC1 registers
//! through embassy-nxp's PAC; every value it writes and every decision it
//! makes on what it reads comes from here, so it is unit tested on the
//! host, as `rp_base::periph` is for the RP2040 and RP2350:
//!
//! * [`i2c`] — LPI2C1 (I2C bus 0, A4/A5): the master clock registers
//!   (`MCFGR1.PRESCALE`, `MCCR0`, `MCFGR3.PINLOW`), the `MTDR` command
//!   words of one write / read / write-then-read / address probe, the
//!   `MSR` error decoding and the time bound of a transfer.
//! * [`spi`] — LPSPI1 (SPI bus 0, D10–D13): the clock and delay registers
//!   (`CCR`, `TCR`), the clock actually achieved (never above the
//!   request), and the bounded FIFO pump of one full-duplex transfer.
//! * [`adc`] — ADC1 (A0–A5): `CFG` and `GC` for 12-bit averaged
//!   conversions, the calibration plan, `HC0` for a board pin and the
//!   reading the payload gets.
//!
//! Pin checks (which board pins the buses hold, which pin an ADC read
//! names) are the board profile's ([`crate::chip`]); the transfer state
//! machine of an SPI transfer (`Idle → InFlight → Done`, read back once)
//! is esp-base's `esp_base::periph::dma::DmaState`, as on the RP base
//! firmware.
//!
//! # Clock roots
//!
//! The register values depend on the peripheral clock roots the shell sets
//! in the CCM; each plan takes the root's frequency as a parameter and the
//! constants below name the roots the shell is expected to configure:
//!
//! | block  | root                                                   | Hz                          |
//! |--------|--------------------------------------------------------|-----------------------------|
//! | LPI2C  | 24 MHz oscillator (`CSCDR2.LPI2C_CLK_SEL` = OSC, `LPI2C_CLK_PODF` = /1) | [`i2c::LPI2C_CLK_HZ`] |
//! | LPSPI  | PLL2 528 MHz / 8 (`CBCMR.LPSPI_CLK_SEL` = PLL2, `LPSPI_PODF` = /8)      | [`spi::LPSPI_CLK_HZ`] |
//! | ADC1   | its own asynchronous clock ADACK (`CFG.ADICLK` = ADACK)                 | [`adc::ADACK_HZ`]     |
//!
//! The LPI2C runs from the oscillator, like the LPUART1 console, so it does
//! not depend on which PLLs the boot chain left running (the alternative,
//! PLL3 / 8 = 60 MHz, is [`i2c::LPI2C_CLK_PLL3_HZ`] and works with the same
//! plans). The LPSPI has no oscillator option; PLL2 is the one PLL the
//! boot header's DCD always turns on (it clocks the SEMC), and PLL3's PFD0
//! belongs to the FlexSPI.
//!
//! # Bounded waits
//!
//! Every wait on a status flag is bounded. A transfer has a time bound
//! (`timeout_us`, which the shell may check against embassy-time) and a
//! poll [`Budget`] derived from it: the shell spends one poll per status
//! read and gives up when the budget is gone, so even a wedged peripheral
//! or a stopped timer cannot hang the firmware.

pub mod adc;
pub mod i2c;
pub mod spi;

/// The most status-register polls the shell can make in one µs: an upper
/// bound (the Cortex-M7 at 600 MHz needs well over ten core cycles for a
/// read on the peripheral bus), so a [`Budget::from_us`] budget of `us`
/// microseconds never runs out before `us` µs have passed.
pub const POLLS_PER_US: u32 = 64;

/// A bounded number of polls of a status register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    left: u32,
}

impl Budget {
    /// A budget of `polls` polls.
    pub const fn new(polls: u32) -> Self {
        Budget { left: polls }
    }

    /// A budget lasting at least `us` µs ([`POLLS_PER_US`] polls per µs,
    /// saturating at `u32::MAX` polls).
    pub const fn from_us(us: u64) -> Self {
        let polls = us.saturating_mul(POLLS_PER_US as u64);
        Budget {
            left: if polls > u32::MAX as u64 {
                u32::MAX
            } else {
                polls as u32
            },
        }
    }

    /// Spends one poll: `true` if one was left, `false` once the budget is
    /// gone (and on every later call).
    pub fn spend(&mut self) -> bool {
        match self.left.checked_sub(1) {
            Some(left) => {
                self.left = left;
                true
            }
            None => false,
        }
    }

    /// Polls left.
    pub const fn left(&self) -> u32 {
        self.left
    }

    /// Whether no poll is left.
    pub const fn exhausted(&self) -> bool {
        self.left == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periph_budgets_are_bounded_and_saturate() {
        let mut b = Budget::new(2);
        assert!(b.spend() && b.spend());
        assert!(!b.spend() && !b.spend() && b.exhausted());
        assert_eq!(Budget::from_us(10).left(), 10 * POLLS_PER_US);
        assert_eq!(Budget::from_us(u64::MAX).left(), u32::MAX);
        assert!(Budget::from_us(0).exhausted());
    }
}
