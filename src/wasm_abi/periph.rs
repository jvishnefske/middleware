//! Argument validation for the ABI 2 peripheral imports.
//!
//! Each parser turns raw `i32` import arguments into a typed value or an
//! [`AbiError::InvalidArgument`], so the runtime (`wasm-host`), firmware and
//! simulators share one definition of what is valid. Chip-specific checks
//! (which pins exist or are reserved) stay in the firmware.

use super::AbiError;

/// Hardware timers available to a payload (ids `0..MAX_TIMERS`).
pub const MAX_TIMERS: usize = 4;

/// Shortest timer period.
pub const MIN_TIMER_US: u32 = 100;

/// Largest single SPI or UART DMA transfer, in bytes (one ESP DMA
/// descriptor holds at most 4092 bytes).
pub const MAX_DMA: usize = 4092;

/// SPI buses available to a payload (bus 0 is SPI2 on every supported chip).
pub const SPI_BUSES: i32 = 1;

/// Highest SPI clock, in kHz.
pub const MAX_SPI_KHZ: i32 = 40_000;

/// The one secondary UART a payload may open (UART0 carries the protocol).
pub const UART_PORT: i32 = 1;

/// Accepted UART baud range.
pub const UART_BAUD: core::ops::RangeInclusive<i32> = 300..=3_000_000;

/// Receive buffer per secondary UART; bytes beyond it are dropped and the
/// next `UartRx` event still reports the buffered count.
pub const UART_RX_BUFFER: usize = 1024;

/// Which edges raise a GPIO interrupt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// Interrupt disabled.
    Off = 0,
    /// Low → high.
    Rising = 1,
    /// High → low.
    Falling = 2,
    /// Either edge.
    Any = 3,
}

impl Edge {
    /// Parses the `edge` argument of `gpio_irq`.
    pub const fn from_i32(v: i32) -> Result<Self, AbiError> {
        match v {
            0 => Ok(Edge::Off),
            1 => Ok(Edge::Rising),
            2 => Ok(Edge::Falling),
            3 => Ok(Edge::Any),
            _ => Err(AbiError::InvalidArgument),
        }
    }

    /// Whether a transition to `level` (true = high) matches this setting.
    pub const fn matches(self, level: bool) -> bool {
        match self {
            Edge::Off => false,
            Edge::Rising => level,
            Edge::Falling => !level,
            Edge::Any => true,
        }
    }
}

/// A validated `timer_start` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimerSpec {
    /// Timer id, `< MAX_TIMERS`.
    pub id: u8,
    /// Period in microseconds, `>= MIN_TIMER_US`.
    pub period_us: u32,
    /// Re-arm after each alarm.
    pub periodic: bool,
}

impl TimerSpec {
    /// Parses `timer_start(id, period_us, periodic)`.
    pub fn parse(id: i32, period_us: i32, periodic: i32) -> Result<Self, AbiError> {
        let id = timer_id(id)?;
        let period_us = u32::try_from(period_us).map_err(|_| AbiError::InvalidArgument)?;
        if period_us < MIN_TIMER_US {
            return Err(AbiError::InvalidArgument);
        }
        let periodic = match periodic {
            0 => false,
            1 => true,
            _ => return Err(AbiError::InvalidArgument),
        };
        Ok(TimerSpec {
            id,
            period_us,
            periodic,
        })
    }
}

/// Parses a timer id.
pub fn timer_id(id: i32) -> Result<u8, AbiError> {
    u8::try_from(id)
        .ok()
        .filter(|&i| usize::from(i) < MAX_TIMERS)
        .ok_or(AbiError::InvalidArgument)
}

/// Parses an optional pin argument: `-1` means unused.
pub fn optional_pin(v: i32) -> Result<Option<u8>, AbiError> {
    match v {
        -1 => Ok(None),
        p => u8::try_from(p)
            .map(Some)
            .map_err(|_| AbiError::InvalidArgument),
    }
}

/// A validated `spi_open` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpiConfig {
    /// Bus number, `< SPI_BUSES`.
    pub bus: u8,
    /// Clock pin.
    pub sck: u8,
    /// Controller-out pin, if any.
    pub mosi: Option<u8>,
    /// Controller-in pin, if any.
    pub miso: Option<u8>,
    /// Hardware chip-select pin, if any.
    pub cs: Option<u8>,
    /// Clock in kHz, `1..=MAX_SPI_KHZ`.
    pub freq_khz: u32,
    /// SPI mode 0–3 (CPOL << 1 | CPHA).
    pub mode: u8,
}

impl SpiConfig {
    /// Parses `spi_open(bus, sck, mosi, miso, cs, freq_khz, mode)`. At least
    /// one of `mosi` / `miso` is required, and pins must be distinct.
    pub fn parse(
        bus: i32,
        sck: i32,
        mosi: i32,
        miso: i32,
        cs: i32,
        freq_khz: i32,
        mode: i32,
    ) -> Result<Self, AbiError> {
        let bad = AbiError::InvalidArgument;
        let bus = spi_bus(bus)?;
        let sck = optional_pin(sck)?.ok_or(bad)?;
        let (mosi, miso, cs) = (optional_pin(mosi)?, optional_pin(miso)?, optional_pin(cs)?);
        if mosi.is_none() && miso.is_none() {
            return Err(bad);
        }
        let pins = [Some(sck), mosi, miso, cs];
        for (i, a) in pins.iter().enumerate() {
            if a.is_some() && pins.iter().skip(i + 1).any(|b| b == a) {
                return Err(bad);
            }
        }
        if !(1..=MAX_SPI_KHZ).contains(&freq_khz) {
            return Err(bad);
        }
        let mode = u8::try_from(mode).ok().filter(|m| *m <= 3).ok_or(bad)?;
        Ok(SpiConfig {
            bus,
            sck,
            mosi,
            miso,
            cs,
            freq_khz: freq_khz as u32,
            mode,
        })
    }
}

/// Parses an SPI bus number.
pub fn spi_bus(bus: i32) -> Result<u8, AbiError> {
    if (0..SPI_BUSES).contains(&bus) {
        Ok(bus as u8)
    } else {
        Err(AbiError::InvalidArgument)
    }
}

/// Validates `spi_transfer` lengths: the transfer clocks
/// `max(tx_len, rx_len)` bytes (transmit is padded with `0x00`), which must
/// be `1..=MAX_DMA`. Returns that length.
pub fn spi_transfer_len(tx_len: i32, rx_len: i32) -> Result<usize, AbiError> {
    let tx = usize::try_from(tx_len).map_err(|_| AbiError::InvalidArgument)?;
    let rx = usize::try_from(rx_len).map_err(|_| AbiError::InvalidArgument)?;
    let n = tx.max(rx);
    if n == 0 || n > MAX_DMA {
        return Err(AbiError::InvalidArgument);
    }
    Ok(n)
}

/// A validated `uart_open` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UartConfig {
    /// Port, always [`UART_PORT`].
    pub port: u8,
    /// Transmit pin, if any.
    pub tx: Option<u8>,
    /// Receive pin, if any.
    pub rx: Option<u8>,
    /// Baud rate within [`UART_BAUD`].
    pub baud: u32,
}

impl UartConfig {
    /// Parses `uart_open(port, tx, rx, baud)`; at least one pin is required.
    pub fn parse(port: i32, tx: i32, rx: i32, baud: i32) -> Result<Self, AbiError> {
        let bad = AbiError::InvalidArgument;
        let port = uart_port(port)?;
        let (tx, rx) = (optional_pin(tx)?, optional_pin(rx)?);
        if (tx.is_none() && rx.is_none()) || (tx.is_some() && tx == rx) {
            return Err(bad);
        }
        if !UART_BAUD.contains(&baud) {
            return Err(bad);
        }
        Ok(UartConfig {
            port,
            tx,
            rx,
            baud: baud as u32,
        })
    }
}

/// Parses a UART port number.
pub fn uart_port(port: i32) -> Result<u8, AbiError> {
    if port == UART_PORT {
        Ok(port as u8)
    } else {
        Err(AbiError::InvalidArgument)
    }
}

/// Validates a `uart_write` length (`1..=MAX_DMA`).
pub fn uart_write_len(len: i32) -> Result<usize, AbiError> {
    usize::try_from(len)
        .ok()
        .filter(|n| (1..=MAX_DMA).contains(n))
        .ok_or(AbiError::InvalidArgument)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges_parse_and_match() {
        assert_eq!(Edge::from_i32(4), Err(AbiError::InvalidArgument));
        assert!(Edge::Rising.matches(true) && !Edge::Rising.matches(false));
        assert!(Edge::Falling.matches(false) && !Edge::Falling.matches(true));
        assert!(Edge::Any.matches(true) && Edge::Any.matches(false));
        assert!(!Edge::Off.matches(true));
    }

    #[test]
    fn timer_specs_are_range_checked() {
        assert_eq!(
            TimerSpec::parse(3, 100, 1),
            Ok(TimerSpec {
                id: 3,
                period_us: 100,
                periodic: true
            })
        );
        assert!(TimerSpec::parse(4, 1000, 0).is_err());
        assert!(TimerSpec::parse(-1, 1000, 0).is_err());
        assert!(TimerSpec::parse(0, 99, 0).is_err());
        assert!(TimerSpec::parse(0, -5, 0).is_err());
        assert!(TimerSpec::parse(0, 1000, 2).is_err());
    }

    #[test]
    fn spi_config_rules() {
        let ok = SpiConfig::parse(0, 6, 7, 2, 10, 1000, 0).unwrap();
        assert_eq!(
            (ok.sck, ok.mosi, ok.miso, ok.cs, ok.freq_khz),
            (6, Some(7), Some(2), Some(10), 1000)
        );
        assert!(SpiConfig::parse(0, 6, 7, -1, -1, 1000, 3).is_ok());
        assert!(SpiConfig::parse(1, 6, 7, 2, 10, 1000, 0).is_err(), "bus");
        assert!(
            SpiConfig::parse(0, -1, 7, 2, 10, 1000, 0).is_err(),
            "sck required"
        );
        assert!(
            SpiConfig::parse(0, 6, -1, -1, 10, 1000, 0).is_err(),
            "no data pin"
        );
        assert!(
            SpiConfig::parse(0, 6, 6, 2, 10, 1000, 0).is_err(),
            "duplicate pin"
        );
        assert!(SpiConfig::parse(0, 6, 7, 2, 10, 0, 0).is_err(), "freq");
        assert!(
            SpiConfig::parse(0, 6, 7, 2, 10, MAX_SPI_KHZ + 1, 0).is_err(),
            "freq"
        );
        assert!(SpiConfig::parse(0, 6, 7, 2, 10, 1000, 4).is_err(), "mode");
        assert!(
            SpiConfig::parse(0, 300, 7, 2, 10, 1000, 0).is_err(),
            "pin range"
        );
    }

    #[test]
    fn transfer_lengths() {
        assert_eq!(spi_transfer_len(4, 0), Ok(4));
        assert_eq!(spi_transfer_len(2, 8), Ok(8));
        assert!(spi_transfer_len(0, 0).is_err());
        assert!(spi_transfer_len(MAX_DMA as i32 + 1, 0).is_err());
        assert!(spi_transfer_len(-1, 4).is_err());
        assert_eq!(uart_write_len(1), Ok(1));
        assert!(uart_write_len(0).is_err());
        assert!(uart_write_len(MAX_DMA as i32 + 1).is_err());
    }

    #[test]
    fn uart_config_rules() {
        assert_eq!(
            UartConfig::parse(1, 4, 5, 115_200),
            Ok(UartConfig {
                port: 1,
                tx: Some(4),
                rx: Some(5),
                baud: 115_200
            })
        );
        assert!(UartConfig::parse(1, 4, -1, 9600).is_ok());
        assert!(UartConfig::parse(0, 4, 5, 9600).is_err(), "console port");
        assert!(UartConfig::parse(1, -1, -1, 9600).is_err());
        assert!(UartConfig::parse(1, 4, 4, 9600).is_err());
        assert!(UartConfig::parse(1, 4, 5, 299).is_err());
        assert!(UartConfig::parse(1, 4, 5, 3_000_001).is_err());
    }
}
