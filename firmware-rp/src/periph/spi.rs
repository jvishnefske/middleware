//! SPI bus decisions (V17): the clock divider, the mode bits, the bounded
//! FIFO pump of one transfer and its time bound.
//!
//! The RP2040 and RP2350 SPI controllers (Arm PL022) divide `clk_peri` by
//! an even prescaler `CPSDVSR` (2..=254) and `1 + SCR` (SCR 0..=255).
//! [`Clock::new`] picks the smallest divisor whose clock does not exceed
//! the payload's request, so a device never sees a faster clock than it
//! asked for: the fastest is `clk_peri / 2` (62.5 MHz on the RP2040,
//! 75 MHz on the RP2350, above the ABI's 40 MHz cap), the slowest
//! `clk_peri / 65024` (1.93 kHz, 2.31 kHz); a request below that is
//! `Unsupported`.
//!
//! The mode is the ABI's `CPOL << 1 | CPHA`, the PL022's `SPO` and `SPH`
//! ([`mode_bits`]). Frames are 8 bits, Motorola format.
//!
//! The controller's own chip select (CSn) pulses between frames when
//! `SPH` is 0, which most devices take as the end of a transaction, so the
//! shell does not use it: the payload's CS pin is a GPIO output, high while
//! idle and low for the whole transfer, in every mode.
//!
//! # The pump
//!
//! Transfers do not use DMA. The shell copies the payload's bytes into a
//! firmware buffer, padded with zeros to the transfer length (ABI 2), and
//! the controller loop moves them through the 8-byte FIFOs a step at a time
//! ([`Pump`]), yielding to the executor between steps, so `spi_transfer`
//! returns at once and completion arrives as a `DmaDone` event as on the
//! ESP base. Received bytes overwrite the sent ones in place (byte `i` is
//! received only after byte `i` was sent). The pump never has more than
//! [`FIFO_DEPTH`] bytes in flight, so the receive FIFO cannot overflow,
//! and each step moves at most that many bytes each way. A transfer that
//! is not done by [`timeout_us`] (twice its bus time plus [`MARGIN_US`])
//! is abandoned and completes with `Transport`.

use middleware::wasm_abi::AbiError;

/// Depth of the PL022's transmit and receive FIFOs, in frames.
pub const FIFO_DEPTH: usize = 8;

/// Slack added to every transfer's time bound, in µs (the controller loop
/// may be busy with the payload between pump steps).
pub const MARGIN_US: u64 = 100_000;

/// The PL022 clock divider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clock {
    /// `SSPCPSR.CPSDVSR`: even, 2..=254.
    pub cpsdvsr: u8,
    /// `SSPCR0.SCR`: 0..=255.
    pub scr: u8,
}

impl Clock {
    /// The divider for at most `freq_hz` on `clk_peri_hz` (see the module
    /// docs); `InvalidArgument` for a zero clock, `Unsupported` below the
    /// slowest divider.
    pub fn new(clk_peri_hz: u32, freq_hz: u32) -> Result<Self, AbiError> {
        if freq_hz == 0 || clk_peri_hz == 0 {
            return Err(AbiError::InvalidArgument);
        }
        let ratio = clk_peri_hz.div_ceil(freq_hz);
        let mut best: Option<(u32, Clock)> = None;
        // 127 prescalers: bounded.
        for cpsdvsr in (2u32..=254).step_by(2) {
            let post = ratio.div_ceil(cpsdvsr).max(1);
            if post > 256 {
                continue;
            }
            let divisor = cpsdvsr * post;
            if best.is_none_or(|(d, _)| divisor < d) {
                let clock = Clock {
                    cpsdvsr: u8::try_from(cpsdvsr).map_err(|_| AbiError::Unsupported)?,
                    scr: u8::try_from(post - 1).map_err(|_| AbiError::Unsupported)?,
                };
                best = Some((divisor, clock));
            }
        }
        best.map(|(_, c)| c).ok_or(AbiError::Unsupported)
    }

    /// The divisor `CPSDVSR * (1 + SCR)`.
    pub const fn divisor(&self) -> u32 {
        self.cpsdvsr as u32 * (self.scr as u32 + 1)
    }

    /// The clock it produces from `clk_peri_hz`.
    pub const fn hz(&self, clk_peri_hz: u32) -> u32 {
        clk_peri_hz / self.divisor()
    }
}

/// `(SPO, SPH)` for an ABI mode (`CPOL << 1 | CPHA`); `InvalidArgument`
/// above 3.
pub const fn mode_bits(mode: u8) -> Result<(bool, bool), AbiError> {
    if mode > 3 {
        return Err(AbiError::InvalidArgument);
    }
    Ok((mode & 2 != 0, mode & 1 != 0))
}

/// Progress of one transfer through the FIFOs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pump {
    len: usize,
    sent: usize,
    received: usize,
}

impl Pump {
    /// A transfer of `len` bytes, nothing moved yet.
    pub const fn new(len: usize) -> Self {
        Pump { len, sent: 0, received: 0 }
    }

    /// The transfer length.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the transfer is empty (never started by the shell).
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The index of the next byte to push, if one may go now: bytes remain
    /// and fewer than [`FIFO_DEPTH`] are in flight.
    pub const fn next_send(&self) -> Option<usize> {
        if self.sent < self.len && self.sent - self.received < FIFO_DEPTH { Some(self.sent) } else { None }
    }

    /// After pushing the byte [`Pump::next_send`] named.
    pub const fn sent_one(self) -> Self {
        let sent = if self.sent < self.len { self.sent + 1 } else { self.sent };
        Pump { sent, ..self }
    }

    /// Where the next received byte goes, if one is owed.
    pub const fn next_receive(&self) -> Option<usize> {
        if self.received < self.sent { Some(self.received) } else { None }
    }

    /// After storing the byte [`Pump::next_receive`] named.
    pub const fn received_one(self) -> Self {
        let received = if self.received < self.sent { self.received + 1 } else { self.received };
        Pump { received, ..self }
    }

    /// Every byte sent and received.
    pub const fn done(&self) -> bool {
        self.received == self.len
    }
}

/// The time bound of a `len`-byte transfer at `freq_hz`, in µs: twice its
/// bus time plus [`MARGIN_US`].
pub fn timeout_us(freq_hz: u32, len: usize) -> u64 {
    let bits = 8 * len as u64;
    let bus_us = (bits * 1_000_000).div_ceil(u64::from(freq_hz.max(1)));
    2 * bus_us + MARGIN_US
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::wasm_abi::periph::{MAX_DMA, MAX_SPI_KHZ};

    const CLOCKS: [u32; 2] = [125_000_000, 150_000_000];

    #[test]
    fn v17_the_divider_never_exceeds_the_request_and_is_the_closest() {
        for clk in CLOCKS {
            for khz in [3, 10, 100, 400, 1_000, 8_000, 10_000, 20_000, 33_000, MAX_SPI_KHZ as u32] {
                let freq = khz * 1000;
                let c = Clock::new(clk, freq).unwrap();
                assert!(c.cpsdvsr.is_multiple_of(2) && (2..=254).contains(&c.cpsdvsr), "{c:?}");
                assert!(c.hz(clk) <= freq, "{clk} {freq}: {}", c.hz(clk));
                // No valid divisor between the ideal ratio and the one chosen.
                let ideal = clk.div_ceil(freq);
                let smallest = (ideal..c.divisor())
                    .find(|d| (2..=254).step_by(2).any(|p| d % p == 0 && d / p <= 256));
                assert_eq!(smallest, None, "{clk} {freq}: {c:?}");
            }
        }
        // 1 MHz at 125 MHz: 125 = 2 * 62.5 has no exact divisor, so 126.
        assert_eq!(Clock::new(125_000_000, 1_000_000).map(|c| c.divisor()), Ok(126));
        // 40 MHz at 150 MHz: 37.5 MHz (divisor 4), not faster.
        assert_eq!(Clock::new(150_000_000, 40_000_000).map(|c| c.hz(150_000_000)), Ok(37_500_000));
        // Faster than clk_peri / 2 is clamped to it.
        assert_eq!(Clock::new(125_000_000, 100_000_000).map(|c| c.divisor()), Ok(2));
    }

    #[test]
    fn v17_clocks_below_the_slowest_divider_are_unsupported() {
        assert_eq!(Clock::new(125_000_000, 1_000), Err(AbiError::Unsupported), "ABI minimum 1 kHz");
        assert_eq!(Clock::new(150_000_000, 2_000), Err(AbiError::Unsupported));
        let slowest = Clock::new(125_000_000, 1_923).unwrap();
        assert_eq!((slowest.cpsdvsr, slowest.scr), (254, 255));
        assert_eq!(Clock::new(0, 1000), Err(AbiError::InvalidArgument));
        assert_eq!(Clock::new(125_000_000, 0), Err(AbiError::InvalidArgument));
    }

    #[test]
    fn v17_modes_map_to_polarity_and_phase() {
        assert_eq!(mode_bits(0), Ok((false, false)));
        assert_eq!(mode_bits(1), Ok((false, true)));
        assert_eq!(mode_bits(2), Ok((true, false)));
        assert_eq!(mode_bits(3), Ok((true, true)));
        assert_eq!(mode_bits(4), Err(AbiError::InvalidArgument));
    }

    /// Runs `pump` against a FIFO model that accepts at most `room` pushes
    /// and returns at most `ready` bytes per step; returns the steps taken.
    fn run(len: usize, room: usize, ready: usize) -> usize {
        let mut p = Pump::new(len);
        let mut fifo = 0usize;
        let mut steps = 0;
        while !p.done() {
            steps += 1;
            assert!(steps <= 2 * len + 2, "the pump stalled");
            let mut pushed = 0;
            while let Some(i) = p.next_send() {
                if pushed == room {
                    break;
                }
                assert_eq!(i, p.sent);
                p = p.sent_one();
                pushed += 1;
                fifo += 1;
                assert!(fifo <= FIFO_DEPTH, "receive FIFO would overflow");
            }
            let mut popped = 0;
            while let Some(i) = p.next_receive() {
                if popped == ready {
                    break;
                }
                assert_eq!(i, p.received);
                assert!(i < p.sent, "in place: received only after sent");
                p = p.received_one();
                popped += 1;
                fifo -= 1;
            }
        }
        steps
    }

    #[test]
    fn v17_the_pump_moves_every_byte_in_bounded_steps() {
        for len in [1, 7, 8, 9, 64, MAX_DMA] {
            // A fast bus drains each step; a slow one returns one byte a step.
            assert!(run(len, FIFO_DEPTH, FIFO_DEPTH) <= len.div_ceil(FIFO_DEPTH));
            assert!(run(len, FIFO_DEPTH, 1) <= len + 1);
            assert!(run(len, 1, 1) <= len);
        }
        let p = Pump::new(2);
        assert_eq!((p.next_send(), p.next_receive(), p.done(), p.len()), (Some(0), None, false, 2));
        // Calls out of order are no-ops, never past the length.
        let p = p.received_one();
        assert_eq!(p.next_receive(), None);
        let p = p.sent_one().sent_one().sent_one();
        assert_eq!(p.next_send(), None);
        assert!(Pump::new(0).done() && Pump::new(0).is_empty());
    }

    #[test]
    fn v17_every_transfer_is_bounded_in_time() {
        // 16 bytes at 1 MHz: 128 µs on the bus.
        assert_eq!(timeout_us(1_000_000, 16), 256 + MARGIN_US);
        // The longest transfer at the slowest clock: about 34 s.
        let worst = timeout_us(1_923, MAX_DMA);
        assert!(worst > 30_000_000 && worst < 40_000_000, "{worst}");
        assert!(timeout_us(0, 1) > 0);
    }
}
