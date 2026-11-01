//! WS2812-style LED strips through the `ledstrip_open`, `ledstrip_write`
//! and `ledstrip_close` imports (ABI 5).
//!
//! An LED strip is a chain of one-wire RGB LEDs (WS2812B, SK6812 and
//! compatibles) on one data pin. [`LedStrip::open`] names the pin and the
//! chain length; [`LedStrip::write`] sends a frame of [`Rgb`] colours,
//! starting with the LED nearest the pin. The host generates the pulse
//! train, so the payload never times a pulse; a write is synchronous and
//! returns once the frame is on the wire (about 30 µs per LED plus the
//! [`RESET_US`] latch gap). Strips raise no [`irq`](crate::irq) events.
//!
//! The wire order of WS2812 parts is G, R, B. [`Rgb::grb`] and [`encode`]
//! turn colours into that order in a fixed [`MAX_FRAME`]-byte stack
//! buffer, so nothing is allocated; [`LedStrip::write_grb`] sends bytes
//! already in wire order. A frame may cover fewer LEDs than the strip has
//! (only the first LEDs change) but never more.
//!
//! Arguments are validated with the ABI's own parsers
//! ([`LedStripSpec::parse`], [`frame_len`], [`LedStripSpec::check_frame`])
//! before the host is called, so a bad pin, count or frame length fails
//! with [`AbiError::InvalidArgument`] even without a host. Whether the
//! board may drive the pin, and how long a chain it supports, only the
//! host knows: an unusable pin is `Unsupported` or `InvalidArgument`, a
//! third strip `NoResources` ([`MAX_LEDSTRIPS`]).
//!
//! Dropping a [`LedStrip`] does *not* close it (as with
//! [`Route`](crate::route::Route)): closing is a host call that can fail,
//! so it is the explicit, fallible [`LedStrip::close`]. Stopping the
//! payload closes every strip. The LEDs keep their last colours either way.

use crate::{AbiError, check, ptr_i32, sys};
pub use middleware::wasm_abi::ledstrip::{
    BYTES_PER_LED, LedCount, LedStripSpec, MAX_FRAME, MAX_LEDS, MAX_LEDSTRIPS, RESET_US, frame_len,
    ledstrip_handle,
};

/// One LED's colour, 8 bits per channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rgb {
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
}

impl Rgb {
    /// All channels off.
    pub const BLACK: Rgb = Rgb::new(0, 0, 0);

    /// A colour from its red, green and blue levels.
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Rgb { r, g, b }
    }

    /// The colour in WS2812 wire order: G, R, B.
    pub const fn grb(self) -> [u8; BYTES_PER_LED] {
        [self.g, self.r, self.b]
    }

    /// The colour dimmed by `(brightness + 1) / 256`, rounding down: 255
    /// keeps it unchanged and 0 turns it off. Integer only.
    pub const fn scale(self, brightness: u8) -> Self {
        const fn level(c: u8, brightness: u8) -> u8 {
            // c · (brightness + 1) ≤ 255 · 256, so the shifted value fits u8.
            ((c as u16 * (brightness as u16 + 1)) >> 8) as u8
        }
        Rgb {
            r: level(self.r, brightness),
            g: level(self.g, brightness),
            b: level(self.b, brightness),
        }
    }
}

/// Encodes `colors` in wire order (G, R, B per LED) into `out` and returns
/// the frame length in bytes.
///
/// `InvalidArgument` for no colours or more than [`MAX_LEDS`] of them (the
/// longest frame any strip takes); `out` is then left unchanged.
pub fn encode(colors: &[Rgb], out: &mut [u8; MAX_FRAME]) -> Result<usize, AbiError> {
    if colors.is_empty() || colors.len() > MAX_LEDS {
        return Err(AbiError::InvalidArgument);
    }
    for (chunk, c) in out.chunks_exact_mut(BYTES_PER_LED).zip(colors) {
        chunk.copy_from_slice(&c.grb());
    }
    Ok(colors.len() * BYTES_PER_LED)
}

/// An open LED strip.
#[derive(Debug, PartialEq, Eq)]
pub struct LedStrip {
    handle: u8,
    spec: LedStripSpec,
}

impl LedStrip {
    /// Parses the host's `ledstrip_open` result: a handle in
    /// `0..MAX_LEDSTRIPS`, or an error code. A handle out of range (a
    /// misbehaving host) is `Unsupported`.
    fn opened(code: i32, spec: LedStripSpec) -> Result<Self, AbiError> {
        let handle = ledstrip_handle(check(code)?).map_err(|_| AbiError::Unsupported)?;
        Ok(LedStrip { handle, spec })
    }

    /// Opens a strip of `count` LEDs (`1..=MAX_LEDS`) on board pin `pin`.
    /// Sends nothing, so the LEDs keep what they show. Opening the pin of
    /// an open strip changes its length and returns the same handle.
    ///
    /// `InvalidArgument` (before calling the host) for a count of 0 or
    /// above [`MAX_LEDS`]; from the host, `InvalidArgument` for a pin that
    /// does not exist or another peripheral holds, `Unsupported` for a
    /// reserved pin or a chain longer than the board can send, and
    /// `NoResources` when [`MAX_LEDSTRIPS`] strips are open.
    pub fn open(pin: u8, count: u8) -> Result<Self, AbiError> {
        let spec = LedStripSpec::parse(i32::from(pin), i32::from(count))?;
        Self::opened(
            sys::ledstrip_open(i32::from(spec.pin), spec.count.get() as i32),
            spec,
        )
    }

    /// The handle the host assigned.
    pub const fn handle(&self) -> u8 {
        self.handle
    }

    /// The data pin.
    pub const fn pin(&self) -> u8 {
        self.spec.pin
    }

    /// The number of LEDs in the chain.
    pub const fn count(&self) -> LedCount {
        self.spec.count
    }

    /// Checks a frame of `len` bytes against the ABI and this strip, and
    /// returns its length as the ABI's `i32`.
    fn frame(&self, len: usize) -> Result<i32, AbiError> {
        let len = frame_len(i32::try_from(len).unwrap_or(i32::MAX))?;
        self.spec.check_frame(len)?;
        // frame_len bounds len by MAX_FRAME, so it fits.
        i32::try_from(len).map_err(|_| AbiError::InvalidArgument)
    }

    /// Shows `colors` on the first `colors.len()` LEDs (the rest keep their
    /// colours), encoding them to G, R, B in a [`MAX_FRAME`]-byte stack
    /// buffer.
    ///
    /// `InvalidArgument` (before calling the host) for no colours or more
    /// than the strip's [`count`](Self::count); from the host, `Busy` for a
    /// strip that is no longer open and `Transport` for a failed send.
    pub fn write(&mut self, colors: &[Rgb]) -> Result<(), AbiError> {
        let mut buf = [0u8; MAX_FRAME];
        let n = encode(colors, &mut buf)?;
        self.write_grb(buf.get(..n).ok_or(AbiError::InvalidArgument)?)
    }

    /// Sends `grb`, already in wire order ([`BYTES_PER_LED`] bytes per LED,
    /// G, R, B), to the first LEDs.
    ///
    /// `InvalidArgument` (before calling the host) unless `grb.len()` is a
    /// non-zero multiple of [`BYTES_PER_LED`] covering at most the strip's
    /// [`count`](Self::count); host errors as for [`write`](Self::write).
    pub fn write_grb(&mut self, grb: &[u8]) -> Result<(), AbiError> {
        let len = self.frame(grb.len())?;
        check(sys::ledstrip_write(
            i32::from(self.handle),
            ptr_i32(grb),
            len,
        ))
        .map(|_| ())
    }

    /// Closes the strip; its pin returns to idle plain GPIO and the LEDs
    /// keep their last colours. `Busy` if the host no longer has it open.
    pub fn close(self) -> Result<(), AbiError> {
        check(sys::ledstrip_close(i32::from(self.handle))).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(count: i32) -> LedStrip {
        LedStrip {
            handle: 1,
            spec: LedStripSpec::parse(45, count).expect("valid"),
        }
    }

    #[test]
    fn grb_is_wire_order() {
        assert_eq!(Rgb::new(1, 2, 3).grb(), [2, 1, 3]);
        assert_eq!(Rgb::BLACK.grb(), [0, 0, 0]);
        assert_eq!(Rgb::default(), Rgb::BLACK);
    }

    #[test]
    fn scale_spans_off_to_unchanged() {
        let c = Rgb::new(255, 128, 1);
        assert_eq!(c.scale(255), c);
        assert_eq!(c.scale(0), Rgb::BLACK);
        assert_eq!(c.scale(127), Rgb::new(127, 64, 0));
        assert_eq!(c.scale(31), Rgb::new(31, 16, 0));
        // Never brighter than the input, for every level and brightness.
        for v in 0..=255u8 {
            for b in [0u8, 1, 63, 127, 254, 255] {
                assert!(Rgb::new(v, v, v).scale(b).r <= v);
            }
        }
    }

    #[test]
    fn encode_writes_grb_per_led() {
        let mut out = [0xAA; MAX_FRAME];
        let colors = [Rgb::new(1, 2, 3), Rgb::new(4, 5, 6)];
        assert_eq!(encode(&colors, &mut out), Ok(6));
        assert_eq!(out[..6], [2, 1, 3, 5, 4, 6]);
        assert_eq!(out[6], 0xAA, "bytes past the frame are untouched");
        let full = [Rgb::new(9, 8, 7); MAX_LEDS];
        assert_eq!(encode(&full, &mut out), Ok(MAX_FRAME));
        assert_eq!(out[MAX_FRAME - 3..], [8, 9, 7]);
    }

    #[test]
    fn encode_refuses_empty_and_oversized_frames() {
        let mut out = [0u8; MAX_FRAME];
        assert_eq!(encode(&[], &mut out), Err(AbiError::InvalidArgument));
        let long = [Rgb::BLACK; MAX_LEDS + 1];
        assert_eq!(encode(&long, &mut out), Err(AbiError::InvalidArgument));
        assert_eq!(out, [0u8; MAX_FRAME]);
    }

    #[test]
    fn open_validates_before_the_host() {
        assert_eq!(LedStrip::open(45, 0), Err(AbiError::InvalidArgument));
        assert_eq!(
            LedStrip::open(45, MAX_LEDS as u8 + 1),
            Err(AbiError::InvalidArgument)
        );
        assert_eq!(LedStrip::open(45, 8), Err(AbiError::Unsupported), "no host");
        assert_eq!(
            LedStrip::open(0, MAX_LEDS as u8),
            Err(AbiError::Unsupported),
            "valid; no host"
        );
    }

    #[test]
    fn opened_checks_the_handle() {
        let spec = LedStripSpec::parse(45, 8).expect("valid");
        let s = LedStrip::opened(1, spec).expect("valid handle");
        assert_eq!((s.handle(), s.pin(), s.count().get()), (1, 45, 8));
        assert_eq!(
            LedStrip::opened(MAX_LEDSTRIPS as i32, spec),
            Err(AbiError::Unsupported)
        );
        assert_eq!(
            LedStrip::opened(AbiError::NoResources.code(), spec),
            Err(AbiError::NoResources)
        );
        assert_eq!(s.close(), Err(AbiError::Unsupported), "host stub");
    }

    #[test]
    fn frames_are_checked_against_the_strip() {
        let s = strip(8);
        assert_eq!(s.frame(3), Ok(3));
        assert_eq!(s.frame(24), Ok(24));
        for bad in [0, 1, 4, 27, MAX_FRAME, usize::MAX] {
            assert_eq!(s.frame(bad), Err(AbiError::InvalidArgument), "{bad}");
        }
        assert_eq!(
            strip(MAX_LEDS as i32).frame(MAX_FRAME),
            Ok(MAX_FRAME as i32)
        );
    }

    #[test]
    fn writes_validate_before_the_host() {
        let mut s = strip(2);
        let bad = Err(AbiError::InvalidArgument);
        assert_eq!(s.write(&[]), bad);
        assert_eq!(s.write(&[Rgb::BLACK; 3]), bad, "longer than the strip");
        assert_eq!(s.write_grb(&[0; 4]), bad, "not whole LEDs");
        assert_eq!(s.write_grb(&[0; 9]), bad, "longer than the strip");
        assert_eq!(
            s.write(&[Rgb::BLACK]),
            Err(AbiError::Unsupported),
            "no host"
        );
        assert_eq!(s.write_grb(&[0; 6]), Err(AbiError::Unsupported), "no host");
    }
}
