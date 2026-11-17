//! LPI2C1 decisions (I2C bus 0, SDA A4 / SCL A5): the master clock
//! registers, the command words of one transfer, its time bound and the
//! decoding of the master status flags.
//!
//! # Clock
//!
//! The LPI2C master divides its functional clock (the LPI2C clock root,
//! [`LPI2C_CLK_HZ`]) by `2^PRESCALE` (`MCFGR1.PRESCALE`, 0..=7) and counts
//! SCL in those prescaled cycles (`MCCR0`):
//!
//! ```text
//! SCL period = (CLKLO + 1) + (CLKHI + 1) + SCL_LATENCY   prescaled cycles
//! SCL_LATENCY = (2 + FILTSCL) >> PRESCALE                 (FILTSCL = 0 here)
//! ```
//!
//! [`Timing::new`] takes the smallest prescaler whose period fits
//! (`CLKLO` 3..=63, `CLKHI` 1..=63), rounds the period *up* so the bus
//! never runs faster than the payload asked, and splits it 60 % low, 40 %
//! high (I2C's `tLOW` is the longer minimum in every mode). `SETHOLD`
//! (START hold, repeated-START and STOP setup) is the low time, which
//! covers `tSU;STA` (4.7 µs at 100 kHz); `DATAVD` (data hold after SCL
//! falls) is half the high time, inside `tVD;DAT`. The glitch filters
//! (`MCFGR2.FILTSCL`/`FILTSDA`) and the bus-idle timeout stay 0.
//! `MCFGR3.PINLOW` makes the controller flag `PLTF` when SCL or SDA stays
//! low for [`PIN_LOW_TIMEOUT_US`], a target stretching the clock for good.
//!
//! On the 24 MHz oscillator 100 kHz is `PRESCALE` 2 (6 MHz, 60 cycles) and
//! 400 kHz `PRESCALE` 0 (60 cycles), both exact; on the 60 MHz PLL3 root
//! they are `PRESCALE` 3 and 1, 75 cycles each. Every clock the ABI admits
//! (10 kHz to 1 MHz) has a timing on both roots.
//!
//! # Commands
//!
//! The controller runs the words written to `MTDR` ([`Cmd::mtdr`]): START
//! with the address byte, TRANSMIT one byte, RECEIVE `DATA + 1` bytes (one
//! command per [`RECEIVE_CHUNK`] bytes), STOP. [`commands`] gives the words
//! of one transfer:
//!
//! | transfer        | words                                                  |
//! |-----------------|--------------------------------------------------------|
//! | write           | START(addr W), TRANSMIT × n, STOP                      |
//! | read            | START(addr R), RECEIVE(n), STOP                        |
//! | write then read | START(addr W), TRANSMIT × n, START(addr R), RECEIVE(m), STOP |
//! | address probe   | START(addr W), STOP                                    |
//!
//! Unlike the RP2040's controller, the LPI2C can address a target without
//! a data byte, so an address-only probe (`tx_len` = `rx_len` = 0) is
//! allowed ([`check_lengths`]); a NACK of the address fails it.
//!
//! The transmit and receive FIFOs hold [`FIFO_DEPTH`] words: the shell
//! waits for room (`MFSR.TXCOUNT < FIFO_DEPTH`) before each word and
//! drains `MRDR` ([`rx_byte`]) while it waits, so the receive FIFO never
//! fills; after the last word it waits for `MSR.SDF` (the STOP).
//!
//! # Errors and bounds
//!
//! Every wait goes through [`poll`]: the `MSR` error flags first
//! ([`Fault::from_msr`]: `PLTF`, `ALF`, `NDF`, `FEF`), then the condition,
//! then one poll of the transfer's [`Budget`] ([`budget`], from
//! [`timeout_us`]: twice the bus time plus [`MARGIN_US`]). Every fault
//! reaches the payload as [`AbiError::Transport`] ([`Fault::abi`]), as ABI
//! 3 specifies. After a fault the shell clears the flags
//! ([`msr::CLEAR`]), resets both FIFOs (`MCR.RTF | MCR.RRF`), sends a STOP
//! if the controller still owns the bus ([`Fault::needs_stop`]) and, if it
//! is still busy after that, resets the controller (`MCR.RST`) and writes
//! the timing again, so a wedged controller never carries over to the
//! next transfer.

use super::Budget;
use middleware::wasm_abi::AbiError;
use middleware::wasm_abi::periph::{I2C_FREQ_HZ, MAX_I2C};

/// The 24 MHz oscillator as the LPI2C clock root (`CSCDR2.LPI2C_CLK_SEL`
/// = 1, `LPI2C_CLK_PODF` = 0, divide by 1).
pub const LPI2C_CLK_OSC_HZ: u32 = 24_000_000;

/// PLL3 / 8 as the LPI2C clock root (`CSCDR2.LPI2C_CLK_SEL` = 0,
/// `LPI2C_CLK_PODF` = 0): the alternative root, needs PLL3 running.
pub const LPI2C_CLK_PLL3_HZ: u32 = 60_000_000;

/// The LPI2C clock root the shell configures: the oscillator.
pub const LPI2C_CLK_HZ: u32 = LPI2C_CLK_OSC_HZ;

/// Depth of the LPI2C's transmit and receive FIFOs on the RT1050, in
/// words.
pub const FIFO_DEPTH: u8 = 4;

/// Most bytes one RECEIVE command reads (`DATA + 1`, `DATA` 0..=255).
pub const RECEIVE_CHUNK: usize = 256;

/// How long SCL or SDA may stay low before the controller flags `PLTF`,
/// in µs (SMBus's `tTIMEOUT`).
pub const PIN_LOW_TIMEOUT_US: u32 = 25_000;

/// Slack added to every transfer's time bound, in µs: more than
/// [`PIN_LOW_TIMEOUT_US`], so a target stretching the clock is reported by
/// the controller's own pin-low flag before the deadline.
pub const MARGIN_US: u32 = 30_000;

/// `MCR` bits.
pub mod mcr {
    /// Master enable.
    pub const MEN: u32 = 1 << 0;
    /// Software reset of the master logic.
    pub const RST: u32 = 1 << 1;
    /// Reset the transmit FIFO.
    pub const RTF: u32 = 1 << 8;
    /// Reset the receive FIFO.
    pub const RRF: u32 = 1 << 9;
}

/// `MSR` bits.
pub mod msr {
    /// Transmit data (FIFO at or below the watermark).
    pub const TDF: u32 = 1 << 0;
    /// Receive data (FIFO above the watermark).
    pub const RDF: u32 = 1 << 1;
    /// End packet (a STOP or repeated START was generated).
    pub const EPF: u32 = 1 << 8;
    /// STOP detected.
    pub const SDF: u32 = 1 << 9;
    /// NACK detected (of the address or of a written byte).
    pub const NDF: u32 = 1 << 10;
    /// Arbitration lost.
    pub const ALF: u32 = 1 << 11;
    /// FIFO error (data without a START, or a command after a fault).
    pub const FEF: u32 = 1 << 12;
    /// Pin low timeout (`MCFGR3.PINLOW`).
    pub const PLTF: u32 = 1 << 13;
    /// Data match.
    pub const DMF: u32 = 1 << 14;
    /// Master busy.
    pub const MBF: u32 = 1 << 24;
    /// Bus busy.
    pub const BBF: u32 = 1 << 25;
    /// The flags [`super::Fault::from_msr`] reports.
    pub const ERRORS: u32 = NDF | ALF | FEF | PLTF;
    /// Every write-one-to-clear flag: written back to `MSR` before a
    /// transfer and after a fault.
    pub const CLEAR: u32 = EPF | SDF | NDF | ALF | FEF | PLTF | DMF;
}

/// `MRDR.RXEMPTY`: the receive FIFO was empty, `DATA` is not a byte.
pub const MRDR_RXEMPTY: u32 = 1 << 14;

/// The byte of an `MRDR` read, `None` when the receive FIFO was empty.
pub const fn rx_byte(mrdr: u32) -> Option<u8> {
    if mrdr & MRDR_RXEMPTY != 0 {
        None
    } else {
        Some((mrdr & 0xFF) as u8)
    }
}

/// The master clock registers for one SCL frequency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// `MCFGR1.PRESCALE`: the functional clock is divided by
    /// `2^prescale`, 0..=7.
    pub prescale: u8,
    /// `MCCR0.CLKLO`: SCL low, in prescaled cycles minus one, 3..=63.
    pub clklo: u8,
    /// `MCCR0.CLKHI`: SCL high, in prescaled cycles minus one (before the
    /// SCL latency), 1..=63.
    pub clkhi: u8,
    /// `MCCR0.SETHOLD`: START hold and STOP / repeated-START setup, in
    /// prescaled cycles minus one.
    pub sethold: u8,
    /// `MCCR0.DATAVD`: data hold after SCL falls, in prescaled cycles
    /// minus one.
    pub datavd: u8,
    /// `MCFGR3.PINLOW`: pin-low timeout in units of 256 prescaled cycles,
    /// 1..=4095.
    pub pinlow: u16,
}

impl Timing {
    /// The timing for `freq_hz` on an LPI2C clock root of `clk_hz` (see
    /// the module docs): `InvalidArgument` outside the ABI's range or for
    /// a zero root, `Unsupported` when no prescaler fits the period.
    pub fn new(clk_hz: u32, freq_hz: u32) -> Result<Self, AbiError> {
        let in_range = i32::try_from(freq_hz).is_ok_and(|f| I2C_FREQ_HZ.contains(&f));
        if !in_range || clk_hz == 0 {
            return Err(AbiError::InvalidArgument);
        }
        let narrow = |v: u64| u8::try_from(v).map_err(|_| AbiError::Unsupported);
        // Eight prescalers: bounded.
        for prescale in 0u8..=7 {
            let total = u64::from(clk_hz).div_ceil(u64::from(freq_hz) << prescale);
            let latency = 2u64 >> prescale;
            let lo_ticks = total * 3 / 5;
            let hi_ticks = total - lo_ticks;
            let (Some(clklo), Some(clkhi)) =
                (lo_ticks.checked_sub(1), hi_ticks.checked_sub(1 + latency))
            else {
                continue;
            };
            if !(3..=63).contains(&clklo) || !(1..=63).contains(&clkhi) {
                continue;
            }
            let pclk = u64::from(clk_hz) >> prescale;
            let pinlow = (u64::from(PIN_LOW_TIMEOUT_US) * pclk).div_ceil(1_000_000 * 256);
            return Ok(Timing {
                prescale,
                clklo: narrow(clklo)?,
                clkhi: narrow(clkhi)?,
                sethold: narrow(clklo)?,
                datavd: narrow(clkhi / 2)?,
                pinlow: u16::try_from(pinlow.clamp(1, 4095)).map_err(|_| AbiError::Unsupported)?,
            });
        }
        Err(AbiError::Unsupported)
    }

    /// `MCFGR1`: `PRESCALE`; `PINCFG` 0 (open drain), `IGNACK` 0,
    /// `AUTOSTOP` 0.
    pub const fn mcfgr1(&self) -> u32 {
        (self.prescale & 0x7) as u32
    }

    /// `MCCR0`: `CLKLO` `[5:0]`, `CLKHI` `[13:8]`, `SETHOLD` `[21:16]`,
    /// `DATAVD` `[29:24]`.
    pub const fn mccr0(&self) -> u32 {
        (self.clklo as u32 & 0x3F)
            | (self.clkhi as u32 & 0x3F) << 8
            | (self.sethold as u32 & 0x3F) << 16
            | (self.datavd as u32 & 0x3F) << 24
    }

    /// `MCFGR3`: `PINLOW` `[19:8]`.
    pub const fn mcfgr3(&self) -> u32 {
        (self.pinlow as u32 & 0xFFF) << 8
    }

    /// The SCL latency (`2 >> PRESCALE`), in prescaled cycles.
    pub const fn latency(&self) -> u32 {
        2 >> self.prescale
    }

    /// One SCL period in root clock cycles.
    pub const fn period_cycles(&self) -> u32 {
        (self.clklo as u32 + 1 + self.clkhi as u32 + 1 + self.latency()) << self.prescale
    }

    /// The SCL frequency it produces from `clk_hz` (at most the request).
    pub const fn hz(&self, clk_hz: u32) -> u32 {
        clk_hz / self.period_cycles()
    }
}

/// One `MTDR` command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    /// (Repeated) START, then the address byte (`addr7 << 1 | R/W`).
    Start {
        /// The address byte.
        addr_byte: u8,
    },
    /// Transmit one byte.
    Transmit(u8),
    /// Receive `count` bytes, 1..=[`RECEIVE_CHUNK`] (the last one NACKed
    /// before the STOP).
    Receive {
        /// Bytes to receive.
        count: u16,
    },
    /// STOP.
    Stop,
}

impl Cmd {
    /// The `MTDR` word: `CMD` `[10:8]`, `DATA` `[7:0]`. A `Receive` count
    /// outside 1..=256 is clamped into it.
    pub const fn mtdr(self) -> u32 {
        match self {
            Cmd::Transmit(byte) => byte as u32,
            Cmd::Receive { count } => {
                let data = if count == 0 {
                    0
                } else if count > 256 {
                    255
                } else {
                    count - 1
                };
                0b001 << 8 | data as u32
            }
            Cmd::Stop => 0b010 << 8,
            Cmd::Start { addr_byte } => 0b100 << 8 | addr_byte as u32,
        }
    }

    /// Bytes this command puts in the receive FIFO (`MTDR` clamping
    /// included).
    pub const fn rx_bytes(self) -> usize {
        match self {
            Cmd::Receive { count } => {
                if count == 0 {
                    1
                } else if count > 256 {
                    256
                } else {
                    count as usize
                }
            }
            _ => 0,
        }
    }
}

/// Whether a transfer of these lengths can run: each at most [`MAX_I2C`]
/// (`InvalidArgument`). Both zero is an address-only probe, which the
/// LPI2C can issue.
pub const fn check_lengths(tx_len: usize, rx_len: usize) -> Result<(), AbiError> {
    if tx_len > MAX_I2C || rx_len > MAX_I2C {
        return Err(AbiError::InvalidArgument);
    }
    Ok(())
}

/// The commands of one transfer to 7-bit `addr` (masked to 7 bits): the
/// write phase (START W and the `tx` bytes) when there is something to
/// write or nothing to read, the read phase (START R and `rx_len` bytes in
/// [`RECEIVE_CHUNK`] commands) when there is something to read, then STOP.
pub fn commands(addr: u8, tx: &[u8], rx_len: usize) -> impl Iterator<Item = Cmd> + '_ {
    let addr = addr & 0x7F;
    let write = !tx.is_empty() || rx_len == 0;
    let start_w = write.then_some(Cmd::Start {
        addr_byte: addr << 1,
    });
    let start_r = (rx_len > 0).then_some(Cmd::Start {
        addr_byte: addr << 1 | 1,
    });
    let chunks = (0..rx_len.div_ceil(RECEIVE_CHUNK)).map(move |i| Cmd::Receive {
        count: (rx_len - i * RECEIVE_CHUNK).min(RECEIVE_CHUNK) as u16,
    });
    start_w
        .into_iter()
        .chain(tx.iter().map(|&b| Cmd::Transmit(b)))
        .chain(start_r)
        .chain(chunks)
        .chain(core::iter::once(Cmd::Stop))
}

/// The time bound of a transfer, in µs: twice its bus time at `freq_hz`
/// (9 clocks per byte and per address byte, one per START and the STOP)
/// plus [`MARGIN_US`].
pub fn timeout_us(freq_hz: u32, tx_len: usize, rx_len: usize) -> u32 {
    let phases = usize::from(tx_len > 0 || rx_len == 0) + usize::from(rx_len > 0);
    let bytes = tx_len.saturating_add(rx_len).saturating_add(phases) as u64;
    let clocks = bytes.saturating_mul(9).saturating_add(phases as u64 + 1);
    let freq = u64::from(freq_hz.max(1));
    let bus_us = (clocks.saturating_mul(1_000_000)).div_ceil(freq);
    u32::try_from(
        bus_us
            .saturating_mul(2)
            .saturating_add(u64::from(MARGIN_US)),
    )
    .unwrap_or(u32::MAX)
}

/// The poll budget of a transfer: [`timeout_us`] in polls.
pub fn budget(freq_hz: u32, tx_len: usize, rx_len: usize) -> Budget {
    Budget::from_us(u64::from(timeout_us(freq_hz, tx_len, rx_len)))
}

/// Why a transfer failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// `NDF`: the target did not acknowledge its address (no such
    /// device) or a written byte; the LPI2C does not say which.
    Nack,
    /// `ALF`: another controller won arbitration.
    ArbitrationLost,
    /// `FEF`: the controller refused a command (data without a START).
    FifoError,
    /// `PLTF`: SCL or SDA stayed low for [`PIN_LOW_TIMEOUT_US`].
    PinLowTimeout,
    /// The transfer's [`Budget`] ran out.
    Timeout,
}

impl Fault {
    /// What the payload sees: always `Transport` (ABI 3).
    pub const fn abi(self) -> AbiError {
        AbiError::Transport
    }

    /// The fault an `MSR` value reports, `None` without an error flag. A
    /// stuck pin outranks lost arbitration, which outranks a NACK, which
    /// outranks the FIFO error that follows it.
    pub const fn from_msr(msr: u32) -> Option<Self> {
        if msr & msr::PLTF != 0 {
            Some(Fault::PinLowTimeout)
        } else if msr & msr::ALF != 0 {
            Some(Fault::ArbitrationLost)
        } else if msr & msr::NDF != 0 {
            Some(Fault::Nack)
        } else if msr & msr::FEF != 0 {
            Some(Fault::FifoError)
        } else {
            None
        }
    }

    /// Whether the shell should send a STOP after clearing the flags and
    /// the FIFOs: the controller still owns the bus (`MSR.MBF`, read after
    /// the clear) and did not lose it to another controller.
    pub const fn needs_stop(self, msr_after_clear: u32) -> bool {
        !matches!(self, Fault::ArbitrationLost) && msr_after_clear & msr::MBF != 0
    }
}

/// One bounded wait step: `Err` if `msr` reports a fault, `Ok(true)` once
/// `ready`, else spends one poll of `budget` (`Ok(false)`, or
/// `Err(Fault::Timeout)` once it is gone).
pub fn poll(msr: u32, ready: bool, budget: &mut Budget) -> Result<bool, Fault> {
    if let Some(fault) = Fault::from_msr(msr) {
        return Err(fault);
    }
    if ready {
        return Ok(true);
    }
    if budget.spend() {
        Ok(false)
    } else {
        Err(Fault::Timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOTS: [u32; 2] = [LPI2C_CLK_OSC_HZ, LPI2C_CLK_PLL3_HZ];

    #[test]
    fn rt16_every_abi_clock_has_a_timing_on_both_roots() {
        for clk in ROOTS {
            for freq in [10_000, 50_000, 100_000, 400_000, 1_000_000] {
                let t = Timing::new(clk, freq).unwrap_or_else(|e| panic!("{clk} {freq}: {e:?}"));
                let actual = t.hz(clk);
                assert!(actual <= freq, "{clk} {freq}: {actual} faster than asked");
                assert!(
                    actual.abs_diff(freq) * 50 <= freq,
                    "{clk} {freq}: {actual} off by > 2 %"
                );
                assert!(
                    (3..=63).contains(&t.clklo) && (1..=63).contains(&t.clkhi),
                    "{t:?}"
                );
                assert!(t.prescale <= 7 && t.sethold <= 63 && t.datavd <= 63);
                // Low at least as long as high (latency included), data
                // valid inside the low time.
                assert!(
                    u32::from(t.clklo) + 1 >= u32::from(t.clkhi) + 1 + t.latency(),
                    "{t:?}"
                );
                assert!(t.datavd < t.clklo);
                assert!((1..=4095).contains(&t.pinlow));
            }
        }
    }

    #[test]
    fn rt16_standard_and_fast_mode_registers() {
        // 100 kHz on the oscillator: 6 MHz prescaled, 60 cycles, 36 low.
        let t = Timing::new(LPI2C_CLK_OSC_HZ, 100_000).unwrap();
        assert_eq!(
            t,
            Timing {
                prescale: 2,
                clklo: 35,
                clkhi: 23,
                sethold: 35,
                datavd: 11,
                pinlow: 586
            }
        );
        assert_eq!(t.hz(LPI2C_CLK_OSC_HZ), 100_000);
        assert_eq!(t.mcfgr1(), 2);
        assert_eq!(t.mccr0(), 35 | 23 << 8 | 35 << 16 | 11 << 24);
        assert_eq!(t.mcfgr3(), 586 << 8);
        // tLOW 6 µs ≥ 4.7, tHIGH 4 µs ≥ 4.0, tSU;STA 6 µs ≥ 4.7.
        assert_eq!(u32::from(t.clklo + 1) * 1_000_000 / 6_000_000, 6);
        // 400 kHz on the oscillator: no prescaler, the 2-cycle latency.
        let t = Timing::new(LPI2C_CLK_OSC_HZ, 400_000).unwrap();
        assert_eq!((t.prescale, t.clklo, t.clkhi, t.latency()), (0, 35, 21, 2));
        assert_eq!(t.hz(LPI2C_CLK_OSC_HZ), 400_000);
        // tLOW 1.5 µs ≥ 1.3, tVD;DAT (11 cycles) 0.46 µs ≤ 0.9.
        assert!(u64::from(t.datavd + 1) * 1_000_000_000 / u64::from(LPI2C_CLK_OSC_HZ) <= 900);
        // The PLL3 root: prescalers 3 and 1, 75 cycles.
        let t = Timing::new(LPI2C_CLK_PLL3_HZ, 100_000).unwrap();
        assert_eq!((t.prescale, t.hz(LPI2C_CLK_PLL3_HZ)), (3, 100_000));
        let t = Timing::new(LPI2C_CLK_PLL3_HZ, 400_000).unwrap();
        assert_eq!(
            (t.prescale, t.clklo, t.clkhi, t.hz(LPI2C_CLK_PLL3_HZ)),
            (1, 44, 28, 400_000)
        );
        // The pin-low register saturates on a fast unprescaled root.
        assert_eq!(
            Timing::new(600_000_000, 1_000_000).map(|t| t.pinlow),
            Ok(4095)
        );
    }

    #[test]
    fn rt16_clocks_outside_the_abi_or_the_dividers_are_refused() {
        assert_eq!(
            Timing::new(LPI2C_CLK_HZ, 9_999),
            Err(AbiError::InvalidArgument)
        );
        assert_eq!(
            Timing::new(LPI2C_CLK_HZ, 1_000_001),
            Err(AbiError::InvalidArgument)
        );
        assert_eq!(
            Timing::new(LPI2C_CLK_HZ, u32::MAX),
            Err(AbiError::InvalidArgument)
        );
        assert_eq!(Timing::new(0, 100_000), Err(AbiError::InvalidArgument));
        // Too slow a root for the shortest low count, too fast for the
        // largest prescaler.
        assert_eq!(
            Timing::new(4_000_000, 1_000_000),
            Err(AbiError::Unsupported)
        );
        assert_eq!(Timing::new(u32::MAX, 10_000), Err(AbiError::Unsupported));
        assert_eq!(Timing::new(1, 10_000), Err(AbiError::Unsupported));
    }

    fn words(addr: u8, tx: &[u8], rx: usize) -> heapless::Vec<u32, 160> {
        commands(addr, tx, rx).map(Cmd::mtdr).collect()
    }

    #[test]
    fn rt16_transfers_are_command_words() {
        const START: u32 = 0x400;
        const STOP: u32 = 0x200;
        const RECV: u32 = 0x100;
        // WHO_AM_I of the FXOS8700CQ (0x1F): write 0x0D, read 1.
        assert_eq!(
            words(0x1F, &[0x0D], 1).as_slice(),
            &[START | 0x3E, 0x0D, START | 0x3F, RECV, STOP]
        );
        assert_eq!(
            words(0x1A, &[1, 2], 0).as_slice(),
            &[START | 0x34, 1, 2, STOP]
        );
        assert_eq!(
            words(0x1A, &[], 3).as_slice(),
            &[START | 0x35, RECV | 2, STOP]
        );
        // The address probe.
        assert_eq!(words(0x50, &[], 0).as_slice(), &[START | 0xA0, STOP]);
        // The address is masked to 7 bits, never overflows.
        assert_eq!(words(0xFF, &[], 0).as_slice(), &[START | 0xFE, STOP]);
        // The ABI's largest: one RECEIVE of 64.
        assert_eq!(commands(0x10, &[0; MAX_I2C], MAX_I2C).count(), MAX_I2C + 4);
        assert_eq!(
            commands(0x10, &[], MAX_I2C)
                .map(Cmd::rx_bytes)
                .sum::<usize>(),
            MAX_I2C
        );
        // Long reads are split into 256-byte RECEIVE commands.
        let cmds: heapless::Vec<Cmd, 8> = commands(0x10, &[], 600).collect();
        assert_eq!(
            cmds.as_slice(),
            &[
                Cmd::Start { addr_byte: 0x21 },
                Cmd::Receive { count: 256 },
                Cmd::Receive { count: 256 },
                Cmd::Receive { count: 88 },
                Cmd::Stop
            ]
        );
        assert_eq!(Cmd::Receive { count: 256 }.mtdr(), RECV | 255);
        assert_eq!(
            commands(0x10, &[], 512)
                .filter(|c| matches!(c, Cmd::Receive { .. }))
                .count(),
            2
        );
        // Out-of-range counts are clamped consistently.
        assert_eq!(
            (
                Cmd::Receive { count: 0 }.mtdr(),
                Cmd::Receive { count: 0 }.rx_bytes()
            ),
            (RECV, 1)
        );
        assert_eq!(
            (
                Cmd::Receive { count: 999 }.mtdr(),
                Cmd::Receive { count: 999 }.rx_bytes()
            ),
            (RECV | 255, 256)
        );
        assert_eq!(check_lengths(0, 0), Ok(()), "address probes are allowed");
        assert_eq!(check_lengths(MAX_I2C, MAX_I2C), Ok(()));
        assert_eq!(
            check_lengths(MAX_I2C + 1, 0),
            Err(AbiError::InvalidArgument)
        );
        assert_eq!(
            check_lengths(0, MAX_I2C + 1),
            Err(AbiError::InvalidArgument)
        );
        assert_eq!(rx_byte(0x00AB), Some(0xAB));
        assert_eq!(rx_byte(MRDR_RXEMPTY | 0xAB), None);
    }

    #[test]
    fn rt16_every_transfer_is_bounded_in_time_and_polls() {
        // A register read at 100 kHz: 9 × (1 + 1 + 2) + 2 + 1 = 39 clocks.
        assert_eq!(timeout_us(100_000, 1, 1), 2 * 390 + MARGIN_US);
        // The probe: 9 + 1 + 1 clocks.
        assert_eq!(timeout_us(100_000, 0, 0), 2 * 110 + MARGIN_US);
        let worst = timeout_us(10_000, MAX_I2C, MAX_I2C);
        assert!(worst < 300_000, "{worst}");
        assert!(timeout_us(0, 1, 1) > 0, "a zero clock is clamped");
        assert_eq!(timeout_us(1, usize::MAX, usize::MAX), u32::MAX, "saturates");
        assert!(timeout_us(100_000, 2, 0) > timeout_us(100_000, 1, 0));
        assert!(timeout_us(50_000, 1, 0) > timeout_us(100_000, 1, 0));
        const { assert!(MARGIN_US > PIN_LOW_TIMEOUT_US) };
        assert_eq!(
            budget(100_000, 1, 1),
            Budget::from_us(u64::from(timeout_us(100_000, 1, 1)))
        );
    }

    #[test]
    fn rt16_status_flags_decode_to_transport() {
        assert_eq!(Fault::from_msr(0), None);
        assert_eq!(
            Fault::from_msr(msr::TDF | msr::RDF | msr::SDF | msr::MBF | msr::BBF),
            None
        );
        assert_eq!(Fault::from_msr(msr::NDF), Some(Fault::Nack));
        assert_eq!(Fault::from_msr(msr::NDF | msr::FEF), Some(Fault::Nack));
        assert_eq!(Fault::from_msr(msr::FEF), Some(Fault::FifoError));
        assert_eq!(
            Fault::from_msr(msr::ALF | msr::NDF),
            Some(Fault::ArbitrationLost)
        );
        assert_eq!(Fault::from_msr(msr::ERRORS), Some(Fault::PinLowTimeout));
        for f in [
            Fault::Nack,
            Fault::ArbitrationLost,
            Fault::FifoError,
            Fault::PinLowTimeout,
            Fault::Timeout,
        ] {
            assert_eq!(f.abi(), AbiError::Transport);
        }
        assert!(Fault::Nack.needs_stop(msr::MBF));
        assert!(!Fault::Nack.needs_stop(0));
        assert!(!Fault::ArbitrationLost.needs_stop(msr::MBF));
        assert_eq!(msr::CLEAR & msr::ERRORS, msr::ERRORS);
        assert_eq!(msr::CLEAR & (msr::MBF | msr::BBF | msr::TDF | msr::RDF), 0);
    }

    /// A fake LPI2C: answers each transfer's commands, NACKing the
    /// address when `present` is false.
    fn run(
        addr: u8,
        tx: &[u8],
        rx: &mut [u8],
        present: bool,
        budget_polls: u32,
    ) -> Result<(), Fault> {
        let mut budget = Budget::new(budget_polls);
        let mut msr = 0u32;
        let mut fifo: heapless::Deque<u8, 4> = heapless::Deque::new();
        let mut slots = rx.iter_mut();
        let mut next = 0x40u8;
        let rx_len = slots.len();
        for cmd in commands(addr, tx, rx_len) {
            while !poll(msr, true, &mut budget)? {}
            match cmd {
                Cmd::Start { .. } if !present => msr |= msr::NDF,
                Cmd::Receive { count } => {
                    for _ in 0..count {
                        let _ = fifo.push_back(next);
                        next = next.wrapping_add(1);
                        // Drain while waiting.
                        while let Some(b) = fifo.pop_front() {
                            if let Some(s) = slots.next() {
                                *s = b;
                            }
                        }
                    }
                }
                Cmd::Stop => msr |= msr::SDF,
                _ => {}
            }
        }
        while !poll(msr, msr & msr::SDF != 0, &mut budget)? {}
        Ok(())
    }

    #[test]
    fn rt16_a_fake_bus_runs_reads_nacks_and_timeouts() {
        let mut rx = [0u8; 3];
        assert_eq!(run(0x1F, &[0x0D], &mut rx, true, 10), Ok(()));
        assert_eq!(rx, [0x40, 0x41, 0x42]);
        assert_eq!(run(0x1F, &[0x0D], &mut rx, false, 10), Err(Fault::Nack));
        assert_eq!(
            run(0x1F, &[], &mut [], false, 10).map_err(Fault::abi),
            Err(AbiError::Transport)
        );
        // A STOP that never comes ends with the budget.
        let mut budget = Budget::new(3);
        let mut polls = 0;
        let r = loop {
            polls += 1;
            match poll(msr::MBF, false, &mut budget) {
                Ok(false) => continue,
                other => break other,
            }
        };
        assert_eq!((r, polls), (Err(Fault::Timeout), 4));
    }
}
