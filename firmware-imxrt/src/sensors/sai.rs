//! SAI1 and its clocks for the microphone (SC5): the audio PLL, the SAI1
//! root clock and MCLK, and the transmitter and receiver register values
//! for each [`AudioRate`].
//!
//! Clocks ([`clock_plan`]). The audio PLL (PLL4) runs at 786.432 MHz =
//! 24 MHz × (32 + 768/1000), post divider 1 (`MISC2.AUDIO_DIV` 1 too).
//! SAI1's root clock takes PLL4 (`CSCMR1.SAI1_CLK_SEL` = 2) through
//! `CS1CDR.SAI1_CLK_PRED` and `SAI1_CLK_PODF`: ÷128 for 6.144 MHz, ÷64 for
//! 12.288 MHz ([`AudioRate::mclk_hz`]). The root clock leaves on
//! SAI1_MCLK (GPIO_AD_B1_09) as the codec's MCLK (`IOMUXC_GPR1`:
//! `SAI1_MCLK1_SEL` = 0, `SAI1_MCLK_DIR` = 1), and is the SAI's own bit
//! clock source (`MSEL` = 1, MCLK1).
//!
//! Frames ([`sai_plan`]): I2S, the RT1050 the clock master and the codec
//! the slave. The EVKB wires SAI1's *transmit* bit clock and frame sync
//! (GPIO_AD_B1_14/15) to the codec, so the transmitter generates the
//! clocks and the receiver runs synchronous to it (`RCR2.SYNC` = 1),
//! receiving on RX_DATA00 (GPIO_AD_B1_12). Each frame is two 16-bit words
//! (left, right), MSB first, frame sync 16 bit clocks wide, active low,
//! one bit early: the bit clock is 32 × fs, so
//! `DIV` = MCLK / (32 × fs) / 2 − 1:
//!
//! | rate | MCLK | bit clock | `DIV` |
//! |------|------|-----------|-------|
//! | 8 kHz | 6.144 MHz | 256 kHz | 11 |
//! | 16 kHz | 6.144 MHz | 512 kHz | 5 |
//! | 32 kHz | 12.288 MHz | 1.024 MHz | 5 |
//! | 48 kHz | 12.288 MHz | 1.536 MHz | 3 |
//!
//! The receiver masks the right word (`RMR` = 0b10), so each FIFO entry
//! is one microphone sample ([`sample_from_fifo`]) and the FIFO request
//! interrupt fires at [`RX_WATERMARK`] samples.
//!
//! The shell's order: the clocks; `TCSR`/`RCSR` software and FIFO reset
//! ([`CSR_RESET`]), then 0; the `TCR*`/`RCR*`/`RMR` values; then
//! [`RCSR_ENABLE`] and [`TCSR_ENABLE`] (the transmitter must run for the
//! synchronous receiver's clocks; it has no data channel enabled).

use super::AudioRate;

/// The audio PLL (PLL4) and SAI1 root clock configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockPlan {
    /// `PLL_AUDIO.DIV_SELECT` (27..=54): the integer loop divider.
    pub pll_div_select: u8,
    /// `PLL_AUDIO_NUM`: the fractional loop divider's numerator.
    pub pll_num: u32,
    /// `PLL_AUDIO_DENOM`: its denominator.
    pub pll_denom: u32,
    /// `PLL_AUDIO.POST_DIV_SELECT` field (2 = ÷1).
    pub pll_post_div_field: u8,
    /// `CSCMR1.SAI1_CLK_SEL` field (2 = PLL4).
    pub sai1_clk_sel: u8,
    /// `CS1CDR.SAI1_CLK_PRED` field (divide by field + 1, 0..=7).
    pub sai1_clk_pred: u8,
    /// `CS1CDR.SAI1_CLK_PODF` field (divide by field + 1, 0..=63).
    pub sai1_clk_podf: u8,
}

impl ClockPlan {
    /// The PLL4 output in Hz (24 MHz reference).
    pub const fn pll_hz(&self) -> u64 {
        if self.pll_denom == 0 {
            return 0;
        }
        let post = match self.pll_post_div_field {
            0 => 4,
            1 => 2,
            _ => 1,
        };
        24_000_000 * self.pll_div_select as u64 / post
            + 24_000_000 * self.pll_num as u64 / self.pll_denom as u64 / post
    }

    /// The SAI1 root clock (MCLK) in Hz.
    pub const fn mclk_hz(&self) -> u64 {
        self.pll_hz() / (self.sai1_clk_pred as u64 + 1) / (self.sai1_clk_podf as u64 + 1)
    }
}

/// The clock configuration for a rate.
pub const fn clock_plan(rate: AudioRate) -> ClockPlan {
    ClockPlan {
        pll_div_select: 32,
        pll_num: 768,
        pll_denom: 1000,
        pll_post_div_field: 2,
        sai1_clk_sel: 2,
        sai1_clk_pred: match rate.mclk_hz() {
            6_144_000 => 1,
            _ => 0,
        },
        sai1_clk_podf: 63,
    }
}

/// Words a FIFO request waits for (`RCR1.RFW` + 1; the FIFO holds 32).
pub const RX_WATERMARK: u32 = 16;

/// `xCSR` software reset (`SR`) and FIFO reset (`FR`).
pub const CSR_RESET: u32 = (1 << 24) | (1 << 25);
/// `TCSR`: transmitter enable (`TE`) and bit clock enable (`BCE`).
pub const TCSR_ENABLE: u32 = (1 << 31) | (1 << 28);
/// `RCSR`: receiver enable (`RE`), bit clock enable (`BCE`), FIFO
/// request (`FRIE`) and FIFO error (`FEIE`) interrupts.
pub const RCSR_ENABLE: u32 = (1 << 31) | (1 << 28) | (1 << 10) | (1 << 8);
/// `xCSR.FEF`: FIFO error (receiver overflow); write 1 to clear.
pub const CSR_FEF: u32 = 1 << 18;

/// The SAI1 register values for a rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaiPlan {
    /// `TCR2`: asynchronous, bit clock polarity, MCLK1, master, `DIV`.
    pub tcr2: u32,
    /// `TCR3`: no transmit data channel.
    pub tcr3: u32,
    /// `TCR4`: frame shape (also generates the frame sync).
    pub tcr4: u32,
    /// `TCR5`: word widths.
    pub tcr5: u32,
    /// `RCR1`: receive FIFO watermark.
    pub rcr1: u32,
    /// `RCR2`: synchronous to the transmitter, as `TCR2` otherwise.
    pub rcr2: u32,
    /// `RCR3`: receive data channel 0 enabled.
    pub rcr3: u32,
    /// `RCR4`: frame shape, as `TCR4`.
    pub rcr4: u32,
    /// `RCR5`: word widths, as `TCR5`.
    pub rcr5: u32,
    /// `RMR`: the right word masked.
    pub rmr: u32,
}

/// Bit clocks per frame: two 16-bit words.
const BITS_PER_FRAME: u32 = 32;
/// `xCR2`: `BCP` (drive on falling, sample on rising), `MSEL` = MCLK1,
/// `BCD` (bit clock generated).
const CR2_MASTER: u32 = (1 << 25) | (1 << 26) | (1 << 24);
/// `RCR2.SYNC` = 1: synchronous with the transmitter.
const CR2_SYNC_TX: u32 = 1 << 30;
/// `xCR4`: `FRSZ` = 1 (two words), `SYWD` = 15 (16 bit clocks), `MF`
/// (MSB first), `FSE` (one bit early), `FSP` (active low), `FSD`
/// (generated).
const CR4_I2S: u32 = (1 << 16) | (15 << 8) | (1 << 4) | (1 << 3) | (1 << 1) | 1;
/// `xCR5`: `WNW` = `W0W` = 15 (16-bit words), `FBT` = 15 (MSB first).
const CR5_16BIT: u32 = (15 << 24) | (15 << 16) | (15 << 8);

/// The bit clock divider field for a rate: MCLK / ((`DIV` + 1) × 2) is
/// 32 × fs.
pub const fn bclk_div(rate: AudioRate) -> u32 {
    rate.mclk_hz() / (BITS_PER_FRAME * rate.hz()) / 2 - 1
}

/// The SAI1 register values for a rate (the table in the module
/// documentation).
pub const fn sai_plan(rate: AudioRate) -> SaiPlan {
    let div = bclk_div(rate);
    SaiPlan {
        tcr2: CR2_MASTER | div,
        tcr3: 0,
        tcr4: CR4_I2S,
        tcr5: CR5_16BIT,
        rcr1: RX_WATERMARK - 1,
        rcr2: CR2_SYNC_TX | CR2_MASTER | div,
        rcr3: 1 << 16,
        rcr4: CR4_I2S,
        rcr5: CR5_16BIT,
        rmr: 0b10,
    }
}

/// A receive FIFO entry (`RDR0`) to its sample: with `FBT` = 15 the
/// 16-bit word is in bits 15..0.
pub const fn sample_from_fifo(word: u32) -> i16 {
    word as u16 as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sc5_the_audio_pll_gives_each_rate_its_mclk() {
        for rate in AudioRate::ALL {
            let c = clock_plan(rate);
            assert_eq!(c.pll_hz(), 786_432_000);
            assert_eq!(c.mclk_hz(), rate.mclk_hz() as u64, "{rate:?}");
            assert!((27..=54).contains(&c.pll_div_select));
            assert!(c.pll_num < c.pll_denom);
            assert!(c.sai1_clk_pred <= 7 && c.sai1_clk_podf <= 63);
        }
        let zero = ClockPlan {
            pll_denom: 0,
            ..clock_plan(AudioRate::Hz8000)
        };
        assert_eq!(zero.mclk_hz(), 0);
    }

    #[test]
    fn sc5_sai_bit_clock_dividers_and_frame_shape_per_rate() {
        let divs: [u32; 4] = [11, 5, 5, 3];
        for (rate, div) in AudioRate::ALL.into_iter().zip(divs) {
            let p = sai_plan(rate);
            assert_eq!(bclk_div(rate), div, "{rate:?}");
            assert_eq!(p.tcr2 & 0xFF, div);
            assert_eq!(p.rcr2 & 0xFF, div);
            // The bit clock is exactly 32 × fs.
            assert_eq!(rate.mclk_hz() / ((div + 1) * 2), 32 * rate.hz());
            assert_eq!(p.tcr2 >> 30, 0, "transmitter asynchronous");
            assert_eq!(p.rcr2 >> 30, 1, "receiver synchronous to it");
            assert_eq!((p.tcr2 >> 26) & 0b11, 1, "MCLK1");
            assert_eq!(p.tcr2 & (1 << 24), 1 << 24, "bit clock master");
        }
        let p = sai_plan(AudioRate::Hz16000);
        assert_eq!(p.tcr2, 0x0700_0005);
        assert_eq!(p.rcr2, 0x4700_0005);
        assert_eq!(p.rcr4, 0x0001_0F1B);
        assert_eq!(p.tcr4, p.rcr4);
        assert_eq!((p.rcr4 >> 16) & 0x1F, 1, "FRSZ: two words");
        assert_eq!((p.rcr4 >> 8) & 0x1F, 15, "SYWD: 16 bit clocks");
        assert_eq!(p.rcr5, 0x0F0F_0F00);
        assert_eq!(p.rcr3, 0x0001_0000);
        assert_eq!(p.tcr3, 0);
        assert_eq!(p.rcr1, 15);
        assert_eq!(p.rmr, 0b10);
        assert_eq!(RCSR_ENABLE, 0x9000_0500);
        assert_eq!(TCSR_ENABLE, 0x9000_0000);
    }

    #[test]
    fn sc5_fifo_words_are_16_bit_samples() {
        assert_eq!(sample_from_fifo(0x0000_7FFF), i16::MAX);
        assert_eq!(sample_from_fifo(0x0000_8000), i16::MIN);
        assert_eq!(sample_from_fifo(0x0000_FFFF), -1);
        assert_eq!(sample_from_fifo(0xFFFF_0001), 1);
    }
}
