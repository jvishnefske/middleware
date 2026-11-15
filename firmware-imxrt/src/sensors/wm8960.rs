//! The WM8960 audio codec (SC5): the register list that records from the
//! EVKB's onboard microphone.
//!
//! The WM8960 has 7-bit register addresses and 9-bit values, written over
//! I2C as one two-byte transfer to [`super::WM8960_ADDR`]:
//! `(reg << 1) | value[8]`, then `value[7:0]` ([`CodecWrite::bytes`]). Its
//! registers cannot be read back, so the plan writes whole values.
//!
//! [`record_plan`] follows the NXP SDK's `WM8960_Init` (`fsl_wm8960.c`)
//! with the EVKB examples' configuration (left input
//! `kWM8960_InputDifferentialMicInput3`, right input
//! `kWM8960_InputDifferentialMicInput2`, I2S, slave, 16-bit) and the
//! record route, in the SDK's order:
//!
//! | step | register | value | meaning |
//! |------|----------|-------|---------|
//! | 1 | `RESET` (0x0F) | 0x000 | all registers to defaults |
//! | 2 | `POWER1` (0x19) | 0x0FE | VMID 2×50 kΩ, VREF, AINL/R, ADCL/R, MICBIAS |
//! | 3 | `POWER2` (0x1A) | 0x000 | DACs, outputs and PLL off (the SDK's 0x1E0 powers playback) |
//! | 4 | `POWER3` (0x2F) | 0x030 | left and right input PGAs (LMIC, RMIC) |
//! | 5 | `IFACE2` (0x09) | 0x040 | ADCLRC pin as GPIO: ADC and DAC share DACLRC |
//! | 6 | `LINPATH` (0x20) | 0x1B8 | LINPUT1 − / LINPUT3 + to the PGA, +29 dB boost |
//! | 7 | `RINPATH` (0x21) | 0x178 | RINPUT1 − / RINPUT2 + to the PGA, +29 dB boost |
//! | 8 | `IFACE1` (0x07) | 0x002 | I2S, 16-bit, slave |
//! | 9 | `ADDCTL1` (0x17) | 0x0C0 | analogue bias for AVDD 3.3 V |
//! | 10 | `ADDCTL4` (0x30) | 0x040 | as the SDK (mic bias 0.9 × AVDD) |
//! | 11 | `BYPASS1` (0x2D) | 0x000 | no analogue bypass to the left output mixer |
//! | 12 | `BYPASS2` (0x2E) | 0x000 | no analogue bypass to the right output mixer |
//! | 13 | `LADC` (0x15) | 0x1C3 | left ADC volume 0 dB, update |
//! | 14 | `RADC` (0x16) | 0x1C3 | right ADC volume 0 dB, update |
//! | 15 | `LINVOL` (0x00) | 0x100 \| PGA | left PGA unmuted at the route's gain, update |
//! | 16 | `RINVOL` (0x01) | 0x100 \| PGA | right PGA likewise |
//! | 17 | `CLOCK1` (0x04) | per rate | ADC and DAC dividers from MCLK ([`clock1`]) |
//!
//! The EVKB's microphone is on the right input (RINPUT1 − / RINPUT2 +,
//! the SDK's `kWM8960_InputDifferentialMicInput2` for the right channel;
//! MicroPython's WM8960 notes: "a MIMXRT Dev board mic, which is connected
//! to the right input"), so SAI1 keeps the right word
//! ([`super::sai::MIC_WORD`]); both channels are powered, as the SDK does,
//! so moving the capture changes only the mask. The ADC's high-pass filter
//! stays on (the default), removing the microphone's DC offset. The
//! register fields were checked against the WM8960 register map: R0/R1
//! `IPVU`|`LINVOL`, R25 `VMIDSEL`=01 `VREF` `AINL/R` `ADCL/R` `MICB`, R47
//! `LMIC`/`RMIC`, R9 `ALRCGPIO`, R32/R33 `xMN1` `xMP3`/`xMP2` boost +29 dB
//! `xMIC2B`, R7 I2S 16-bit slave, R4 `ADCDIV`/`DACDIV`.
//!
//! The route's gain ([`pga_for_gain_db`]) is the input PGA's, above the
//! fixed +29 dB boost: 0 dB is code 0x17, 0.75 dB a step.

use super::AudioRate;

/// `LINVOL` (R0): left input PGA volume.
pub const LINVOL: u8 = 0x00;
/// `RINVOL` (R1): right input PGA volume.
pub const RINVOL: u8 = 0x01;
/// `CLOCK1` (R4): ADC/DAC sample rate dividers, SYSCLK divider and source.
pub const CLOCK1: u8 = 0x04;
/// `IFACE1` (R7): audio interface format, word length, master/slave.
pub const IFACE1: u8 = 0x07;
/// `IFACE2` (R9): ADCLRC/GPIO selection.
pub const IFACE2: u8 = 0x09;
/// `RESET` (R15): any write resets the codec.
pub const RESET: u8 = 0x0F;
/// `LADC` (R21): left ADC digital volume.
pub const LADC: u8 = 0x15;
/// `RADC` (R22): right ADC digital volume.
pub const RADC: u8 = 0x16;
/// `ADDCTL1` (R23): additional control 1 (bias, ADC data select).
pub const ADDCTL1: u8 = 0x17;
/// `POWER1` (R25): VMID, VREF, input PGAs, ADCs, mic bias.
pub const POWER1: u8 = 0x19;
/// `POWER2` (R26): DACs, outputs, PLL.
pub const POWER2: u8 = 0x1A;
/// `LINPATH` (R32): left input signal path to the PGA.
pub const LINPATH: u8 = 0x20;
/// `RINPATH` (R33): right input signal path to the PGA.
pub const RINPATH: u8 = 0x21;
/// `BYPASS1` (R45): left bypass to the output mixer.
pub const BYPASS1: u8 = 0x2D;
/// `BYPASS2` (R46): right bypass to the output mixer.
pub const BYPASS2: u8 = 0x2E;
/// `POWER3` (R47): input PGA and output mixer enables.
pub const POWER3: u8 = 0x2F;
/// `ADDCTL4` (R48): additional control 4 (GPIO, mic bias level).
pub const ADDCTL4: u8 = 0x30;

/// The input PGA's 0 dB code. The PGA runs from −17.25 dB (0) to
/// +30 dB ([`PGA_MAX`]) in 0.75 dB steps.
pub const PGA_0DB: u8 = 0x17;
/// The input PGA's highest code (+30 dB).
pub const PGA_MAX: u8 = 0x3F;

/// One codec register write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecWrite {
    /// Register address (7 bits).
    pub reg: u8,
    /// Value (9 bits).
    pub value: u16,
}

impl CodecWrite {
    /// The two I2C data bytes: `(reg << 1) | value[8]`, `value[7:0]`.
    /// Bits of `reg` above 7 and of `value` above 9 are dropped.
    pub const fn bytes(&self) -> [u8; 2] {
        [
            ((self.reg & 0x7F) << 1) | ((self.value >> 8) & 1) as u8,
            (self.value & 0xFF) as u8,
        ]
    }
}

const fn w(reg: u8, value: u16) -> CodecWrite {
    CodecWrite { reg, value }
}

/// `CLOCK1` for a rate: SYSCLK = MCLK ([`AudioRate::mclk_hz`], no PLL,
/// `SYSCLKDIV` 1) and the same `ADCDIV` and `DACDIV` (the two share
/// DACLRC), so that SYSCLK / (256 × divider) is the rate:
///
/// | rate | MCLK | MCLK / fs | divider | code |
/// |------|------|-----------|---------|------|
/// | 8 kHz | 6.144 MHz | 768 | 3 | 0b011 |
/// | 16 kHz | 6.144 MHz | 384 | 1.5 | 0b001 |
/// | 32 kHz | 12.288 MHz | 384 | 1.5 | 0b001 |
/// | 48 kHz | 12.288 MHz | 256 | 1 | 0b000 |
pub const fn clock1(rate: AudioRate) -> u16 {
    let code: u16 = adc_div_code(rate);
    (code << 6) | (code << 3)
}

/// The `ADCDIV` (and `DACDIV`) field for a rate.
const fn adc_div_code(rate: AudioRate) -> u16 {
    match rate {
        AudioRate::Hz8000 => 0b011,
        AudioRate::Hz16000 | AudioRate::Hz32000 => 0b001,
        AudioRate::Hz48000 => 0b000,
    }
}

/// The divider a `ADCDIV` code stands for, in halves (1 → 2, 1.5 → 3, …),
/// from the WM8960 datasheet.
#[cfg(test)]
const fn adc_div_halves(code: u16) -> u32 {
    match code {
        0b000 => 2,
        0b001 => 3,
        0b010 => 4,
        0b011 => 6,
        0b100 => 8,
        0b101 => 11,
        _ => 12,
    }
}

/// The input PGA code for a route gain of `db` dB (0..=30; more is
/// clamped): 0x17 + db × 4/3 rounded to the nearest 0.75 dB step, 0x3F at
/// +30 dB.
pub const fn pga_for_gain_db(db: u8) -> u8 {
    let db = if db > 30 { 30 } else { db } as u16;
    let steps = (db * 4 + 1) / 3;
    let code = PGA_0DB as u16 + steps;
    if code > PGA_MAX as u16 { PGA_MAX } else { code as u8 }
}

/// Number of writes in [`record_plan`].
pub const RECORD_PLAN_LEN: usize = 17;

/// The register writes that record from the onboard microphone at `rate`
/// with the input PGA at `pga` (clamped to [`PGA_MAX`]; [`PGA_0DB`] is
/// unity), in order (the table in the module documentation). Write them
/// once MCLK runs.
pub const fn record_plan(rate: AudioRate, pga: u8) -> [CodecWrite; RECORD_PLAN_LEN] {
    let pga = if pga > PGA_MAX { PGA_MAX } else { pga } as u16;
    [
        w(RESET, 0x000),
        w(POWER1, 0x0FE),
        w(POWER2, 0x000),
        w(POWER3, 0x030),
        w(IFACE2, 0x040),
        w(LINPATH, 0x1B8),
        w(RINPATH, 0x178),
        w(IFACE1, 0x002),
        w(ADDCTL1, 0x0C0),
        w(ADDCTL4, 0x040),
        w(BYPASS1, 0x000),
        w(BYPASS2, 0x000),
        w(LADC, 0x1C3),
        w(RADC, 0x1C3),
        w(LINVOL, 0x100 | pga),
        w(RINVOL, 0x100 | pga),
        w(CLOCK1, clock1(rate)),
    ]
}

/// The writes that stop recording: everything powered down (the codec
/// keeps its other registers until the next [`record_plan`] resets it).
pub const fn stop_plan() -> [CodecWrite; 3] {
    [w(POWER1, 0x000), w(POWER2, 0x000), w(POWER3, 0x000)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sc5_codec_writes_are_seven_bit_address_and_nine_bit_value() {
        assert_eq!(w(RESET, 0).bytes(), [0x1E, 0x00]);
        assert_eq!(w(LINPATH, 0x1B8).bytes(), [0x41, 0xB8]);
        assert_eq!(w(POWER1, 0x0FE).bytes(), [0x32, 0xFE]);
        assert_eq!(w(0x7F, 0x1FF).bytes(), [0xFF, 0xFF]);
        // Out-of-range bits are dropped, never shifted into the address.
        assert_eq!(w(0xFF, 0xFFFF).bytes(), [0xFF, 0xFF]);
        assert_eq!(w(0x80, 0xFE00).bytes(), [0x00, 0x00]);
    }

    #[test]
    fn sc5_record_plan_follows_the_sdk_init_for_the_evkb_microphone() {
        let p = record_plan(AudioRate::Hz16000, PGA_0DB);
        assert_eq!(p.len(), RECORD_PLAN_LEN);
        assert_eq!(p[0], w(RESET, 0));
        let get = |reg| p.iter().rev().find(|x| x.reg == reg).map(|x| x.value);
        assert_eq!(get(POWER1), Some(0x0FE));
        assert_eq!(get(POWER2), Some(0x000));
        assert_eq!(get(POWER3), Some(0x030));
        assert_eq!(get(IFACE1), Some(0x002)); // I2S, 16-bit, slave
        assert_eq!(get(IFACE2), Some(0x040));
        assert_eq!(get(LINPATH), Some(0x1B8));
        assert_eq!(get(RINPATH), Some(0x178));
        assert_eq!(get(LADC), Some(0x1C3));
        assert_eq!(get(LINVOL), Some(0x117)); // the SDK's value at 0 dB
        assert_eq!(get(RINVOL), Some(0x117));
        // The SDK's 16 kHz at 6.144 MHz: ADCDIV = DACDIV = 1.5.
        assert_eq!(get(CLOCK1), Some(0x048));
        // Clocks last, every value fits 9 bits.
        assert_eq!(p[RECORD_PLAN_LEN - 1].reg, CLOCK1);
        assert!(p.iter().all(|x| x.value <= 0x1FF && x.reg <= 0x37));
        // PGA gain is clamped and the mute bit never set.
        let loud = record_plan(AudioRate::Hz16000, 0xFF);
        assert_eq!(loud[14], w(LINVOL, 0x13F));
        assert_eq!(record_plan(AudioRate::Hz8000, 0)[15], w(RINVOL, 0x100));
        assert!(stop_plan().iter().all(|x| x.value == 0));
    }

    #[test]
    fn sc8_route_gain_maps_onto_the_pga_steps() {
        assert_eq!(pga_for_gain_db(0), PGA_0DB);
        assert_eq!(pga_for_gain_db(30), PGA_MAX, "+30 dB = 0x17 + 40 steps");
        assert_eq!(pga_for_gain_db(255), PGA_MAX);
        assert_eq!(pga_for_gain_db(3), PGA_0DB + 4);
        assert_eq!(pga_for_gain_db(1), PGA_0DB + 1, "1.33 steps rounds to 1");
        assert_eq!(pga_for_gain_db(2), PGA_0DB + 3, "2.67 steps rounds to 3");
        for db in 0..=30u8 {
            let code = pga_for_gain_db(db);
            let back = (f64::from(code) - f64::from(PGA_0DB)) * 0.75;
            assert!((back - f64::from(db)).abs() <= 0.375 + 1e-9, "{db} dB -> {code:#x}");
        }
    }

    #[test]
    fn sc5_clock1_divides_mclk_to_each_rate() {
        for rate in AudioRate::ALL {
            let c = clock1(rate);
            let adc = (c >> 6) & 0b111;
            let dac = (c >> 3) & 0b111;
            assert_eq!(adc, dac);
            assert_eq!(c & 0b111, 0, "SYSCLKDIV 1, SYSCLK from MCLK");
            // fs = MCLK / (256 × divider), divider in halves.
            let fs = rate.mclk_hz() * 2 / (256 * adc_div_halves(adc));
            assert_eq!(fs, rate.hz(), "{rate:?}");
        }
        assert_eq!(clock1(AudioRate::Hz8000), 0x0D8);
        assert_eq!(clock1(AudioRate::Hz48000), 0x000);
    }
}
