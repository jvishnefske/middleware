//! Argument validation for the ABI 2 (interrupts, timers, SPI, UART) and
//! ABI 3 (PWM, ADC, I2C) peripheral imports.
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

/// PWM timers available to a payload (ids `0..MAX_PWM_TIMERS`, ABI 3).
/// Which hardware timer each id maps to is board documentation.
pub const MAX_PWM_TIMERS: usize = 6;

/// Output channels per PWM timer (`0..PWM_CHANNELS`, ABI 3). All channels
/// of a timer share its frequency.
pub const PWM_CHANNELS: usize = 4;

/// Accepted PWM frequency range, in Hz (ABI 3). The board may refuse a
/// frequency inside it that its timer clock cannot reach (`Unsupported`).
pub const PWM_FREQ_HZ: core::ops::RangeInclusive<i32> = 1..=1_000_000;

/// Largest raw `adc_read` result: readings are 12-bit (`0..=ADC_MAX`), so
/// boards with a different resolution scale to this range (ABI 3).
pub const ADC_MAX: u16 = 4095;

/// I2C buses available to a payload (`0..I2C_BUSES`, ABI 3).
pub const I2C_BUSES: i32 = 2;

/// Accepted I2C clock range, in Hz (ABI 3): 10 kHz up to Fast-mode Plus.
/// Boards limited to standard (100 kHz) or fast mode (400 kHz) refuse
/// higher clocks with `Unsupported`.
pub const I2C_FREQ_HZ: core::ops::RangeInclusive<i32> = 10_000..=1_000_000;

/// Largest write or read part of one `i2c_transfer`, in bytes (ABI 3).
/// 64 bytes covers a register address plus one EEPROM page or a sensor's
/// whole register block, keeps the host's copy buffers on the stack, and
/// bounds how long the synchronous call blocks (about 6 ms per direction at
/// 100 kHz).
pub const MAX_I2C: usize = 64;

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

/// A validated `pwm_open` request (ABI 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PwmSpec {
    /// Timer id, `< MAX_PWM_TIMERS`.
    pub timer: u8,
    /// Frequency in Hz, within [`PWM_FREQ_HZ`].
    pub freq_hz: u32,
}

impl PwmSpec {
    /// Parses `pwm_open(timer, freq_hz)`.
    pub fn parse(timer: i32, freq_hz: i32) -> Result<Self, AbiError> {
        let timer = pwm_timer(timer)?;
        if !PWM_FREQ_HZ.contains(&freq_hz) {
            return Err(AbiError::InvalidArgument);
        }
        Ok(PwmSpec {
            timer,
            freq_hz: freq_hz as u32,
        })
    }
}

/// A validated `pwm_set` request (ABI 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PwmDuty {
    /// Timer id, `< MAX_PWM_TIMERS`.
    pub timer: u8,
    /// Channel, `< PWM_CHANNELS`.
    pub channel: u8,
    /// High time as a fraction of `u16::MAX` (0 = always low, 65535 =
    /// always high).
    pub duty: u16,
}

impl PwmDuty {
    /// Parses `pwm_set(timer, channel, duty)`; `duty` must fit a `u16`.
    pub fn parse(timer: i32, channel: i32, duty: i32) -> Result<Self, AbiError> {
        let bad = AbiError::InvalidArgument;
        let timer = pwm_timer(timer)?;
        let channel = u8::try_from(channel)
            .ok()
            .filter(|&c| usize::from(c) < PWM_CHANNELS)
            .ok_or(bad)?;
        let duty = u16::try_from(duty).map_err(|_| bad)?;
        Ok(PwmDuty {
            timer,
            channel,
            duty,
        })
    }
}

/// Parses a PWM timer id.
pub fn pwm_timer(timer: i32) -> Result<u8, AbiError> {
    u8::try_from(timer)
        .ok()
        .filter(|&t| usize::from(t) < MAX_PWM_TIMERS)
        .ok_or(AbiError::InvalidArgument)
}

/// Parses the `pin` argument of `adc_read`.
pub fn adc_pin(pin: i32) -> Result<u8, AbiError> {
    u8::try_from(pin).map_err(|_| AbiError::InvalidArgument)
}

/// A device's ADC reading as the `adc_read` result: clamped to
/// `0..=ADC_MAX`, so it is never mistaken for an error code or exceeds the
/// documented 12-bit range.
pub const fn adc_result(raw: u16) -> i32 {
    if raw > ADC_MAX {
        ADC_MAX as i32
    } else {
        raw as i32
    }
}

/// A validated `i2c_open` request (ABI 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct I2cConfig {
    /// Bus number, `< I2C_BUSES`.
    pub bus: u8,
    /// Clock in Hz, within [`I2C_FREQ_HZ`].
    pub freq_hz: u32,
}

impl I2cConfig {
    /// Parses `i2c_open(bus, freq_hz)`.
    pub fn parse(bus: i32, freq_hz: i32) -> Result<Self, AbiError> {
        let bus = i2c_bus(bus)?;
        if !I2C_FREQ_HZ.contains(&freq_hz) {
            return Err(AbiError::InvalidArgument);
        }
        Ok(I2cConfig {
            bus,
            freq_hz: freq_hz as u32,
        })
    }
}

/// Parses an I2C bus number.
pub fn i2c_bus(bus: i32) -> Result<u8, AbiError> {
    if (0..I2C_BUSES).contains(&bus) {
        Ok(bus as u8)
    } else {
        Err(AbiError::InvalidArgument)
    }
}

/// The non-pointer arguments of a validated `i2c_transfer` (ABI 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct I2cTransfer {
    /// Bus number, `< I2C_BUSES`.
    pub bus: u8,
    /// 7-bit target address, `<= 0x7F`.
    pub addr: u8,
    /// Bytes written before the repeated start, `0..=MAX_I2C`.
    pub tx_len: usize,
    /// Bytes read after it, `0..=MAX_I2C`.
    pub rx_len: usize,
}

impl I2cTransfer {
    /// Parses `i2c_transfer(bus, addr7, _, tx_len, _, rx_len)`. Both lengths
    /// may be 0 (an address-only probe; boards that cannot issue one refuse
    /// it with `Unsupported`).
    pub fn parse(bus: i32, addr7: i32, tx_len: i32, rx_len: i32) -> Result<Self, AbiError> {
        let bad = AbiError::InvalidArgument;
        let bus = i2c_bus(bus)?;
        let addr = u8::try_from(addr7).ok().filter(|&a| a <= 0x7F).ok_or(bad)?;
        let len = |n: i32| usize::try_from(n).ok().filter(|&n| n <= MAX_I2C).ok_or(bad);
        Ok(I2cTransfer {
            bus,
            addr,
            tx_len: len(tx_len)?,
            rx_len: len(rx_len)?,
        })
    }
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

    #[test]
    fn pwm_rules() {
        assert_eq!(
            PwmSpec::parse(5, 20_000),
            Ok(PwmSpec {
                timer: 5,
                freq_hz: 20_000
            })
        );
        assert!(PwmSpec::parse(0, 1).is_ok());
        assert!(PwmSpec::parse(0, 1_000_000).is_ok());
        assert!(
            PwmSpec::parse(MAX_PWM_TIMERS as i32, 1000).is_err(),
            "timer"
        );
        assert!(PwmSpec::parse(-1, 1000).is_err(), "timer");
        assert!(PwmSpec::parse(0, 0).is_err(), "freq");
        assert!(PwmSpec::parse(0, 1_000_001).is_err(), "freq");
        assert!(PwmSpec::parse(0, -5).is_err(), "freq");
        assert_eq!(
            PwmDuty::parse(1, 3, 65_535),
            Ok(PwmDuty {
                timer: 1,
                channel: 3,
                duty: u16::MAX
            })
        );
        assert!(PwmDuty::parse(1, 0, 0).is_ok());
        assert!(
            PwmDuty::parse(1, PWM_CHANNELS as i32, 0).is_err(),
            "channel"
        );
        assert!(PwmDuty::parse(1, -1, 0).is_err(), "channel");
        assert!(PwmDuty::parse(6, 0, 0).is_err(), "timer");
        assert!(PwmDuty::parse(1, 0, 65_536).is_err(), "duty");
        assert!(PwmDuty::parse(1, 0, -1).is_err(), "duty");
        assert_eq!(pwm_timer(5), Ok(5));
        assert!(pwm_timer(6).is_err());
    }

    #[test]
    fn adc_rules() {
        assert_eq!(adc_pin(0), Ok(0));
        assert_eq!(adc_pin(255), Ok(255));
        assert!(adc_pin(256).is_err());
        assert!(adc_pin(-1).is_err());
        assert_eq!(adc_result(0), 0);
        assert_eq!(adc_result(ADC_MAX), 4095);
        assert_eq!(adc_result(u16::MAX), 4095, "clamped");
    }

    #[test]
    fn i2c_rules() {
        assert_eq!(
            I2cConfig::parse(1, 400_000),
            Ok(I2cConfig {
                bus: 1,
                freq_hz: 400_000
            })
        );
        assert!(I2cConfig::parse(0, 10_000).is_ok());
        assert!(I2cConfig::parse(0, 1_000_000).is_ok());
        assert!(I2cConfig::parse(2, 100_000).is_err(), "bus");
        assert!(I2cConfig::parse(-1, 100_000).is_err(), "bus");
        assert!(I2cConfig::parse(0, 9_999).is_err(), "freq");
        assert!(I2cConfig::parse(0, 1_000_001).is_err(), "freq");
        assert!(I2cConfig::parse(0, 0).is_err(), "freq");

        assert_eq!(
            I2cTransfer::parse(1, 0x7F, 1, MAX_I2C as i32),
            Ok(I2cTransfer {
                bus: 1,
                addr: 0x7F,
                tx_len: 1,
                rx_len: MAX_I2C
            })
        );
        assert!(I2cTransfer::parse(0, 0x50, 0, 0).is_ok(), "probe");
        assert!(I2cTransfer::parse(0, 0x50, 4, 0).is_ok(), "write only");
        assert!(I2cTransfer::parse(0, 0x50, 0, 4).is_ok(), "read only");
        assert!(I2cTransfer::parse(0, 0x80, 1, 1).is_err(), "addr");
        assert!(I2cTransfer::parse(0, -1, 1, 1).is_err(), "addr");
        assert!(I2cTransfer::parse(2, 0x50, 1, 1).is_err(), "bus");
        let over = MAX_I2C as i32 + 1;
        assert!(I2cTransfer::parse(0, 0x50, over, 0).is_err(), "tx len");
        assert!(I2cTransfer::parse(0, 0x50, 0, over).is_err(), "rx len");
        assert!(I2cTransfer::parse(0, 0x50, -1, 0).is_err(), "tx len");
        assert!(I2cTransfer::parse(0, 0x50, 0, -1).is_err(), "rx len");
    }
}
