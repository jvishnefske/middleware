//! The EVKB's sensors as middleware channels: the pure core of the
//! accelerometer and microphone drivers (design.md "Sensor channels and
//! fast links", SC4 and SC5).
//!
//! Both devices sit on LPI2C1 (GPIO_AD_B1_00/01, payload I2C bus 0); the
//! microphone's samples arrive on SAI1. Everything here is register
//! values, byte layouts and arithmetic, so it is tested on the host; the
//! shell performs the I2C transfers and register writes in the order
//! given.
//!
//! * [`fxos8700`] — the FXOS8700CQ accelerometer and magnetometer
//!   ([`FXOS8700_ADDR`]): the register plan for each output data rate and
//!   range, the data-ready decision and the conversion of the burst read to
//!   `i16` samples with the scale the stream header carries (SC4).
//! * [`wm8960`] — the WM8960 codec ([`WM8960_ADDR`]): the register list
//!   that records from the onboard microphone at each [`AudioRate`] as an
//!   I2S slave, 16-bit (SC5).
//! * [`sai`] — the audio PLL, SAI1 root clock and SAI1 transmitter and
//!   receiver register values for each [`AudioRate`] (SC5).
//! * [`audio`] — fixed-point RMS and peak over a block, and the G.711
//!   µ-law encoder and decoder (SC5).
//! * [`imu_route`] and [`mic_route`] — the source routes' cores: samples
//!   to `middleware::stream::samples` batches with exact indices and loss
//!   accounting (SC7, SC8).
//!
//! The shell's order for the microphone: [`sai::clock_plan`] (audio PLL,
//! SAI1 root, MCLK out) so the codec has MCLK; [`wm8960::record_plan`]
//! over LPI2C1; then [`sai::sai_plan`] and enable the receiver, its FIFO
//! drained by eDMA into a ring that the shell reads with
//! [`sai::sample_from_fifo`] into a [`mic_route::MicRoute`].

pub mod audio;
pub mod fxos8700;
pub mod imu_route;
pub mod mic_route;
pub mod sai;
pub mod wm8960;

/// The FXOS8700CQ's 7-bit I2C address on LPI2C1 (SA1 = 1, SA0 = 1).
pub const FXOS8700_ADDR: u8 = 0x1F;
/// The WM8960's 7-bit I2C address on LPI2C1 (fixed by the part).
pub const WM8960_ADDR: u8 = 0x1A;

/// A microphone sample rate the codec and SAI1 are planned for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioRate {
    /// 8 kHz (telephone band; µ-law's native rate).
    Hz8000,
    /// 16 kHz (wideband speech; the NXP SDK's tested rate).
    Hz16000,
    /// 32 kHz.
    Hz32000,
    /// 48 kHz.
    Hz48000,
}

impl AudioRate {
    /// Every supported rate, slowest first.
    pub const ALL: [AudioRate; 4] = [
        AudioRate::Hz8000,
        AudioRate::Hz16000,
        AudioRate::Hz32000,
        AudioRate::Hz48000,
    ];

    /// The rate for a route configuration's sample rate in Hz, or `None`
    /// for a rate the board does not offer (`InvalidArgument`).
    pub const fn from_hz(hz: u32) -> Option<AudioRate> {
        match hz {
            8000 => Some(AudioRate::Hz8000),
            16000 => Some(AudioRate::Hz16000),
            32000 => Some(AudioRate::Hz32000),
            48000 => Some(AudioRate::Hz48000),
            _ => None,
        }
    }

    /// The rate in Hz, as the stream header carries it.
    pub const fn hz(&self) -> u32 {
        match self {
            AudioRate::Hz8000 => 8000,
            AudioRate::Hz16000 => 16000,
            AudioRate::Hz32000 => 32000,
            AudioRate::Hz48000 => 48000,
        }
    }

    /// The codec's MCLK (SAI1_MCLK) for this rate: 6.144 MHz (768 and
    /// 384 × fs) for 8 and 16 kHz, as the NXP SDK's EVKB examples run it;
    /// 12.288 MHz (384 and 256 × fs) for 32 and 48 kHz, which the WM8960's
    /// ADC dividers cannot reach from 6.144 MHz.
    pub const fn mclk_hz(&self) -> u32 {
        match self {
            AudioRate::Hz8000 | AudioRate::Hz16000 => 6_144_000,
            AudioRate::Hz32000 | AudioRate::Hz48000 => 12_288_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sc5_rates_round_trip_through_hz_and_others_are_refused() {
        for rate in AudioRate::ALL {
            assert_eq!(AudioRate::from_hz(rate.hz()), Some(rate));
            // MCLK / fs is 256 × a WM8960 ADC divider (1, 1.5 or 3 here).
            assert!(rate.mclk_hz().is_multiple_of(128 * rate.hz()));
        }
        for hz in [0, 1, 11025, 22050, 44100, 96000, u32::MAX] {
            assert_eq!(AudioRate::from_hz(hz), None);
        }
    }
}
