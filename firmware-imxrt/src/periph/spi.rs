//! LPSPI1 decisions (SPI bus 0, SCK D13, MOSI D11, MISO D12): the clock
//! and delay registers, the mode bits, the bounded FIFO pump of one
//! transfer and its time bound.
//!
//! # Clock
//!
//! The LPSPI master divides its functional clock (the LPSPI clock root,
//! [`LPSPI_CLK_HZ`]) by `2^PRESCALE` (`TCR.PRESCALE`, 0..=7) and then by
//! `SCKDIV + 2` (`CCR.SCKDIV`, 0..=255):
//!
//! ```text
//! SCK = LPSPI clock root / (2^PRESCALE × (SCKDIV + 2))
//! ```
//!
//! [`Clock::new`] picks the smallest divisor whose clock does not exceed
//! the payload's request (the smaller prescaler on a tie), so a device
//! never sees a faster clock than it asked for: on the 66 MHz root the
//! fastest is 33 MHz (below the ABI's 40 MHz cap), the slowest
//! 66 MHz / 32 896 = 2.006 kHz; a request below that is `Unsupported`.
//!
//! The delays are in the same prescaled cycles: `PCSSCK` (PCS to the
//! first SCK edge) and `SCKPCS` (last edge to PCS negation) are half an SCK
//! period, `DBT` (between frames) one period ([`Clock::ccr`]). The chip
//! select the device sees is not the controller's PCS (the payload's CS
//! pin is a GPIO output, low for the whole transfer, as on the RP base
//! firmware), so the delays only space the frames: each 8-bit frame takes
//! [`Clock::frame_cycles`], ten SCK periods.
//!
//! # Mode and frames
//!
//! The mode is the ABI's `CPOL << 1 | CPHA` ([`mode_bits`]), `TCR.CPOL`
//! and `TCR.CPHA`; frames are 8 bits MSB first (`FRAMESZ` 7), PCS0, not
//! continuous ([`tcr`]). `CFGR1` makes the block a master that stalls
//! rather than under- or overruns its FIFOs (`NOSTALL` 0, [`CFGR1`]); a
//! transmit or receive error flag (`SR.TEF`, `SR.REF`) is still checked
//! ([`check_status`]) and fails the transfer with `Transport`.
//!
//! # The pump
//!
//! As on the RP base firmware, transfers do not use DMA: the shell copies
//! the payload's bytes into a firmware buffer (zero padded to the transfer
//! length, ABI 2) and moves them through the [`FIFO_DEPTH`]-word FIFOs a
//! step at a time ([`Pump::step`]), yielding between steps, and completes
//! the transfer as a `DmaDone` event. Each step reads `FSR.TXCOUNT` and
//! `FSR.RXCOUNT`, then pops the bytes the step names (into the buffer, in
//! place: byte `i` is received only after byte `i` was sent) and pushes the
//! ones it names after them. The pump never has more than [`FIFO_DEPTH`]
//! bytes in flight, so the receive FIFO cannot overflow, and each step
//! moves at most that many each way. A transfer not done by
//! [`Clock::timeout_us`] (twice its bus time plus [`MARGIN_US`]) is
//! abandoned and completes with `Transport`.

use core::ops::Range;
use middleware::wasm_abi::AbiError;

/// PLL2 (528 MHz) / 8 as the LPSPI clock root (`CBCMR.LPSPI_CLK_SEL` = 2,
/// `LPSPI_PODF` = 7): the root the shell configures.
pub const LPSPI_CLK_HZ: u32 = 66_000_000;

/// Depth of the LPSPI's transmit and receive FIFOs on the RT1050, in
/// words.
pub const FIFO_DEPTH: usize = 16;

/// Frame size in bits (`TCR.FRAMESZ` + 1).
pub const FRAME_BITS: u32 = 8;

/// Slack added to every transfer's time bound, in µs (the controller loop
/// may be busy with the payload between pump steps).
pub const MARGIN_US: u64 = 100_000;

/// `CFGR1`: `MASTER`; `NOSTALL` 0, `PINCFG` 0 (SIN in, SOUT out), PCS
/// active low.
pub const CFGR1: u32 = 1 << 0;

/// `CR` bits.
pub mod cr {
    /// Module enable.
    pub const MEN: u32 = 1 << 0;
    /// Software reset.
    pub const RST: u32 = 1 << 1;
    /// Reset the transmit FIFO.
    pub const RTF: u32 = 1 << 8;
    /// Reset the receive FIFO.
    pub const RRF: u32 = 1 << 9;
}

/// `SR` bits.
pub mod sr {
    /// Transmit data (FIFO at or below the watermark).
    pub const TDF: u32 = 1 << 0;
    /// Receive data (FIFO above the watermark).
    pub const RDF: u32 = 1 << 1;
    /// Word complete.
    pub const WCF: u32 = 1 << 8;
    /// Frame complete.
    pub const FCF: u32 = 1 << 9;
    /// Transfer complete.
    pub const TCF: u32 = 1 << 10;
    /// Transmit error (underrun).
    pub const TEF: u32 = 1 << 11;
    /// Receive error (overrun).
    pub const REF: u32 = 1 << 12;
    /// Data match.
    pub const DMF: u32 = 1 << 13;
    /// Module busy.
    pub const MBF: u32 = 1 << 24;
    /// The flags [`super::check_status`] fails on.
    pub const ERRORS: u32 = TEF | REF;
    /// Every write-one-to-clear flag.
    pub const CLEAR: u32 = WCF | FCF | TCF | TEF | REF | DMF;
}

/// `Transport` if `sr` reports a transmit or receive error.
pub const fn check_status(sr: u32) -> Result<(), AbiError> {
    if sr & sr::ERRORS != 0 {
        Err(AbiError::Transport)
    } else {
        Ok(())
    }
}

/// The LPSPI clock divider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clock {
    /// `TCR.PRESCALE`: divide by `2^prescale`, 0..=7.
    pub prescale: u8,
    /// `CCR.SCKDIV`: divide by `sckdiv + 2`.
    pub sckdiv: u8,
}

impl Clock {
    /// The divider for at most `freq_hz` on a root of `clk_hz` (see the
    /// module docs); `InvalidArgument` for a zero clock, `Unsupported`
    /// below the slowest divider.
    pub fn new(clk_hz: u32, freq_hz: u32) -> Result<Self, AbiError> {
        if freq_hz == 0 || clk_hz == 0 {
            return Err(AbiError::InvalidArgument);
        }
        let ratio = clk_hz.div_ceil(freq_hz);
        let mut best: Option<(u32, Clock)> = None;
        // Eight prescalers: bounded.
        for prescale in 0u8..=7 {
            let pre = 1u32 << prescale;
            let post = ratio.div_ceil(pre).max(2);
            if post > 257 {
                continue;
            }
            let divisor = pre * post;
            if best.is_none_or(|(d, _)| divisor < d) {
                let sckdiv = u8::try_from(post - 2).map_err(|_| AbiError::Unsupported)?;
                best = Some((divisor, Clock { prescale, sckdiv }));
            }
        }
        best.map(|(_, c)| c).ok_or(AbiError::Unsupported)
    }

    /// The divisor `2^PRESCALE × (SCKDIV + 2)`.
    pub const fn divisor(&self) -> u32 {
        (1 << (self.prescale & 7)) * (self.sckdiv as u32 + 2)
    }

    /// The SCK frequency it produces from `clk_hz` (at most the request).
    pub const fn hz(&self, clk_hz: u32) -> u32 {
        clk_hz / self.divisor()
    }

    /// `CCR.PCSSCK` and `CCR.SCKPCS`: half an SCK period, minus one.
    pub const fn half_delay(&self) -> u8 {
        ((self.sckdiv as u16 + 2) / 2 - 1) as u8
    }

    /// `CCR.DBT`: one SCK period between frames (`DBT + 2` cycles).
    pub const fn dbt(&self) -> u8 {
        self.sckdiv
    }

    /// `CCR`: `SCKDIV` `[7:0]`, `DBT` `[15:8]`, `PCSSCK` `[23:16]`, `SCKPCS`
    /// `[31:24]`.
    pub const fn ccr(&self) -> u32 {
        let half = self.half_delay() as u32;
        self.sckdiv as u32 | (self.dbt() as u32) << 8 | half << 16 | half << 24
    }

    /// Root clock cycles one frame takes: [`FRAME_BITS`] SCK periods plus
    /// the PCS-to-SCK, SCK-to-PCS and between-frame delays.
    pub const fn frame_cycles(&self) -> u32 {
        let period = self.sckdiv as u32 + 2;
        let half = self.half_delay() as u32 + 1;
        (FRAME_BITS * period + 2 * half + self.dbt() as u32 + 2) << (self.prescale & 7)
    }

    /// The time bound of a `len`-byte transfer on a root of `clk_hz`, in
    /// µs: twice its bus time plus [`MARGIN_US`].
    pub fn timeout_us(&self, clk_hz: u32, len: usize) -> u64 {
        let cycles = (len as u64).saturating_mul(u64::from(self.frame_cycles()));
        let bus_us = cycles
            .saturating_mul(1_000_000)
            .div_ceil(u64::from(clk_hz.max(1)));
        bus_us.saturating_mul(2).saturating_add(MARGIN_US)
    }
}

/// `(CPOL, CPHA)` for an ABI mode (`CPOL << 1 | CPHA`); `InvalidArgument`
/// above 3.
pub const fn mode_bits(mode: u8) -> Result<(bool, bool), AbiError> {
    if mode > 3 {
        return Err(AbiError::InvalidArgument);
    }
    Ok((mode & 2 != 0, mode & 1 != 0))
}

/// `TCR` for `mode` and `clock`: `CPOL` `[31]`, `CPHA` `[30]`, `PRESCALE`
/// `[29:27]`, PCS0, MSB first, not continuous, `FRAMESZ` 7;
/// `InvalidArgument` for a mode above 3.
pub const fn tcr(mode: u8, clock: &Clock) -> Result<u32, AbiError> {
    let (cpol, cpha) = match mode_bits(mode) {
        Ok(bits) => bits,
        Err(e) => return Err(e),
    };
    Ok((cpol as u32) << 31
        | (cpha as u32) << 30
        | ((clock.prescale & 7) as u32) << 27
        | (FRAME_BITS - 1))
}

/// What one pump step moves: pop the bytes at `pop` from the receive FIFO
/// (in order, into the buffer at those indices), then push the bytes at
/// `push` into the transmit FIFO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// Buffer indices of the bytes to pop, at most [`FIFO_DEPTH`].
    pub pop: Range<usize>,
    /// Buffer indices of the bytes to push, at most [`FIFO_DEPTH`].
    pub push: Range<usize>,
}

/// Progress of one full-duplex transfer through the FIFOs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pump {
    len: usize,
    sent: usize,
    received: usize,
}

impl Pump {
    /// A transfer of `len` bytes, nothing moved yet.
    pub const fn new(len: usize) -> Self {
        Pump {
            len,
            sent: 0,
            received: 0,
        }
    }

    /// The transfer length.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the transfer is empty (never started by the shell).
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bytes pushed so far.
    pub const fn sent(&self) -> usize {
        self.sent
    }

    /// Bytes popped so far.
    pub const fn received(&self) -> usize {
        self.received
    }

    /// Bytes pushed but not yet popped (at most [`FIFO_DEPTH`]).
    pub const fn in_flight(&self) -> usize {
        self.sent - self.received
    }

    /// The next step, given the FIFO levels just read (`FSR.TXCOUNT`,
    /// `FSR.RXCOUNT`), and the progress after it: the shell must then
    /// move exactly the bytes the step names. Pops what has arrived (never
    /// more than is owed), then pushes while the transmit FIFO has room,
    /// fewer than [`FIFO_DEPTH`] bytes are in flight and bytes remain.
    pub fn step(&mut self, tx_count: usize, rx_count: usize) -> Step {
        let pop_n = rx_count.min(self.in_flight()).min(FIFO_DEPTH);
        let pop = self.received..self.received + pop_n;
        self.received += pop_n;
        let room = FIFO_DEPTH
            .saturating_sub(tx_count)
            .min(FIFO_DEPTH.saturating_sub(self.in_flight()));
        let push_n = room.min(self.len - self.sent);
        let push = self.sent..self.sent + push_n;
        self.sent += push_n;
        Step { pop, push }
    }

    /// Every byte sent and received.
    pub const fn done(&self) -> bool {
        self.received == self.len
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::wasm_abi::periph::{MAX_DMA, MAX_SPI_KHZ};

    const ROOTS: [u32; 2] = [LPSPI_CLK_HZ, 90_000_000];

    #[test]
    fn rt16_the_divider_never_exceeds_the_request_and_is_the_closest() {
        for clk in ROOTS {
            for khz in [
                3,
                10,
                100,
                400,
                1_000,
                8_000,
                10_000,
                20_000,
                33_000,
                MAX_SPI_KHZ as u32,
            ] {
                let freq = khz * 1000;
                let c = Clock::new(clk, freq).unwrap();
                assert!(c.hz(clk) <= freq, "{clk} {freq}: {}", c.hz(clk));
                // No valid divisor between the ideal ratio and the one chosen.
                let ideal = clk.div_ceil(freq);
                let smaller = (ideal..c.divisor())
                    .find(|d| (0..8).any(|p| d % (1 << p) == 0 && (2..=257).contains(&(d >> p))));
                assert_eq!(smaller, None, "{clk} {freq}: {c:?}");
            }
        }
        // 1 MHz: 66 exactly, no prescaler.
        assert_eq!(
            Clock::new(LPSPI_CLK_HZ, 1_000_000),
            Ok(Clock {
                prescale: 0,
                sckdiv: 64
            })
        );
        // 10 MHz: 66 / 7 = 9.43 MHz, not faster.
        assert_eq!(
            Clock::new(LPSPI_CLK_HZ, 10_000_000).map(|c| c.hz(LPSPI_CLK_HZ)),
            Ok(9_428_571)
        );
        // The ABI's 40 MHz is clamped to the root / 2.
        assert_eq!(
            Clock::new(LPSPI_CLK_HZ, 40_000_000).map(|c| c.hz(LPSPI_CLK_HZ)),
            Ok(33_000_000)
        );
        // 100 kHz: 660 = 4 × 165, prescaler 2.
        assert_eq!(
            Clock::new(LPSPI_CLK_HZ, 100_000),
            Ok(Clock {
                prescale: 2,
                sckdiv: 163
            })
        );
    }

    #[test]
    fn rt16_clocks_below_the_slowest_divider_are_unsupported() {
        assert_eq!(
            Clock::new(LPSPI_CLK_HZ, 1_000),
            Err(AbiError::Unsupported),
            "ABI minimum 1 kHz"
        );
        assert_eq!(Clock::new(LPSPI_CLK_HZ, 2_000), Err(AbiError::Unsupported));
        let slowest = Clock::new(LPSPI_CLK_HZ, 2_007).unwrap();
        assert_eq!(
            (slowest.prescale, slowest.sckdiv, slowest.divisor()),
            (7, 255, 32_896)
        );
        assert_eq!(Clock::new(0, 1000), Err(AbiError::InvalidArgument));
        assert_eq!(Clock::new(LPSPI_CLK_HZ, 0), Err(AbiError::InvalidArgument));
        assert_eq!(Clock::new(u32::MAX, 1), Err(AbiError::Unsupported));
        assert_eq!(Clock::new(1, u32::MAX).map(|c| c.divisor()), Ok(2));
    }

    #[test]
    fn rt16_modes_and_registers() {
        assert_eq!(mode_bits(0), Ok((false, false)));
        assert_eq!(mode_bits(1), Ok((false, true)));
        assert_eq!(mode_bits(2), Ok((true, false)));
        assert_eq!(mode_bits(3), Ok((true, true)));
        assert_eq!(mode_bits(4), Err(AbiError::InvalidArgument));
        let c = Clock::new(LPSPI_CLK_HZ, 100_000).unwrap();
        assert_eq!(tcr(0, &c), Ok(2 << 27 | 7));
        assert_eq!(tcr(3, &c), Ok(1 << 31 | 1 << 30 | 2 << 27 | 7));
        assert_eq!(tcr(1, &c), Ok(1 << 30 | 2 << 27 | 7));
        assert_eq!(tcr(4, &c), Err(AbiError::InvalidArgument));
        // 1 MHz: SCKDIV 64, DBT 64, PCSSCK = SCKPCS = 32.
        let c = Clock::new(LPSPI_CLK_HZ, 1_000_000).unwrap();
        assert_eq!(c.ccr(), 64 | 64 << 8 | 32 << 16 | 32 << 24);
        // Ten SCK periods a frame.
        assert_eq!(c.frame_cycles(), 10 * c.divisor());
        let fastest = Clock {
            prescale: 0,
            sckdiv: 0,
        };
        assert_eq!((fastest.half_delay(), fastest.frame_cycles()), (0, 20));
        assert_eq!(
            Clock {
                prescale: 7,
                sckdiv: 255
            }
            .half_delay(),
            127
        );
        assert_eq!(CFGR1 & 1, 1);
        assert_eq!(check_status(sr::TDF | sr::RDF | sr::TCF | sr::MBF), Ok(()));
        assert_eq!(check_status(sr::TEF), Err(AbiError::Transport));
        assert_eq!(check_status(sr::REF), Err(AbiError::Transport));
    }

    /// Runs a `len`-byte transfer against a fake LPSPI: a transmit FIFO,
    /// a shift stage that moves at most `rate` frames a step, a receive
    /// FIFO; the device echoes each byte plus one. Returns the steps.
    fn run(len: usize, rate: usize) -> usize {
        let mut buf: heapless::Vec<u8, MAX_DMA> = (0..len).map(|i| i as u8).collect();
        let mut txf: heapless::Deque<u8, FIFO_DEPTH> = heapless::Deque::new();
        let mut rxf: heapless::Deque<u8, FIFO_DEPTH> = heapless::Deque::new();
        let mut p = Pump::new(len);
        let mut steps = 0;
        while !p.done() {
            steps += 1;
            assert!(steps <= 2 * len + 2, "the pump stalled");
            let step = p.step(txf.len(), rxf.len());
            assert!(step.pop.len() <= FIFO_DEPTH && step.push.len() <= FIFO_DEPTH);
            for i in step.pop {
                buf[i] = rxf.pop_front().expect("popped only what arrived");
            }
            for i in step.push {
                assert!(i >= p.received(), "in place: never push a received byte");
                txf.push_back(buf[i]).expect("pushed only into room");
            }
            assert!(p.in_flight() <= FIFO_DEPTH);
            // The bus: the shift stage stalls rather than overrun (NOSTALL 0).
            for _ in 0..rate {
                if rxf.is_full() {
                    break;
                }
                match txf.pop_front() {
                    Some(b) => rxf.push_back(b.wrapping_add(1)).unwrap(),
                    None => break,
                }
            }
        }
        for (i, b) in buf.iter().enumerate() {
            assert_eq!(*b, (i as u8).wrapping_add(1), "byte {i}");
        }
        steps
    }

    #[test]
    fn rt16_the_pump_moves_every_byte_in_bounded_steps() {
        for len in [1, 15, 16, 17, 64, MAX_DMA] {
            // A fast bus drains each step; a slow one moves a byte a step.
            assert!(
                run(len, FIFO_DEPTH) <= len.div_ceil(FIFO_DEPTH) + 1,
                "{len}"
            );
            assert!(run(len, 1) <= len + 1, "{len}");
        }
        let mut p = Pump::new(20);
        assert_eq!(
            p.step(0, 0),
            Step {
                pop: 0..0,
                push: 0..16
            }
        );
        assert_eq!(p.in_flight(), 16);
        // Nothing back yet: nothing more may go.
        assert_eq!(
            p.step(0, 0),
            Step {
                pop: 0..0,
                push: 16..16
            }
        );
        // Five back, transmit FIFO with room: five more out.
        assert_eq!(
            p.step(11, 5),
            Step {
                pop: 0..5,
                push: 16..20
            }
        );
        // Bogus levels never pop more than is owed or push past the end.
        assert_eq!(
            p.step(0, 99),
            Step {
                pop: 5..20,
                push: 20..20
            }
        );
        assert!(p.done() && p.sent() == 20);
        assert_eq!(
            p.step(99, 99),
            Step {
                pop: 20..20,
                push: 20..20
            }
        );
        // A full transmit FIFO takes nothing.
        let mut p = Pump::new(4);
        assert_eq!(p.step(FIFO_DEPTH, 0).push, 0..0);
        assert_eq!(
            p.step(usize::MAX, usize::MAX),
            Step {
                pop: 0..0,
                push: 0..0
            }
        );
        assert!(Pump::new(0).done() && Pump::new(0).is_empty() && Pump::new(3).len() == 3);
    }

    #[test]
    fn rt16_every_transfer_is_bounded_in_time() {
        let c = Clock::new(LPSPI_CLK_HZ, 1_000_000).unwrap();
        // 16 bytes at 1 MHz: 10 µs a frame, 160 µs on the bus.
        assert_eq!(c.timeout_us(LPSPI_CLK_HZ, 16), 320 + MARGIN_US);
        // The longest transfer at the slowest clock: about 41 s.
        let slow = Clock::new(LPSPI_CLK_HZ, 2_007).unwrap();
        let worst = slow.timeout_us(LPSPI_CLK_HZ, MAX_DMA);
        assert!(worst > 30_000_000 && worst < 50_000_000, "{worst}");
        assert!(c.timeout_us(0, 1) > 0);
        assert_eq!(c.timeout_us(1, usize::MAX), u64::MAX);
    }
}
