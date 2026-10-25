//! Secondary UART through the `uart_open`, `uart_write` and `uart_read`
//! imports.
//!
//! UART0 carries the device protocol, so a payload may open only port
//! [`UART_PORT`] (UART1).
//!
//! * Transmit is asynchronous: [`Uart::write`] hands the bytes to the host,
//!   which copies them into a firmware-owned DMA buffer and returns at once.
//!   Completion arrives as an [`Event::DmaDone`] on
//!   [`DmaChannel::Uart1Tx`]; pass it to [`Uart::on_event`]. A second write
//!   before then fails with [`AbiError::Busy`], so a payload that produces
//!   data faster keeps its own pending buffer.
//! * Receive is buffered by the firmware (up to [`UART_RX_BUFFER`] bytes)
//!   and announced with coalesced [`Event::UartRx`] events; [`Uart::read`]
//!   copies whatever is buffered, possibly 0 bytes.
//!
//! [`Uart::open`] validates its arguments with the ABI's own parser
//! ([`UartConfig::parse`]) before calling the host.

use crate::dma::Transfer;
use crate::irq::{DmaChannel, Event};
use crate::{AbiError, check, sys};
pub use middleware::wasm_abi::periph::{
    MAX_DMA, UART_BAUD, UART_PORT, UART_RX_BUFFER, UartConfig, uart_write_len,
};

/// An optional pin as the ABI encodes it (`-1` = unused).
fn pin_arg(pin: Option<u8>) -> i32 {
    pin.map_or(-1, i32::from)
}

/// Validates `uart_open` arguments for [`UART_PORT`] with the ABI parser.
fn config(tx: Option<u8>, rx: Option<u8>, baud: u32) -> Result<UartConfig, AbiError> {
    let baud = i32::try_from(baud).map_err(|_| AbiError::InvalidArgument)?;
    UartConfig::parse(UART_PORT, pin_arg(tx), pin_arg(rx), baud)
}

/// The open secondary UART.
#[derive(Debug, PartialEq, Eq)]
pub struct Uart {
    config: UartConfig,
    tx: Transfer,
}

impl Uart {
    /// Opens UART1 on the given pins (at least one; distinct) at `baud`
    /// (within [`UART_BAUD`]), 8N1.
    pub fn open(tx: Option<u8>, rx: Option<u8>, baud: u32) -> Result<Self, AbiError> {
        let config = config(tx, rx, baud)?;
        check(sys::uart_open(
            i32::from(config.port),
            pin_arg(config.tx),
            pin_arg(config.rx),
            config.baud as i32,
        ))?;
        Ok(Uart {
            config,
            tx: Transfer::Idle,
        })
    }

    /// The validated configuration.
    pub fn config(&self) -> &UartConfig {
        &self.config
    }

    /// Whether a transmit is in flight.
    pub fn is_busy(&self) -> bool {
        self.tx.is_busy()
    }

    /// Starts transmitting `data` (`1..=MAX_DMA` bytes). The host copies the
    /// bytes before returning. Fails with [`AbiError::Busy`] while the
    /// previous write is in flight.
    pub fn write(&mut self, data: &[u8]) -> Result<(), AbiError> {
        let len = i32::try_from(data.len()).map_err(|_| AbiError::InvalidArgument)?;
        uart_write_len(len)?;
        let next = self.tx.start()?;
        check(sys::uart_write(
            i32::from(self.config.port),
            crate::ptr_i32(data),
            len,
        ))?;
        self.tx = next;
        Ok(())
    }

    /// Copies buffered received bytes into `buf`; returns how many (0 if
    /// none are waiting).
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, AbiError> {
        if buf.is_empty() {
            return Ok(0);
        }
        check(sys::uart_read(i32::from(self.config.port), buf)).map(|n| n as usize)
    }

    /// Feeds an event to the UART. Returns the transmit result if `event` is
    /// the `DmaDone` of [`DmaChannel::Uart1Tx`], `None` otherwise.
    pub fn on_event(&mut self, event: &Event) -> Option<Result<usize, AbiError>> {
        match *event {
            Event::DmaDone {
                channel: DmaChannel::Uart1Tx,
                result,
                ..
            } => {
                self.tx = self.tx.finish(result);
                Some(result)
            }
            _ => None,
        }
    }

    /// Forgets an in-flight write whose `DmaDone` was lost (see
    /// [`Transfer`]).
    pub fn recover(&mut self) {
        self.tx = self.tx.recover();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uart() -> Uart {
        Uart {
            config: config(Some(17), Some(18), 115_200).expect("valid"),
            tx: Transfer::Idle,
        }
    }

    #[test]
    fn open_validates_before_the_host() {
        let bad = Err(AbiError::InvalidArgument);
        assert_eq!(Uart::open(None, None, 9600), bad);
        assert_eq!(Uart::open(Some(4), Some(4), 9600), bad);
        assert_eq!(Uart::open(Some(4), None, 299), bad);
        assert_eq!(Uart::open(Some(4), None, u32::MAX), bad);
        assert_eq!(
            Uart::open(Some(17), Some(18), 115_200),
            Err(AbiError::Unsupported)
        );
        assert_eq!(
            config(None, Some(18), 9600),
            Ok(UartConfig {
                port: 1,
                tx: None,
                rx: Some(18),
                baud: 9600
            })
        );
    }

    #[test]
    fn write_checks_length_and_busy() {
        let mut u = uart();
        assert_eq!(u.write(&[]), Err(AbiError::InvalidArgument));
        assert_eq!(u.write(&[0; MAX_DMA + 1]), Err(AbiError::InvalidArgument));
        assert_eq!(u.write(b"hi"), Err(AbiError::Unsupported));
        assert!(!u.is_busy());
        u.tx = Transfer::InFlight;
        assert_eq!(u.write(b"hi"), Err(AbiError::Busy));
    }

    #[test]
    fn tx_done_clears_busy() {
        let mut u = uart();
        u.tx = Transfer::InFlight;
        let spi = Event::DmaDone {
            channel: DmaChannel::Spi0,
            result: Ok(1),
            time_us: 0,
        };
        assert_eq!(u.on_event(&spi), None);
        let done = Event::DmaDone {
            channel: DmaChannel::Uart1Tx,
            result: Ok(2),
            time_us: 0,
        };
        assert_eq!(u.on_event(&done), Some(Ok(2)));
        assert!(!u.is_busy());
        u.tx = Transfer::InFlight;
        u.recover();
        assert!(!u.is_busy());
    }

    #[test]
    fn read_uses_the_host() {
        let mut u = uart();
        assert_eq!(u.read(&mut []), Ok(0));
        assert_eq!(u.read(&mut [0; 8]), Err(AbiError::Unsupported));
    }
}
