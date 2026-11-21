//! The W5500 on SPI0 (`w5500`, D49, D50): its pins, its clock and the one
//! register write the shell changes on the way to the chip.
//!
//! # Pins (W5500-EVB-Pico)
//!
//! | signal | GPIO | function |
//! |--------|------|----------|
//! | MISO   | 16   | SPI0 RX  |
//! | CS     | 17   | output (SPI0 CSn's pin, driven as a GPIO) |
//! | SCK    | 18   | SPI0 SCK |
//! | MOSI   | 19   | SPI0 TX  |
//! | RST    | 20   | output, active low |
//! | INTn   | 21   | input, pull-up, active low |
//!
//! [`SPI_HZ`] is asked for; embassy-rp's divider gives at most that (about
//! 15.6 MHz at the RP2040's 125 MHz peripheral clock, 18.75 MHz at the
//! RP2350's 150 MHz). Mode 0.
//!
//! # The MAC filter
//!
//! `embassy-net-wiznet` 0.3.0 puts socket 0 in MACRAW mode with `Sn_MR` =
//! `0x84`: MACRAW (`0x04`) with MFEN (bit 7), the MAC filter, on. With the
//! filter on the chip takes only frames to its own address and broadcast,
//! so multicast (`01:00:5e:00:00:fb`, mDNS) never arrives. The value is a
//! sealed constant of the driver's `Chip` trait, so [`NoMacFilter`] wraps
//! the SPI device and clears MFEN in that one write (`Sn_MR` of socket 0,
//! one data byte); every other transaction passes through untouched. With
//! the filter off the chip receives every frame on the segment into its
//! 16 KiB buffer and smoltcp drops what is not for the board.

use embedded_hal_async::spi::{ErrorType, Operation, SpiDevice};

/// The W5500's pins on the W5500-EVB-Pico: MISO.
pub const MISO: u8 = 16;
/// Chip select.
pub const CS: u8 = 17;
/// Clock.
pub const SCK: u8 = 18;
/// MOSI.
pub const MOSI: u8 = 19;
/// Reset (active low).
pub const RST: u8 = 20;
/// Interrupt (active low).
pub const INT: u8 = 21;
/// All six, ascending.
pub const PINS: [u8; 6] = [MISO, CS, SCK, MOSI, RST, INT];

/// The SPI clock asked for.
pub const SPI_HZ: u32 = 20_000_000;

/// `Sn_MR`'s MFEN bit (MAC filter enable, MACRAW mode).
pub const MFEN: u8 = 1 << 7;

/// The address phase of socket 0's `Sn_MR` (offset 0).
const SN_MR_ADDRESS: [u8; 2] = [0x00, 0x00];
/// The control phase of a write to socket 0's register block (block 1,
/// write bit 2).
const SOCKET0_WRITE: [u8; 1] = [(0x01 << 3) | 0b100];

/// The value a transaction writes to socket 0's `Sn_MR`, if that is what it
/// is: the driver's three-phase write (address, control, one data byte).
pub fn socket_mode_write(operations: &[Operation<'_, u8>]) -> Option<u8> {
    match operations {
        [
            Operation::Write(addr),
            Operation::Write(ctrl),
            Operation::Write([value]),
        ] if *addr == SN_MR_ADDRESS && *ctrl == SOCKET0_WRITE => Some(*value),
        _ => None,
    }
}

/// An SPI device that clears [`MFEN`] in every write of socket 0's
/// `Sn_MR` (module docs).
#[derive(Debug)]
pub struct NoMacFilter<S>(S);

impl<S> NoMacFilter<S> {
    /// Wraps `spi`.
    pub const fn new(spi: S) -> Self {
        NoMacFilter(spi)
    }

    /// The wrapped device.
    pub fn into_inner(self) -> S {
        self.0
    }
}

impl<S: ErrorType> ErrorType for NoMacFilter<S> {
    type Error = S::Error;
}

impl<S: SpiDevice> SpiDevice for NoMacFilter<S> {
    async fn transaction(&mut self, operations: &mut [Operation<'_, u8>]) -> Result<(), S::Error> {
        match socket_mode_write(operations) {
            Some(mode) if mode & MFEN != 0 => {
                let value = [mode & !MFEN];
                let mut patched = [
                    Operation::Write(&SN_MR_ADDRESS),
                    Operation::Write(&SOCKET0_WRITE),
                    Operation::Write(&value),
                ];
                self.0.transaction(&mut patched).await
            }
            _ => self.0.transaction(operations).await,
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use core::cell::RefCell;
    use core::convert::Infallible;
    use embassy_futures::block_on;
    use std::vec::Vec;

    /// One recorded operation: written bytes, or the length read.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Op {
        Write(Vec<u8>),
        Read(usize),
        Transfer(Vec<u8>),
        DelayNs(u32),
    }

    /// Records every transaction; reads return `0xA5`.
    #[derive(Debug, Default)]
    pub(crate) struct Recorder {
        pub transactions: Vec<Vec<Op>>,
    }

    impl ErrorType for Recorder {
        type Error = Infallible;
    }

    impl SpiDevice for Recorder {
        async fn transaction(&mut self, ops: &mut [Operation<'_, u8>]) -> Result<(), Infallible> {
            let mut t = Vec::new();
            for op in ops {
                t.push(match op {
                    Operation::Write(w) => Op::Write(w.to_vec()),
                    Operation::Read(r) => {
                        r.fill(0xA5);
                        Op::Read(r.len())
                    }
                    Operation::Transfer(r, w) => {
                        r.fill(0xA5);
                        Op::Transfer(w.to_vec())
                    }
                    Operation::TransferInPlace(b) => {
                        let sent = b.to_vec();
                        b.fill(0xA5);
                        Op::Transfer(sent)
                    }
                    Operation::DelayNs(d) => Op::DelayNs(*d),
                });
            }
            self.transactions.push(t);
            Ok(())
        }
    }

    fn write3(addr: [u8; 2], ctrl: u8, data: &[u8]) -> Vec<Op> {
        std::vec![
            Op::Write(addr.to_vec()),
            Op::Write(std::vec![ctrl]),
            Op::Write(data.to_vec())
        ]
    }

    /// Runs one three-phase write through the wrapper.
    fn through(addr: [u8; 2], ctrl: u8, data: &[u8]) -> Vec<Op> {
        let mut dev = NoMacFilter::new(Recorder::default());
        let (a, c) = (addr, [ctrl]);
        let mut ops = [
            Operation::Write(&a),
            Operation::Write(&c),
            Operation::Write(data),
        ];
        block_on(dev.transaction(&mut ops)).unwrap();
        dev.into_inner().transactions.remove(0)
    }

    #[test]
    fn d49_the_socket_mode_write_loses_the_mac_filter() {
        assert_eq!(
            through([0, 0], 0x0C, &[0x84]),
            write3([0, 0], 0x0C, &[0x04])
        );
        // Any value: only MFEN is cleared.
        assert_eq!(
            through([0, 0], 0x0C, &[0xE4]),
            write3([0, 0], 0x0C, &[0x64])
        );
        assert_eq!(
            through([0, 0], 0x0C, &[0x04]),
            write3([0, 0], 0x0C, &[0x04])
        );
        let ops = [
            Operation::Write(&SN_MR_ADDRESS),
            Operation::Write(&SOCKET0_WRITE),
            Operation::Write(&[0x84]),
        ];
        assert_eq!(socket_mode_write(&ops), Some(0x84));
    }

    #[test]
    fn d49_every_other_transaction_passes_untouched() {
        // The common mode register's reset (block 0): 0x80 stays.
        assert_eq!(
            through([0, 0], 0x04, &[0x80]),
            write3([0, 0], 0x04, &[0x80])
        );
        // Another socket 0 register (Sn_CR at 1), another block, more bytes.
        assert_eq!(
            through([0, 1], 0x0C, &[0x84]),
            write3([0, 1], 0x0C, &[0x84])
        );
        assert_eq!(
            through([0, 0], 0x14, &[0x84]),
            write3([0, 0], 0x14, &[0x84])
        );
        assert_eq!(
            through([0, 0], 0x0C, &[0x84, 0x84]),
            write3([0, 0], 0x0C, &[0x84, 0x84])
        );
        // A read of Sn_MR (control without the write bit) reads.
        let mut dev = NoMacFilter::new(Recorder::default());
        let mut data = [0u8; 1];
        let mut ops = [
            Operation::Write(&[0, 0]),
            Operation::Write(&[0x08]),
            Operation::TransferInPlace(&mut data),
        ];
        block_on(dev.transaction(&mut ops)).unwrap();
        assert_eq!(data, [0xA5]);
        assert_eq!(
            dev.into_inner().transactions,
            [std::vec![
                Op::Write(std::vec![0, 0]),
                Op::Write(std::vec![0x08]),
                Op::Transfer(std::vec![0])
            ]]
        );
        // Two-phase or four-phase shapes are not the driver's write.
        let two = [Operation::Write(&[0, 0, 0x0C]), Operation::Write(&[0x84])];
        assert_eq!(socket_mode_write(&two), None);
    }

    /// A pin the driver resets the chip with, and its interrupt.
    struct Pin;

    impl embedded_hal::digital::ErrorType for Pin {
        type Error = Infallible;
    }

    impl embedded_hal::digital::OutputPin for Pin {
        fn set_low(&mut self) -> Result<(), Infallible> {
            Ok(())
        }
        fn set_high(&mut self) -> Result<(), Infallible> {
            Ok(())
        }
    }

    impl embedded_hal_async::digital::Wait for Pin {
        async fn wait_for_high(&mut self) -> Result<(), Infallible> {
            Ok(())
        }
        async fn wait_for_low(&mut self) -> Result<(), Infallible> {
            Ok(())
        }
        async fn wait_for_rising_edge(&mut self) -> Result<(), Infallible> {
            Ok(())
        }
        async fn wait_for_falling_edge(&mut self) -> Result<(), Infallible> {
            Ok(())
        }
        async fn wait_for_any_edge(&mut self) -> Result<(), Infallible> {
            Ok(())
        }
    }

    /// Answers every read with the W5500's version (0x04), so the
    /// driver's chip check passes, and records like [`Recorder`] into a
    /// log the test keeps (the driver keeps the device).
    struct W5500Bus<'a>(&'a RefCell<Recorder>);

    impl ErrorType for W5500Bus<'_> {
        type Error = Infallible;
    }

    impl SpiDevice for W5500Bus<'_> {
        async fn transaction(&mut self, ops: &mut [Operation<'_, u8>]) -> Result<(), Infallible> {
            block_on(self.0.borrow_mut().transaction(ops))?;
            for op in ops {
                if let Operation::TransferInPlace(b) | Operation::Read(b) = op {
                    b.fill(0x04);
                }
            }
            Ok(())
        }
    }

    /// Every transaction of the real driver's initialisation, through
    /// [`NoMacFilter`] when `wrapped`.
    fn init(wrapped: bool) -> Vec<Vec<Op>> {
        use embassy_net_wiznet::chip::W5500;
        let log = RefCell::new(Recorder::default());
        let mut state = embassy_net_wiznet::State::<2, 2>::new();
        let mac = [0x02, 0x4D, 1, 2, 3, 4];
        let ok = if wrapped {
            let spi = NoMacFilter::new(W5500Bus(&log));
            block_on(embassy_net_wiznet::new::<2, 2, W5500, _, _, _>(
                mac, &mut state, spi, Pin, Pin,
            ))
            .is_ok()
        } else {
            let spi = W5500Bus(&log);
            block_on(embassy_net_wiznet::new::<2, 2, W5500, _, _, _>(
                mac, &mut state, spi, Pin, Pin,
            ))
            .is_ok()
        };
        assert!(ok, "the driver's initialisation failed");
        log.into_inner().transactions
    }

    /// Sn_MR values in `transactions` (the driver's write shape).
    fn socket_modes(transactions: &[Vec<Op>]) -> Vec<u8> {
        transactions
            .iter()
            .filter_map(|t| match t.as_slice() {
                [Op::Write(a), Op::Write(c), Op::Write(v)]
                    if a[..] == SN_MR_ADDRESS && c[..] == SOCKET0_WRITE =>
                {
                    v.first().copied().filter(|_| v.len() == 1)
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn d49_contract_the_real_driver_writes_sn_mr_once_and_the_wrapper_clears_mfen() {
        // Upstream as it is: MACRAW with the MAC filter, written once. If
        // this fails, embassy-net-wiznet changed (perhaps the filter is
        // configurable now) and NoMacFilter may be dropped.
        let bare = init(false);
        assert_eq!(socket_modes(&bare), [0x84], "{bare:?}");
        // Behind the wrapper the chip gets MACRAW with every filter off.
        let wrapped = init(true);
        assert_eq!(socket_modes(&wrapped), [0x04], "{wrapped:?}");
        // Nothing else differs.
        assert_eq!(bare.len(), wrapped.len());
        let other = |ts: &[Vec<Op>]| {
            ts.iter()
                .filter(|t| socket_modes(core::slice::from_ref(t)).is_empty())
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(other(&bare), other(&wrapped));
    }

    #[test]
    fn d49_the_pins_are_the_evb_picos() {
        assert_eq!(PINS, [16, 17, 18, 19, 20, 21]);
        assert_eq!(SPI_HZ, 20_000_000);
    }
}
