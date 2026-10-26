//! I2C controller through the `i2c_open` and `i2c_transfer` imports
//! (ABI 3).
//!
//! Unlike SPI and UART, I2C transfers are synchronous: one
//! `i2c_transfer` writes up to [`MAX_I2C`] bytes, issues a repeated start
//! and reads up to [`MAX_I2C`] bytes before returning. That bound keeps the
//! host call short, so [`I2c`] can implement the blocking
//! `embedded_hal::i2c::I2c` trait (7-bit addresses only). The firmware
//! maps each bus to fixed pins (board documentation).
//!
//! How the trait maps onto the one import:
//!
//! * `write`, `read` and `write_read` are one `i2c_transfer` each.
//! * `transaction` accepts any sequence of writes followed by reads (either
//!   group may be empty). Adjacent operations of the same kind are not
//!   separated by a restart in the `embedded-hal` contract, so the writes
//!   are concatenated and the reads filled from one received block. Each
//!   group may total at most [`MAX_I2C`] bytes.
//! * A read followed by a write needs a second restart the ABI cannot
//!   express, and fails with [`AbiError::Unsupported`] before the host is
//!   called; split it into two transactions.
//!
//! A target that does not acknowledge, or a bus error, is
//! [`AbiError::Transport`], reported to `embedded-hal` users as
//! `ErrorKind::NoAcknowledge(NoAcknowledgeSource::Unknown)`.

use embedded_hal::i2c::{ErrorKind, NoAcknowledgeSource, Operation, SevenBitAddress};

use crate::gpio::HalError;
use crate::{AbiError, check, sys};
pub use middleware::wasm_abi::periph::{I2C_BUSES, I2C_FREQ_HZ, I2cConfig, I2cTransfer, MAX_I2C};

/// Validates `i2c_open` arguments with the ABI parser.
fn config(bus: u8, freq_hz: u32) -> Result<I2cConfig, AbiError> {
    let freq = i32::try_from(freq_hz).map_err(|_| AbiError::InvalidArgument)?;
    I2cConfig::parse(i32::from(bus), freq)
}

/// Total write and read bytes of a transaction of the shape "writes, then
/// reads", each at most [`MAX_I2C`].
fn plan(ops: &[Operation<'_>]) -> Result<(usize, usize), AbiError> {
    let (mut tx, mut rx, mut reading) = (0usize, 0usize, false);
    for op in ops {
        match op {
            Operation::Write(_) if reading => return Err(AbiError::Unsupported),
            Operation::Write(b) => tx += b.len(),
            Operation::Read(b) => {
                reading = true;
                rx += b.len();
            }
        }
        if tx > MAX_I2C || rx > MAX_I2C {
            return Err(AbiError::InvalidArgument);
        }
    }
    Ok((tx, rx))
}

/// An open I2C bus.
#[derive(Debug, PartialEq, Eq)]
pub struct I2c {
    config: I2cConfig,
}

impl I2c {
    /// Opens bus `bus` (`< I2C_BUSES`) at `freq_hz` (within
    /// [`I2C_FREQ_HZ`]; the board may refuse clocks it cannot generate).
    pub fn open(bus: u8, freq_hz: u32) -> Result<Self, AbiError> {
        let config = config(bus, freq_hz)?;
        check(sys::i2c_open(i32::from(config.bus), config.freq_hz as i32))?;
        Ok(I2c { config })
    }

    /// The validated configuration.
    pub fn config(&self) -> &I2cConfig {
        &self.config
    }

    /// Writes `tx`, then (repeated start) reads into `rx`, addressed to the
    /// 7-bit `addr`. Either buffer may be empty; each holds at most
    /// [`MAX_I2C`] bytes.
    pub fn transfer(&mut self, addr: u8, tx: &[u8], rx: &mut [u8]) -> Result<(), AbiError> {
        let len = |n: usize| i32::try_from(n).map_err(|_| AbiError::InvalidArgument);
        I2cTransfer::parse(
            i32::from(self.config.bus),
            i32::from(addr),
            len(tx.len())?,
            len(rx.len())?,
        )?;
        check(sys::i2c_transfer(
            i32::from(self.config.bus),
            i32::from(addr),
            tx,
            rx,
        ))
        .map(|_| ())
    }
}

impl embedded_hal::i2c::Error for HalError {
    fn kind(&self) -> ErrorKind {
        match self.0 {
            AbiError::Transport => ErrorKind::NoAcknowledge(NoAcknowledgeSource::Unknown),
            _ => ErrorKind::Other,
        }
    }
}

impl embedded_hal::i2c::ErrorType for I2c {
    type Error = HalError;
}

impl embedded_hal::i2c::I2c<SevenBitAddress> for I2c {
    fn read(&mut self, address: u8, read: &mut [u8]) -> Result<(), HalError> {
        self.transfer(address, &[], read).map_err(HalError)
    }

    fn write(&mut self, address: u8, write: &[u8]) -> Result<(), HalError> {
        self.transfer(address, write, &mut []).map_err(HalError)
    }

    fn write_read(&mut self, address: u8, write: &[u8], read: &mut [u8]) -> Result<(), HalError> {
        self.transfer(address, write, read).map_err(HalError)
    }

    /// See the module docs: writes, then reads, each group at most
    /// [`MAX_I2C`] bytes; an empty list does nothing.
    fn transaction(
        &mut self,
        address: u8,
        operations: &mut [Operation<'_>],
    ) -> Result<(), HalError> {
        if operations.is_empty() {
            return Ok(());
        }
        let (tx_len, rx_len) = plan(operations).map_err(HalError)?;
        // `plan` bounded both totals by MAX_I2C, so the `get`s below always
        // succeed; they report `InvalidArgument` rather than panic.
        let short = HalError(AbiError::InvalidArgument);
        let (mut tx, mut rx) = ([0u8; MAX_I2C], [0u8; MAX_I2C]);
        let mut at = 0;
        for op in operations.iter() {
            if let Operation::Write(b) = op {
                let end = at + b.len();
                tx.get_mut(at..end).ok_or(short)?.copy_from_slice(b);
                at = end;
            }
        }
        let tx = tx.get(..tx_len).ok_or(short)?;
        let rx = rx.get_mut(..rx_len).ok_or(short)?;
        self.transfer(address, tx, rx).map_err(HalError)?;
        let mut at = 0;
        for op in operations.iter_mut() {
            if let Operation::Read(b) = op {
                let end = at + b.len();
                b.copy_from_slice(rx.get(at..end).ok_or(short)?);
                at = end;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_hal::i2c::{Error as _, I2c as _};

    fn bus() -> I2c {
        I2c {
            config: config(0, 100_000).expect("valid"),
        }
    }

    #[test]
    fn open_validates_before_the_host() {
        let bad = Err(AbiError::InvalidArgument);
        assert_eq!(I2c::open(I2C_BUSES as u8, 100_000), bad);
        assert_eq!(I2c::open(0, 9_999), bad);
        assert_eq!(I2c::open(0, u32::MAX), bad);
        assert_eq!(I2c::open(1, 400_000), Err(AbiError::Unsupported));
        assert_eq!(
            config(1, 400_000),
            Ok(I2cConfig {
                bus: 1,
                freq_hz: 400_000
            })
        );
    }

    #[test]
    fn transfer_checks_address_and_lengths_before_the_host() {
        let mut i2c = bus();
        let bad = Err(AbiError::InvalidArgument);
        assert_eq!(i2c.transfer(0x80, &[1], &mut []), bad);
        assert_eq!(i2c.transfer(0x50, &[0; MAX_I2C + 1], &mut []), bad);
        assert_eq!(i2c.transfer(0x50, &[], &mut [0; MAX_I2C + 1]), bad);
        assert_eq!(
            i2c.transfer(0x50, &[1], &mut [0; 2]),
            Err(AbiError::Unsupported)
        );
        assert_eq!(
            i2c.write_read(0x50, &[1], &mut [0; 2]),
            Err(HalError(AbiError::Unsupported))
        );
    }

    #[test]
    fn transaction_shapes() {
        let (mut a, mut b) = ([0u8; 2], [0u8; 3]);
        assert_eq!(
            plan(&[
                Operation::Write(&[1, 2]),
                Operation::Write(&[3]),
                Operation::Read(&mut a),
                Operation::Read(&mut b),
            ]),
            Ok((3, 5))
        );
        assert_eq!(plan(&[]), Ok((0, 0)));
        assert_eq!(
            plan(&[Operation::Read(&mut a), Operation::Write(&[1])]),
            Err(AbiError::Unsupported),
            "read then write needs a second restart"
        );
        let big = [0u8; MAX_I2C];
        assert_eq!(
            plan(&[Operation::Write(&big), Operation::Write(&[1])]),
            Err(AbiError::InvalidArgument)
        );
        let mut i2c = bus();
        assert_eq!(i2c.transaction(0x50, &mut []), Ok(()));
        assert_eq!(
            i2c.transaction(0x50, &mut [Operation::Read(&mut a), Operation::Write(&[1])]),
            Err(HalError(AbiError::Unsupported))
        );
        assert_eq!(
            i2c.transaction(0x50, &mut [Operation::Write(&[1]), Operation::Read(&mut a)]),
            Err(HalError(AbiError::Unsupported)),
            "valid shape reaches the host stub"
        );
    }

    #[test]
    fn nack_maps_to_no_acknowledge() {
        assert_eq!(
            HalError(AbiError::Transport).kind(),
            ErrorKind::NoAcknowledge(NoAcknowledgeSource::Unknown)
        );
        assert_eq!(HalError(AbiError::Busy).kind(), ErrorKind::Other);
    }
}
