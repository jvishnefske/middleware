//! ADC1 decisions (`adc_read` on A0–A5): the configuration and control
//! registers, the calibration plan, the channel of a board pin, the
//! bounded wait for a conversion and the reading the payload gets.
//!
//! # Configuration
//!
//! ADC1 converts in 12-bit mode (`CFG.MODE` = 2) against VREFH / VREFL
//! (`REFSEL` 0, 3.3 V on the EVKB), with a software trigger (`ADTRG` 0: a
//! write to `HC0` starts one conversion), the long sample time (`ADLSMP`
//! 1, `ADSTS` 0: twelve ADC clocks, as the Arduino header pins are not
//! buffered) and hardware averaging (`GC.AVGE` 1, `CFG.AVGS`):
//! [`Averages::Four`] for reads ([`RUN`]), [`Averages::ThirtyTwo`] while
//! calibrating ([`CALIBRATION`]), as the reference manual recommends.
//!
//! The clock is the converter's own asynchronous clock ADACK (`ADICLK` 3,
//! `GC.ADACKEN` 1, `ADHSC` 0: [`ADACK_HZ`] nominal), so ADC1 needs no
//! clock root of the CCM beyond its clock gate. [`Config::new`] also
//! accepts the IPG clock, divided (`ADICLK` 0 with `ADIV` /1../8, then
//! `ADICLK` 1 with /8 = /16) to at most [`ADC_CLK_MAX_HZ`].
//!
//! # Calibration
//!
//! Once after reset, before the first read ([`calibration_writes`], then
//! [`calibration_poll`] and [`calibration_result`]):
//!
//! 1. `CFG` = [`CALIBRATION`]'s (software trigger, 32 averages);
//! 2. `GC` = its control bits without `CAL`;
//! 3. `GS` = `CALF` (write one to clear an old failure);
//! 4. `GC` = with `CAL`: the calibration starts;
//! 5. poll `GC` and `GS` until `CAL` clears, failing on `CALF` or when the
//!    [`calibration_budget`] ([`CALIBRATION_TIMEOUT_US`]) runs out;
//! 6. it succeeded if `CALF` is clear and `HS.COCO0` is set; read `R0` to
//!    clear `COCO0`;
//! 7. `CFG` and `GC` = [`RUN`]'s.
//!
//! A failed calibration leaves `adc_read` failing with `Transport` until
//! the shell calibrates again.
//!
//! # A read
//!
//! [`hc0_for_pin`] gives the `HC0` word for a board pin, through the board
//! profile (A0 is ADC1 input 15, A1 input 0, A2 9, A3 10, A4 6, A5 5;
//! `InvalidArgument` for a pin the board lacks, `Unsupported` for one
//! without an analog input). Writing it starts the conversion; the shell
//! polls `HS` with [`conversion_poll`] (a [`Config::conversion_budget`])
//! and then reads `R0`, whose low twelve bits are the reading
//! ([`reading`], `0..=ADC_MAX`, what `adc_read` returns). A conversion that
//! never completes is `Transport`, as on the RP base firmware.

use super::Budget;
use crate::chip::Profile;
use board_pins::imxrt::Adc1Channel;
use middleware::wasm_abi::AbiError;
use middleware::wasm_abi::periph::ADC_MAX;

/// Nominal ADACK frequency with `CFG.ADHSC` 0.
pub const ADACK_HZ: u32 = 10_000_000;

/// Fastest ADC clock [`Config::new`] derives from the IPG clock (a
/// conservative bound for 12-bit conversions).
pub const ADC_CLK_MAX_HZ: u32 = 20_000_000;

/// ADC clocks one sample takes at most: the long sample (12) and the
/// 12-bit successive approximation with its overhead, rounded up.
pub const CYCLES_PER_SAMPLE: u32 = 40;

/// Slack added to every conversion's time bound, in µs (ADACK start-up
/// included).
pub const CONVERSION_MARGIN_US: u32 = 100;

/// Time bound of a calibration, in µs: far above the few thousand ADC
/// clocks it takes with 32 averages.
pub const CALIBRATION_TIMEOUT_US: u32 = 50_000;

/// `HC0` with conversions disabled (`ADCH` = 0x1F).
pub const HC0_IDLE: u32 = 0x1F;

/// `GC` bits.
pub mod gc {
    /// Asynchronous clock output enable (ADACK runs between conversions).
    pub const ADACKEN: u32 = 1 << 0;
    /// Hardware average enable.
    pub const AVGE: u32 = 1 << 5;
    /// Continuous conversion.
    pub const ADCO: u32 = 1 << 6;
    /// Calibration: set to start, cleared by the converter when done.
    pub const CAL: u32 = 1 << 7;
}

/// `GS` bits.
pub mod gs {
    /// Conversion active.
    pub const ADACT: u32 = 1 << 0;
    /// Calibration failed (write one to clear).
    pub const CALF: u32 = 1 << 1;
}

/// `HS.COCO0`: conversion of `HC0` complete (cleared by reading `R0`).
pub const HS_COCO0: u32 = 1 << 0;

/// The ADC clock source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdcClock {
    /// The asynchronous clock ADACK ([`ADACK_HZ`]).
    Adack,
    /// The IPG clock at this frequency, divided as needed.
    Ipg(u32),
}

/// Hardware averaging (`CFG.AVGS`, with `GC.AVGE` set).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Averages {
    /// 4 samples.
    Four,
    /// 8 samples.
    Eight,
    /// 16 samples.
    Sixteen,
    /// 32 samples.
    ThirtyTwo,
}

impl Averages {
    /// The `AVGS` field value.
    pub const fn avgs(self) -> u32 {
        match self {
            Averages::Four => 0,
            Averages::Eight => 1,
            Averages::Sixteen => 2,
            Averages::ThirtyTwo => 3,
        }
    }

    /// Samples per conversion.
    pub const fn samples(self) -> u32 {
        4 << self.avgs()
    }
}

/// An ADC1 configuration: the `CFG` and `GC` words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// `CFG.ADICLK`: 0 IPG, 1 IPG / 2, 3 ADACK.
    pub adiclk: u8,
    /// `CFG.ADIV`: divide by `2^adiv`, 0..=3.
    pub adiv: u8,
    /// Hardware averaging.
    pub averages: Averages,
    /// The resulting ADC clock, in Hz.
    pub adck_hz: u32,
}

/// The configuration of reads: ADACK, 4 averages.
pub const RUN: Config = Config::adack(Averages::Four);

/// The configuration of the calibration: ADACK, 32 averages.
pub const CALIBRATION: Config = Config::adack(Averages::ThirtyTwo);

impl Config {
    /// ADACK with `averages`.
    pub const fn adack(averages: Averages) -> Self {
        Config {
            adiclk: 3,
            adiv: 0,
            averages,
            adck_hz: ADACK_HZ,
        }
    }

    /// The configuration for `clock`: `InvalidArgument` for a zero IPG
    /// clock, `Unsupported` for one too fast to divide to
    /// [`ADC_CLK_MAX_HZ`].
    pub fn new(clock: AdcClock, averages: Averages) -> Result<Self, AbiError> {
        let hz = match clock {
            AdcClock::Adack => return Ok(Config::adack(averages)),
            AdcClock::Ipg(0) => return Err(AbiError::InvalidArgument),
            AdcClock::Ipg(hz) => hz,
        };
        // (ADICLK, ADIV, total divisor), fastest first.
        const DIVIDERS: [(u8, u8, u32); 5] =
            [(0, 0, 1), (0, 1, 2), (0, 2, 4), (0, 3, 8), (1, 3, 16)];
        DIVIDERS
            .iter()
            .find(|&&(_, _, d)| hz / d <= ADC_CLK_MAX_HZ)
            .map(|&(adiclk, adiv, d)| Config {
                adiclk,
                adiv,
                averages,
                adck_hz: hz / d,
            })
            .ok_or(AbiError::Unsupported)
    }

    /// `CFG`: `ADICLK` `[1:0]`, `MODE` 12-bit `[3:2]`, `ADLSMP` `[4]`, `ADIV`
    /// `[6:5]`, `AVGS` `[15:14]`; software trigger, VREFH / VREFL, `ADSTS` 0,
    /// normal speed and power, no overwrite.
    pub const fn cfg(&self) -> u32 {
        (self.adiclk as u32 & 0x3)
            | 0b10 << 2
            | 1 << 4
            | (self.adiv as u32 & 0x3) << 5
            | self.averages.avgs() << 14
    }

    /// `GC`: hardware averaging, ADACK enabled when it is the clock, and
    /// `CAL` when `calibrate`.
    pub const fn gc(&self, calibrate: bool) -> u32 {
        let adacken = if self.adiclk == 3 { gc::ADACKEN } else { 0 };
        let cal = if calibrate { gc::CAL } else { 0 };
        gc::AVGE | adacken | cal
    }

    /// Time bound of one conversion, in µs: four times its samples at the
    /// nominal clock plus [`CONVERSION_MARGIN_US`].
    pub const fn conversion_timeout_us(&self) -> u32 {
        let cycles = self.averages.samples() as u64 * CYCLES_PER_SAMPLE as u64;
        let adck = if self.adck_hz == 0 {
            1
        } else {
            self.adck_hz as u64
        };
        let us = (4 * cycles * 1_000_000).div_ceil(adck) + CONVERSION_MARGIN_US as u64;
        if us > u32::MAX as u64 {
            u32::MAX
        } else {
            us as u32
        }
    }

    /// The poll budget of one conversion.
    pub const fn conversion_budget(&self) -> Budget {
        Budget::from_us(self.conversion_timeout_us() as u64)
    }
}

/// An ADC1 register the calibration writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reg {
    /// `CFG`.
    Cfg,
    /// `GC`.
    Gc,
    /// `GS`.
    Gs,
}

/// Steps 1–4 of the calibration (module docs), in order: the writes that
/// start it with configuration `cal`.
pub const fn calibration_writes(cal: &Config) -> [(Reg, u32); 4] {
    [
        (Reg::Cfg, cal.cfg()),
        (Reg::Gc, cal.gc(false)),
        (Reg::Gs, gs::CALF),
        (Reg::Gc, cal.gc(true)),
    ]
}

/// The poll budget of a calibration ([`CALIBRATION_TIMEOUT_US`]).
pub const fn calibration_budget() -> Budget {
    Budget::from_us(CALIBRATION_TIMEOUT_US as u64)
}

/// One bounded calibration wait step on `GC` and `GS`: `Transport` on
/// `CALF` or once `budget` is gone, `Ok(true)` when `CAL` has cleared,
/// else `Ok(false)` (one poll spent).
pub fn calibration_poll(gc: u32, gs: u32, budget: &mut Budget) -> Result<bool, AbiError> {
    if gs & gs::CALF != 0 {
        return Err(AbiError::Transport);
    }
    if gc & gc::CAL == 0 {
        return Ok(true);
    }
    if budget.spend() {
        Ok(false)
    } else {
        Err(AbiError::Transport)
    }
}

/// Whether a finished calibration succeeded: `CALF` clear and `COCO0` set
/// (`Transport` otherwise).
pub const fn calibration_result(gs: u32, hs: u32) -> Result<(), AbiError> {
    if gs & gs::CALF != 0 || hs & HS_COCO0 == 0 {
        Err(AbiError::Transport)
    } else {
        Ok(())
    }
}

/// The `HC0` word converting `channel` (interrupt off).
pub const fn hc0(channel: Adc1Channel) -> u32 {
    channel.number() as u32 & 0x1F
}

/// The `HC0` word for board `pin` on `profile` (its ADC1 input), with the
/// errors of [`Profile::adc1_input`].
pub fn hc0_for_pin(profile: &Profile, pin: u8) -> Result<u32, AbiError> {
    profile.adc1_input(pin).map(hc0)
}

/// One bounded conversion wait step on `HS`: `Ok(true)` when `COCO0` is
/// set, else `Ok(false)` (one poll spent), `Transport` once `budget` is
/// gone.
pub fn conversion_poll(hs: u32, budget: &mut Budget) -> Result<bool, AbiError> {
    if hs & HS_COCO0 != 0 {
        return Ok(true);
    }
    if budget.spend() {
        Ok(false)
    } else {
        Err(AbiError::Transport)
    }
}

/// The reading in an `R0` value: its 12-bit `CDATA`, `0..=ADC_MAX`.
pub const fn reading(r0: u32) -> u16 {
    (r0 & ADC_MAX as u32) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chip::RT1050;

    #[test]
    fn rt16_cfg_is_12_bit_averaged_on_adack() {
        // ADICLK 3, MODE 2, ADLSMP, ADIV 0, AVGS 0.
        assert_eq!(RUN.cfg(), 0b11 | 0b10 << 2 | 1 << 4);
        assert_eq!(RUN.gc(false), gc::AVGE | gc::ADACKEN);
        assert_eq!(CALIBRATION.cfg(), RUN.cfg() | 3 << 14);
        assert_eq!(CALIBRATION.gc(true), gc::AVGE | gc::ADACKEN | gc::CAL);
        // Software trigger (ADTRG, bit 13) and VREFH (REFSEL [12:11]).
        assert_eq!(RUN.cfg() & (1 << 13 | 0b11 << 11), 0);
        assert_eq!(Config::new(AdcClock::Adack, Averages::Four), Ok(RUN));
        assert_eq!(
            [
                Averages::Four,
                Averages::Eight,
                Averages::Sixteen,
                Averages::ThirtyTwo
            ]
            .map(Averages::samples),
            [4, 8, 16, 32]
        );
    }

    #[test]
    fn rt16_ipg_clocks_are_divided_to_the_limit() {
        let c = Config::new(AdcClock::Ipg(150_000_000), Averages::Four).unwrap();
        assert_eq!((c.adiclk, c.adiv, c.adck_hz), (0, 3, 18_750_000));
        assert_eq!(c.cfg() & (0b11 << 5 | 0b11), 3 << 5, "ADIV 8, ADICLK IPG");
        assert_eq!(c.gc(false), gc::AVGE, "no ADACK");
        let c = Config::new(AdcClock::Ipg(300_000_000), Averages::Four).unwrap();
        assert_eq!((c.adiclk, c.adiv, c.adck_hz), (1, 3, 18_750_000));
        let c = Config::new(AdcClock::Ipg(20_000_000), Averages::Four).unwrap();
        assert_eq!((c.adiclk, c.adiv), (0, 0));
        assert_eq!(
            Config::new(AdcClock::Ipg(400_000_000), Averages::Four),
            Err(AbiError::Unsupported)
        );
        assert_eq!(
            Config::new(AdcClock::Ipg(0), Averages::Four),
            Err(AbiError::InvalidArgument)
        );
    }

    #[test]
    fn rt16_calibration_plan_is_bounded() {
        assert_eq!(
            calibration_writes(&CALIBRATION),
            [
                (Reg::Cfg, CALIBRATION.cfg()),
                (Reg::Gc, gc::AVGE | gc::ADACKEN),
                (Reg::Gs, gs::CALF),
                (Reg::Gc, gc::AVGE | gc::ADACKEN | gc::CAL)
            ]
        );
        // Runs, then CAL clears: done.
        let mut b = Budget::new(3);
        assert_eq!(calibration_poll(gc::CAL, 0, &mut b), Ok(false));
        assert_eq!(calibration_poll(0, gs::ADACT, &mut b), Ok(true));
        assert_eq!(calibration_result(0, HS_COCO0), Ok(()));
        // CALF fails at once; a CAL bit that never clears ends with the budget.
        assert_eq!(
            calibration_poll(gc::CAL, gs::CALF, &mut b),
            Err(AbiError::Transport)
        );
        let mut b = Budget::new(2);
        let mut polls = 0;
        let r = loop {
            polls += 1;
            match calibration_poll(gc::CAL, 0, &mut b) {
                Ok(false) => continue,
                other => break other,
            }
        };
        assert_eq!((r, polls), (Err(AbiError::Transport), 3));
        // Finished without COCO0, or with CALF: failed.
        assert_eq!(calibration_result(0, 0), Err(AbiError::Transport));
        assert_eq!(
            calibration_result(gs::CALF, HS_COCO0),
            Err(AbiError::Transport)
        );
        assert_eq!(
            calibration_budget(),
            Budget::from_us(u64::from(CALIBRATION_TIMEOUT_US))
        );
    }

    #[test]
    fn rt16_a0_to_a5_convert_their_adc1_inputs() {
        let inputs = [15, 0, 9, 10, 6, 5];
        for (n, input) in inputs.into_iter().enumerate() {
            assert_eq!(hc0_for_pin(&RT1050, 16 + n as u8), Ok(input), "A{n}");
        }
        assert_eq!(hc0_for_pin(&RT1050, 13), Err(AbiError::Unsupported));
        assert_eq!(hc0_for_pin(&RT1050, 40), Err(AbiError::InvalidArgument));
        assert_eq!(
            hc0_for_pin(&RT1050, u8::MAX),
            Err(AbiError::InvalidArgument)
        );
        assert_ne!(HC0_IDLE, 15);
    }

    #[test]
    fn rt16_conversions_are_bounded_and_scaled() {
        // 4 × 40 clocks at 10 MHz: 16 µs, bound 4 × 16 + 100.
        assert_eq!(RUN.conversion_timeout_us(), 164);
        assert!(CALIBRATION.conversion_timeout_us() > RUN.conversion_timeout_us());
        let slow = Config { adck_hz: 0, ..RUN };
        assert!(slow.conversion_timeout_us() > 0, "a zero clock is clamped");
        let mut b = RUN.conversion_budget();
        assert_eq!(conversion_poll(0, &mut b), Ok(false));
        assert_eq!(conversion_poll(HS_COCO0, &mut b), Ok(true));
        let mut b = Budget::new(0);
        assert_eq!(conversion_poll(0, &mut b), Err(AbiError::Transport));
        assert_eq!(reading(0), 0);
        assert_eq!(reading(4095), ADC_MAX);
        assert_eq!(reading(0xFFFF_F123), 0x123, "only CDATA");
        assert_eq!(
            middleware::wasm_abi::periph::adc_result(reading(u32::MAX)),
            4095
        );
    }
}
