//! SAI1 receiving the WM8960's microphone (SC8), without interrupts.
//!
//! [`start_clocks`] brings up the audio PLL (PLL4), SAI1's root clock and
//! MCLK on GPIO_AD_B1_09 ([`sai::clock_plan`]) so the codec has its clock
//! before it is configured; [`start`] then routes the bit clock, frame
//! sync and receive data pads, programs the transmitter (the clock master
//! the EVKB wires to the codec) and the receiver synchronous to it
//! ([`sai::sai_plan`]) and starts eDMA channel 1 ([`super::edma`]) copying
//! each received word into [`AUDIO_RING`] in DTCM. [`SaiRx::drain`] takes
//! what arrived, as `UartRx` does, and converts each word to a sample
//! ([`sai::sample_from_fifo`]). [`stop`] disables both directions, the
//! channel and the PLL.
//!
//! The ring holds [`RING_LEN`] words (256 ms at 16 kHz, 85 ms at 48 kHz);
//! the device drains it every `mic_route::POLL_US`. A drain that comes
//! after more than 7/8 of the ring's time cannot tell how often the DMA
//! lapped the reader, so it discards the ring and reports the samples that
//! time stood for as lost (an estimate from the clock and the rate).
//!
//! The values come from the RT1052 SDK (`fsl_clock.c`'s
//! `CLOCK_InitAudioPll`, `fsl_sai.c`'s `SAI_RxEnable`, `fsl_iomuxc.h`),
//! MicroPython's `machine_i2s.c` for the MIMXRT1050_EVK and Zephyr's EVK
//! pinctrl (see [`sai`]).

use core::sync::atomic::{AtomicU32, Ordering::Relaxed};

use embassy_nxp::pac;
use embassy_time::{Duration, Instant};
use imxrt_base::sensors::AudioRate;
use imxrt_base::sensors::sai;
use middleware::wasm_abi::AbiError;

use super::edma;

/// Words in the receive ring (one microphone sample each).
pub const RING_LEN: usize = 4096;

/// The receive ring the eDMA writes, in DTCM (`.bss`).
static AUDIO_RING: [AtomicU32; RING_LEN] = [const { AtomicU32::new(0) }; RING_LEN];

/// The eDMA channel of the receiver.
const CHANNEL: edma::Channel = edma::Channel::Ch1;

/// Longest wait for the audio PLL to lock (it takes well under 1 ms).
const LOCK_TIMEOUT: Duration = Duration::from_millis(10);

/// The SAI1 pads' control: the SDK's pin_mux value for them (keeper,
/// 100 MHz, R0/6 drive, slow slew).
const PAD_SAI: u32 = 0x10B0;

/// The audio PLL, SAI1's root clock and MCLK out, for `rate`.
/// `Transport` if the PLL does not lock.
pub fn start_clocks(rate: AudioRate) -> Result<(), AbiError> {
    use pac::ccm_analog::regs::{PllAudio, PllAudioDenom, PllAudioNum};
    let c = sai::clock_plan(rate);
    let a = pac::CCM_ANALOG;
    // CLOCK_InitAudioPll: bypass (24 MHz reference), the fraction, the post
    // divider (MISC2.AUDIO_DIV 0 for ÷1), enable, wait for lock, unbypass.
    let bypassed = (a.pll_audio().read().0 & !(0b11 << 14)) | sai::PLL_BYPASS;
    a.pll_audio().write_value(PllAudio(bypassed));
    a.pll_audio_num().write_value(PllAudioNum(c.pll_num));
    a.pll_audio_denom().write_value(PllAudioDenom(c.pll_denom));
    a.misc2().modify(|w| w.0 &= !sai::MISC2_AUDIO_DIV);
    a.pll_audio().write_value(PllAudio(c.pll_audio() | sai::PLL_BYPASS));
    let deadline = Instant::now() + LOCK_TIMEOUT;
    while a.pll_audio().read().0 & sai::PLL_LOCK == 0 {
        if Instant::now() > deadline {
            a.pll_audio().write_value(PllAudio(sai::PLL_POWERDOWN));
            return Err(AbiError::Transport);
        }
    }
    a.pll_audio().write_value(PllAudio(c.pll_audio()));

    let ccm = pac::CCM;
    ccm.cscmr1().modify(|w| w.0 = c.cscmr1(w.0));
    ccm.cs1cdr().modify(|w| w.0 = c.cs1cdr(w.0));
    // SAI1's clock gate (CCGR5 CG9), before any SAI1 register access.
    ccm.ccgr5().modify(|w| w.set_cg9(0b11));

    // MCLK1 = the SAI1 root clock, driven out on SAI1_MCLK.
    pac::IOMUXC_GPR
        .gpr1()
        .modify(|w| w.0 = (w.0 & !0b111) | sai::GPR1_SAI1_MCLK_DIR);
    let m = pac::IOMUXC;
    m.sai1_mclk2_select_input()
        .write_value(pac::iomuxc::regs::Sai1Mclk2SelectInput(sai::PAD_DAISY));
    m.sw_pad_ctl_pad_gpio_ad_b1_09().write_value(pac::iomuxc::regs::Ctl(PAD_SAI));
    m.sw_mux_ctl_pad_gpio_ad_b1_09().write_value(pac::iomuxc::regs::MuxCtl(sai::PAD_ALT));
    Ok(())
}

/// The receiver's reader.
pub struct SaiRx {
    ring: edma::Ring,
    tail: usize,
    rate_hz: u32,
    last_us: u64,
}

/// What a [`SaiRx::drain`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Drained {
    /// Samples written to the buffer.
    pub samples: usize,
    /// Samples lost before them (the ring overran).
    pub lost: u32,
}

/// Routes the frame pads, programs SAI1 for `rate` and starts receiving
/// (the clocks must run: [`start_clocks`]).
pub fn start(rate: AudioRate, now_us: u64) -> SaiRx {
    use pac::sai1::regs::*;
    let m = pac::IOMUXC;
    m.sai1_rx_data0_select_input()
        .write_value(pac::iomuxc::regs::Sai1RxData0SelectInput(sai::PAD_DAISY));
    m.sai1_tx_bclk_select_input()
        .write_value(pac::iomuxc::regs::Sai1TxBclkSelectInput(sai::PAD_DAISY));
    m.sai1_tx_sync_select_input()
        .write_value(pac::iomuxc::regs::Sai1TxSyncSelectInput(sai::PAD_DAISY));
    for (ctl, mux) in [
        (m.sw_pad_ctl_pad_gpio_ad_b1_12(), m.sw_mux_ctl_pad_gpio_ad_b1_12()),
        (m.sw_pad_ctl_pad_gpio_ad_b1_14(), m.sw_mux_ctl_pad_gpio_ad_b1_14()),
        (m.sw_pad_ctl_pad_gpio_ad_b1_15(), m.sw_mux_ctl_pad_gpio_ad_b1_15()),
    ] {
        ctl.write_value(pac::iomuxc::regs::Ctl(PAD_SAI));
        mux.write_value(pac::iomuxc::regs::MuxCtl(sai::PAD_ALT));
    }

    let s = pac::SAI1;
    let p = sai::sai_plan(rate);
    // Software and FIFO reset of both directions, then released.
    s.tcsr().write_value(Tcsr(sai::CSR_RESET));
    s.rcsr().write_value(Rcsr(sai::CSR_RESET));
    s.tcsr().write_value(Tcsr(0));
    s.rcsr().write_value(Rcsr(0));
    s.tcr1().write_value(Tcr1(0));
    s.tcr2().write_value(Tcr2(p.tcr2));
    s.tcr3().write_value(Tcr3(p.tcr3));
    s.tcr4().write_value(Tcr4(p.tcr4));
    s.tcr5().write_value(Tcr5(p.tcr5));
    s.tmr().write_value(Tmr(0));
    s.rcr1().write_value(Rcr1(p.rcr1));
    s.rcr2().write_value(Rcr2(p.rcr2));
    s.rcr3().write_value(Rcr3(p.rcr3));
    s.rcr4().write_value(Rcr4(p.rcr4));
    s.rcr5().write_value(Rcr5(p.rcr5));
    s.rmr().write_value(Rmr(p.rmr));

    let ring = edma::Ring {
        channel: CHANNEL,
        source: sai::DMAMUX_SAI1_RX,
        src: s.rdr(0).as_ptr() as u32,
        base: AUDIO_RING.as_ptr() as u32,
        len: RING_LEN as u16,
        width: edma::Width::U32,
    };
    edma::start_ring(&ring);
    // SAI_RxEnable for a synchronous receiver: the transmitter's bit clock
    // and enable first, then the receiver's, the FIFO error cleared.
    s.tcsr().write_value(Tcsr(1 << 28));
    s.tcsr().write_value(Tcsr(sai::TCSR_ENABLE));
    s.rcsr().write_value(Rcsr(1 << 28));
    s.rcsr().write_value(Rcsr(sai::RCSR_ENABLE | sai::CSR_FEF));
    let tail = edma::write_index(&ring);
    SaiRx {
        ring,
        tail,
        rate_hz: rate.hz(),
        last_us: now_us,
    }
}

/// Stops SAI1 (both directions), the receive channel and the audio PLL;
/// MCLK stops with them.
pub fn stop() {
    use pac::sai1::regs::{Rcsr, Tcsr};
    let s = pac::SAI1;
    s.rcsr().write_value(Rcsr(0));
    s.tcsr().write_value(Tcsr(0));
    edma::stop(CHANNEL);
    pac::CCM_ANALOG
        .pll_audio()
        .write_value(pac::ccm_analog::regs::PllAudio(sai::PLL_POWERDOWN));
}

impl SaiRx {
    /// Copies the samples that arrived into `out` (as many as fit; call
    /// again until it returns none). `now_us` is the time of the call.
    pub fn drain(&mut self, now_us: u64, out: &mut [i16]) -> Drained {
        let head = edma::write_index(&self.ring);
        let elapsed = now_us.saturating_sub(self.last_us);
        self.last_us = now_us;
        let ring_us = RING_LEN as u64 * 1_000_000 / u64::from(self.rate_hz.max(1));
        if elapsed >= ring_us / 8 * 7 {
            // Possibly lapped: what the ring holds may be newer than what
            // the tail expects. Start over from the write position.
            self.tail = head;
            let lost = elapsed * u64::from(self.rate_hz) / 1_000_000;
            return Drained {
                samples: 0,
                lost: u32::try_from(lost).unwrap_or(u32::MAX),
            };
        }
        let mut n = 0;
        while self.tail != head && n < out.len() {
            if let (Some(w), Some(slot)) = (AUDIO_RING.get(self.tail), out.get_mut(n)) {
                *slot = sai::sample_from_fifo(w.load(Relaxed));
            }
            self.tail = (self.tail + 1) % RING_LEN;
            n += 1;
        }
        Drained { samples: n, lost: 0 }
    }
}
