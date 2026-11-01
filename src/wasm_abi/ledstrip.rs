//! Argument validation for the ABI 5 LED-strip imports.
//!
//! An *LED strip* is a chain of WS2812-style one-wire RGB LEDs (WS2812B,
//! SK6812 and compatibles) on one data pin. The payload names the pin and
//! the chain length, then sends whole frames of colour bytes; the host
//! turns them into the chain's pulse train (on the ESP base firmware with
//! the RMT peripheral) and the payload never times a pulse.
//!
//! # `ledstrip_open(pin, count)`
//!
//! Opens a strip of `count` LEDs (`1..=MAX_LEDS`) on board pin `pin` and
//! returns a handle `0..MAX_LEDSTRIPS`. The pin goes through the same
//! checks as `gpio_output`: a pin that does not exist is
//! `InvalidArgument`, a reserved or input-only pin `Unsupported`, and a pin
//! another peripheral (SPI, UART, PWM, I2C, a route) holds
//! `InvalidArgument`. Opening the pin of a strip that is already open
//! changes its length and returns the same handle. A board drives at most
//! [`MAX_LEDSTRIPS`] strips and may support fewer, or a shorter chain than
//! [`MAX_LEDS`]: a further strip is `NoResources`, a chain longer than the
//! board can send in one go `Unsupported`. The data line idles low; opening
//! sends nothing, so the LEDs keep what they showed.
//!
//! # `ledstrip_write(handle, ptr, len)`
//!
//! Sends `len` bytes from guest memory: [`BYTES_PER_LED`] per LED in the
//! chain's wire order, which for WS2812 parts is **G, R, B**, starting with
//! the LED nearest the pin. `len` must be a non-zero multiple of
//! [`BYTES_PER_LED`] covering at most the strip's `count` LEDs
//! ([`frame_len`], [`LedStripSpec::check_frame`]); a shorter frame updates
//! only the first LEDs. The call is synchronous and bounded: it returns
//! once the frame is on the wire, about 30 µs per LED plus the chain's
//! latch gap ([`RESET_US`], which the host waits out before a frame that
//! follows the previous one too closely). A handle that is not open is
//! `Busy`; a failed transmission is `Transport`.
//!
//! # `ledstrip_close(handle)`
//!
//! Closes the strip and returns its pin to idle plain GPIO (the LEDs keep
//! their last colours). A handle that is not open is `Busy`. Stopping the
//! payload closes every strip.
//!
//! None of these raise `on_irq` events.

use super::AbiError;

/// LED strips a payload may open at once (handles `0..MAX_LEDSTRIPS`).
pub const MAX_LEDSTRIPS: usize = 2;

/// Longest chain one strip may have, in LEDs. Boards may support fewer.
pub const MAX_LEDS: usize = 64;

/// Bytes per LED in a frame (G, R, B for WS2812 parts).
pub const BYTES_PER_LED: usize = 3;

/// Largest `ledstrip_write` frame, in bytes.
pub const MAX_FRAME: usize = MAX_LEDS * BYTES_PER_LED;

/// Shortest idle-low gap between two frames, in microseconds: the WS2812B
/// latches its colour after at least 280 µs of low (older parts after
/// 50 µs).
pub const RESET_US: u32 = 300;

/// A chain length, `1..=MAX_LEDS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct LedCount(u8);

impl LedCount {
    /// Parses a `count` argument.
    pub fn new(count: i32) -> Result<Self, AbiError> {
        u8::try_from(count)
            .ok()
            .filter(|&c| c != 0 && usize::from(c) <= MAX_LEDS)
            .map(LedCount)
            .ok_or(AbiError::InvalidArgument)
    }

    /// The number of LEDs.
    pub const fn get(self) -> usize {
        self.0 as usize
    }

    /// Bytes of a frame covering the whole chain.
    pub const fn frame_bytes(self) -> usize {
        self.get() * BYTES_PER_LED
    }
}

/// A validated `ledstrip_open(pin, count)` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedStripSpec {
    /// Board pin of the data line.
    pub pin: u8,
    /// Number of LEDs in the chain.
    pub count: LedCount,
}

impl LedStripSpec {
    /// Parses `ledstrip_open(pin, count)`. The pin is only range checked
    /// here (`0..=255`); the board decides whether it may drive it.
    pub fn parse(pin: i32, count: i32) -> Result<Self, AbiError> {
        let pin = u8::try_from(pin).map_err(|_| AbiError::InvalidArgument)?;
        Ok(LedStripSpec {
            pin,
            count: LedCount::new(count)?,
        })
    }

    /// Checks that a frame of `len` bytes (already a valid [`frame_len`])
    /// fits this strip: at most `count` LEDs.
    pub const fn check_frame(&self, len: usize) -> Result<(), AbiError> {
        if len <= self.count.frame_bytes() {
            Ok(())
        } else {
            Err(AbiError::InvalidArgument)
        }
    }
}

/// Parses a strip handle argument (`0..MAX_LEDSTRIPS`).
pub fn ledstrip_handle(v: i32) -> Result<u8, AbiError> {
    u8::try_from(v)
        .ok()
        .filter(|&h| usize::from(h) < MAX_LEDSTRIPS)
        .ok_or(AbiError::InvalidArgument)
}

/// Parses the `len` of `ledstrip_write`: a non-zero multiple of
/// [`BYTES_PER_LED`], at most [`MAX_FRAME`].
pub fn frame_len(len: i32) -> Result<usize, AbiError> {
    usize::try_from(len)
        .ok()
        .filter(|&n| n != 0 && n <= MAX_FRAME && n % BYTES_PER_LED == 0)
        .ok_or(AbiError::InvalidArgument)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_one_to_max() {
        assert_eq!(LedCount::new(0), Err(AbiError::InvalidArgument));
        assert_eq!(LedCount::new(-1), Err(AbiError::InvalidArgument));
        assert_eq!(LedCount::new(1).map(LedCount::get), Ok(1));
        assert_eq!(
            LedCount::new(MAX_LEDS as i32).map(LedCount::get),
            Ok(MAX_LEDS)
        );
        assert_eq!(
            LedCount::new(MAX_LEDS as i32 + 1),
            Err(AbiError::InvalidArgument)
        );
        assert_eq!(LedCount::new(256), Err(AbiError::InvalidArgument));
        assert_eq!(LedCount::new(8).map(LedCount::frame_bytes), Ok(24));
    }

    #[test]
    fn open_parses_pin_and_count() {
        let spec = LedStripSpec::parse(45, 8).expect("valid");
        assert_eq!((spec.pin, spec.count.get()), (45, 8));
        assert_eq!(LedStripSpec::parse(-1, 8), Err(AbiError::InvalidArgument));
        assert_eq!(LedStripSpec::parse(256, 8), Err(AbiError::InvalidArgument));
        assert_eq!(LedStripSpec::parse(45, 0), Err(AbiError::InvalidArgument));
    }

    #[test]
    fn frames_are_whole_leds_within_the_strip() {
        assert_eq!(frame_len(3), Ok(3));
        assert_eq!(frame_len(MAX_FRAME as i32), Ok(MAX_FRAME));
        for bad in [0, -3, 1, 2, 4, MAX_FRAME as i32 + 3, i32::MAX] {
            assert_eq!(frame_len(bad), Err(AbiError::InvalidArgument), "{bad}");
        }
        let spec = LedStripSpec::parse(45, 8).expect("valid");
        assert_eq!(spec.check_frame(24), Ok(()));
        assert_eq!(spec.check_frame(3), Ok(()));
        assert_eq!(spec.check_frame(27), Err(AbiError::InvalidArgument));
    }

    #[test]
    fn handles_are_bounded() {
        assert_eq!(ledstrip_handle(0), Ok(0));
        assert_eq!(
            ledstrip_handle(MAX_LEDSTRIPS as i32 - 1),
            Ok(MAX_LEDSTRIPS as u8 - 1)
        );
        assert_eq!(
            ledstrip_handle(MAX_LEDSTRIPS as i32),
            Err(AbiError::InvalidArgument)
        );
        assert_eq!(ledstrip_handle(-1), Err(AbiError::InvalidArgument));
    }
}
