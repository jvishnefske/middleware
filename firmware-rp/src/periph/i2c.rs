//! I2C bus decisions (V16): clock timing, the command sequence of one
//! transfer, its time bound and the error mapping.
//!
//! The RP2040 and RP2350 I2C controllers (Synopsys DW_apb_i2c) run as
//! controllers in fast mode: one SCL period of `clk_peri / freq` cycles,
//! 60 % low and 40 % high, a spike filter of a sixteenth of the low time,
//! and an SDA hold of 300 ns (120 ns in Fast-mode Plus), as the Pico SDK's
//! `i2c_set_baudrate` sets them ([`Timing::new`]). Every clock the ABI
//! admits (10 kHz to 1 MHz) fits at the default `clk_peri` of both chips
//! (125 MHz, 150 MHz); 1 MHz needs strong external pull-ups, the internal
//! ones (about 50 kΩ) only suit slow buses.
//!
//! A transfer ([`commands`]) writes `tx`, then reads `rx_len` bytes after a
//! repeated start, with a stop after the last byte. Either part may be
//! empty, but not both: the controller cannot address a target without a
//! data byte, so an address-only probe is `Unsupported`
//! ([`check_lengths`]), which ABI 3 allows.
//!
//! Every transfer is bounded in time: the shell gives up after
//! [`timeout_us`], twice the bus time of its bytes plus [`MARGIN_US`] for
//! clock stretching (at most 237 ms, 64 bytes each way at 10 kHz), aborts
//! the controller and reports [`Fault::Timeout`]. Every fault — the target
//! not acknowledging its address or a data byte, lost arbitration, the
//! timeout — reaches the payload as [`AbiError::Transport`] ([`Fault::abi`]),
//! as ABI 3 specifies.

use middleware::wasm_abi::AbiError;
use middleware::wasm_abi::periph::{I2C_FREQ_HZ, MAX_I2C};

/// Depth of the controller's transmit and receive FIFOs.
pub const FIFO_DEPTH: u8 = 16;

/// Slack added to every transfer's time bound, in µs, for targets that
/// stretch the clock.
pub const MARGIN_US: u32 = 2_000;

/// The controller's SCL timing registers for one clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// `IC_FS_SCL_HCNT`: SCL high cycles.
    pub hcnt: u16,
    /// `IC_FS_SCL_LCNT`: SCL low cycles.
    pub lcnt: u16,
    /// `IC_FS_SPKLEN`: longest spike suppressed, in cycles (at least 1).
    pub spklen: u8,
    /// `IC_SDA_HOLD.IC_SDA_TX_HOLD`: SDA hold after SCL falls, in cycles.
    pub sda_hold: u16,
}

impl Timing {
    /// The timing for `freq_hz` on a `clk_peri_hz` peripheral clock:
    /// `InvalidArgument` outside the ABI's range, `Unsupported` when the
    /// clock cannot produce it (counts outside 8..=65535, or a hold time
    /// that does not fit the low period).
    pub fn new(clk_peri_hz: u32, freq_hz: u32) -> Result<Self, AbiError> {
        let in_range = i32::try_from(freq_hz).is_ok_and(|f| I2C_FREQ_HZ.contains(&f));
        if !in_range || clk_peri_hz == 0 {
            return Err(AbiError::InvalidArgument);
        }
        let clk = u64::from(clk_peri_hz);
        let freq = u64::from(freq_hz);
        let period = (clk + freq / 2) / freq;
        let lcnt = period * 3 / 5;
        let hcnt = period - lcnt;
        if !(8..=0xFFFF).contains(&lcnt) || !(8..=0xFFFF).contains(&hcnt) {
            return Err(AbiError::Unsupported);
        }
        let sda_hold = if freq_hz < 1_000_000 {
            clk * 3 / 10_000_000 + 1
        } else {
            if clk_peri_hz <= 32_000_000 {
                return Err(AbiError::Unsupported);
            }
            clk * 3 / 25_000_000 + 1
        };
        if sda_hold + 2 > lcnt {
            return Err(AbiError::Unsupported);
        }
        let spklen = if lcnt < 16 { 1 } else { (lcnt / 16).min(255) };
        let narrow = |v: u64| u16::try_from(v).map_err(|_| AbiError::Unsupported);
        Ok(Timing {
            hcnt: narrow(hcnt)?,
            lcnt: narrow(lcnt)?,
            spklen: u8::try_from(spklen).map_err(|_| AbiError::Unsupported)?,
            sda_hold: narrow(sda_hold)?,
        })
    }
}

/// One `IC_DATA_CMD` write: a byte to send or a read, with its framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cmd {
    /// A read (`CMD` = 1); otherwise `byte` is written.
    pub read: bool,
    /// The byte to write (0 for reads).
    pub byte: u8,
    /// Issue a repeated start before this byte.
    pub restart: bool,
    /// Issue a stop after this byte.
    pub stop: bool,
}

/// Whether a transfer of these lengths can run: each at most
/// [`MAX_I2C`] (`InvalidArgument`), not both zero (`Unsupported`, no
/// address-only probes).
pub const fn check_lengths(tx_len: usize, rx_len: usize) -> Result<(), AbiError> {
    if tx_len > MAX_I2C || rx_len > MAX_I2C {
        return Err(AbiError::InvalidArgument);
    }
    if tx_len == 0 && rx_len == 0 {
        return Err(AbiError::Unsupported);
    }
    Ok(())
}

/// The commands of one transfer: the `tx` bytes, then `rx_len` reads, the
/// first read after a repeated start when something was written, and a
/// stop after the last command. `tx.len() + rx_len` commands in all.
pub fn commands(tx: &[u8], rx_len: usize) -> impl Iterator<Item = Cmd> + '_ {
    let total = tx.len() + rx_len;
    let writes = tx.iter().map(|&byte| (false, byte));
    let reads = core::iter::repeat_n((true, 0u8), rx_len);
    writes.chain(reads).enumerate().map(move |(i, (read, byte))| Cmd {
        read,
        byte,
        restart: read && i == tx.len() && !tx.is_empty(),
        stop: i + 1 == total,
    })
}

/// The time bound of a transfer, in µs: twice its bus time at `freq_hz`
/// (9 clocks per byte and per address phase, plus start and stop) plus
/// [`MARGIN_US`].
pub fn timeout_us(freq_hz: u32, tx_len: usize, rx_len: usize) -> u32 {
    let phases = usize::from(tx_len > 0) + usize::from(rx_len > 0);
    let clocks = 9 * (tx_len + rx_len + phases) + 2;
    let freq = u64::from(freq_hz.max(1));
    let bus_us = (clocks as u64 * 1_000_000).div_ceil(freq);
    u32::try_from(2 * bus_us + u64::from(MARGIN_US)).unwrap_or(u32::MAX)
}

/// Why a transfer failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The target did not acknowledge its address.
    AddressNack,
    /// The target did not acknowledge a written byte.
    DataNack,
    /// Another controller won arbitration (or the bus misbehaved).
    ArbitrationLost,
    /// Any other abort the controller reported.
    Abort,
    /// The transfer exceeded [`timeout_us`] (a target holding SCL low, or
    /// a stuck bus).
    Timeout,
}

impl Fault {
    /// What the payload sees: always `Transport` (ABI 3).
    pub const fn abi(self) -> AbiError {
        AbiError::Transport
    }

    /// Classifies a non-zero `IC_TX_ABRT_SOURCE`; `None` when it is 0.
    pub const fn from_abort_source(source: u32) -> Option<Self> {
        const ADDR_NOACK: u32 = 0b111; // 7-bit, 10-bit first and second byte
        const TXDATA_NOACK: u32 = 1 << 3;
        const ARB_LOST: u32 = 1 << 12;
        if source == 0 {
            None
        } else if source & ADDR_NOACK != 0 {
            Some(Fault::AddressNack)
        } else if source & TXDATA_NOACK != 0 {
            Some(Fault::DataNack)
        } else if source & ARB_LOST != 0 {
            Some(Fault::ArbitrationLost)
        } else {
            Some(Fault::Abort)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLOCKS: [u32; 2] = [125_000_000, 150_000_000];

    #[test]
    fn v16_every_abi_clock_has_a_timing_on_both_chips() {
        for clk in CLOCKS {
            for freq in [10_000, 50_000, 100_000, 400_000, 1_000_000] {
                let t = Timing::new(clk, freq).unwrap_or_else(|e| panic!("{clk} {freq}: {e:?}"));
                let period = u32::from(t.hcnt) + u32::from(t.lcnt);
                let actual = clk / period;
                // Within 1 % of the request.
                assert!(actual.abs_diff(freq) * 100 <= freq, "{clk} {freq}: {actual}");
                assert!(t.lcnt > t.hcnt, "low longer than high");
                assert!(t.spklen >= 1 && u32::from(t.sda_hold) + 2 <= u32::from(t.lcnt));
            }
        }
        // 100 kHz at 125 MHz: the Pico SDK's numbers.
        assert_eq!(
            Timing::new(125_000_000, 100_000),
            Ok(Timing { hcnt: 500, lcnt: 750, spklen: 46, sda_hold: 38 })
        );
        // 10 kHz at 150 MHz: the spike filter register saturates.
        assert_eq!(Timing::new(150_000_000, 10_000).map(|t| t.spklen), Ok(255));
        // Fast-mode Plus uses the shorter hold.
        assert_eq!(Timing::new(150_000_000, 1_000_000).map(|t| t.sda_hold), Ok(19));
    }

    #[test]
    fn v16_clocks_outside_the_abi_or_the_divider_are_refused() {
        assert_eq!(Timing::new(125_000_000, 9_999), Err(AbiError::InvalidArgument));
        assert_eq!(Timing::new(125_000_000, 1_000_001), Err(AbiError::InvalidArgument));
        assert_eq!(Timing::new(0, 100_000), Err(AbiError::InvalidArgument));
        // A slow peripheral clock cannot make Fast-mode Plus or a long low count.
        assert_eq!(Timing::new(12_000_000, 1_000_000), Err(AbiError::Unsupported));
        assert_eq!(Timing::new(4_000_000, 400_000), Err(AbiError::Unsupported));
        // A fast one overflows the 16-bit counts at 10 kHz.
        assert_eq!(Timing::new(2_000_000_000, 10_000), Err(AbiError::Unsupported));
    }

    #[test]
    fn v16_transfers_are_writes_then_reads_after_a_repeated_start() {
        let w = |byte, restart, stop| Cmd { read: false, byte, restart, stop };
        let r = |restart, stop| Cmd { read: true, byte: 0, restart, stop };
        let write_read: heapless::Vec<Cmd, 8> = commands(&[0x0A, 0x0B], 2).collect();
        assert_eq!(
            write_read.as_slice(),
            &[w(0x0A, false, false), w(0x0B, false, false), r(true, false), r(false, true)]
        );
        let write: heapless::Vec<Cmd, 4> = commands(&[1, 2, 3], 0).collect();
        assert_eq!(write.as_slice(), &[w(1, false, false), w(2, false, false), w(3, false, true)]);
        let read: heapless::Vec<Cmd, 4> = commands(&[], 3).collect();
        assert_eq!(read.as_slice(), &[r(false, false), r(false, false), r(false, true)]);
        let one: heapless::Vec<Cmd, 2> = commands(&[], 1).collect();
        assert_eq!(one.as_slice(), &[r(false, true)]);
        assert_eq!(commands(&[0; MAX_I2C], MAX_I2C).count(), 2 * MAX_I2C);
        assert_eq!(commands(&[0; MAX_I2C], MAX_I2C).filter(|c| c.stop).count(), 1);
        assert_eq!(check_lengths(0, 0), Err(AbiError::Unsupported), "no address-only probe");
        assert_eq!(check_lengths(1, 0), Ok(()));
        assert_eq!(check_lengths(0, MAX_I2C), Ok(()));
        assert_eq!(check_lengths(MAX_I2C + 1, 0), Err(AbiError::InvalidArgument));
    }

    #[test]
    fn v16_every_transfer_is_bounded_in_time() {
        // One register read at 100 kHz: (1 + 1 + 2 phases) * 9 + 2 = 38
        // clocks, 380 µs, bound 2.76 ms.
        assert_eq!(timeout_us(100_000, 1, 1), 2 * 380 + MARGIN_US);
        // The worst case the ABI admits stays under a quarter second.
        let worst = timeout_us(10_000, MAX_I2C, MAX_I2C);
        assert!(worst < 250_000, "{worst}");
        assert!(timeout_us(1_000_000, 1, 0) >= MARGIN_US);
        assert!(timeout_us(0, 1, 1) > 0, "a zero clock is clamped, not a division by zero");
        // More bytes or a slower clock never shorten the bound.
        assert!(timeout_us(100_000, 2, 0) > timeout_us(100_000, 1, 0));
        assert!(timeout_us(50_000, 1, 0) > timeout_us(100_000, 1, 0));
    }

    #[test]
    fn v16_every_fault_is_transport() {
        assert_eq!(Fault::from_abort_source(0), None);
        assert_eq!(Fault::from_abort_source(1), Some(Fault::AddressNack));
        assert_eq!(Fault::from_abort_source(1 << 3), Some(Fault::DataNack));
        assert_eq!(Fault::from_abort_source(1 << 12), Some(Fault::ArbitrationLost));
        assert_eq!(Fault::from_abort_source(1 << 16), Some(Fault::Abort));
        for f in [Fault::AddressNack, Fault::DataNack, Fault::ArbitrationLost, Fault::Abort, Fault::Timeout] {
            assert_eq!(f.abi(), AbiError::Transport);
        }
    }
}
