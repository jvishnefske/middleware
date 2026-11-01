//! Virtual ABI 5 LED strips (requirement V6): a table of at most
//! [`MAX_LEDSTRIPS`] WS2812-style chains, each remembering the colour every
//! one of its LEDs last received. The model only decides;
//! [`super::SimDevice`] checks the data pin against its other users, owns
//! the table and prints what it reports.
//!
//! * [`LedStrips::open`] takes the first free handle, or, for the pin of a
//!   strip that is already open, keeps that handle and changes its length
//!   (LEDs past a shortened chain are forgotten, LEDs added by a longer
//!   one start dark). A further strip is [`AbiError::NoResources`]. A new
//!   strip's LEDs start dark (`0, 0, 0`): opening sends nothing.
//! * [`LedStrips::write`] overwrites the first `grb.len() / 3` LEDs, so a
//!   short frame updates only the LEDs nearest the pin. A frame longer
//!   than the strip is [`AbiError::InvalidArgument`]
//!   ([`LedStripSpec::check_frame`]).
//! * [`LedStrips::close`] frees the handle and returns the pin it held.
//!
//! Writing to or closing a handle that is not open is [`AbiError::Busy`].
//! Colours are stored in wire order (G, R, B); [`render_rgb`] prints them
//! as `#rrggbb`.

use std::fmt::Write as _;

use middleware::wasm_abi::ledstrip::{BYTES_PER_LED, MAX_LEDSTRIPS};
use wasm_host::{AbiError, LedStripSpec};

/// One open strip: its spec and the G, R, B bytes each LED last received.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Strip {
    spec: LedStripSpec,
    /// `spec.count.frame_bytes()` bytes.
    grb: Vec<u8>,
}

impl Strip {
    fn new(spec: LedStripSpec) -> Self {
        Strip {
            spec,
            grb: vec![0; spec.count.frame_bytes()],
        }
    }

    /// The same strip with a new length, keeping the colours that fit.
    fn resized(self, spec: LedStripSpec) -> Self {
        let mut grb = self.grb;
        grb.resize(spec.count.frame_bytes(), 0);
        Strip { spec, grb }
    }
}

/// The payload's LED strips, by handle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LedStrips {
    slots: [Option<Strip>; MAX_LEDSTRIPS],
}

impl LedStrips {
    /// Opens a strip, or resizes the one already on `spec.pin`, and returns
    /// its handle; `NoResources` when every handle is taken by another pin.
    pub fn open(&mut self, spec: LedStripSpec) -> Result<u8, AbiError> {
        let index = self
            .handle_of(spec.pin)
            .map(usize::from)
            .or_else(|| self.slots.iter().position(Option::is_none))
            .ok_or(AbiError::NoResources)?;
        let slot = self.slots.get_mut(index).ok_or(AbiError::NoResources)?;
        *slot = Some(match slot.take() {
            Some(strip) => strip.resized(spec),
            None => Strip::new(spec),
        });
        u8::try_from(index).map_err(|_| AbiError::NoResources)
    }

    /// Records a frame of G, R, B bytes on the first LEDs of `handle`.
    pub fn write(&mut self, handle: u8, grb: &[u8]) -> Result<(), AbiError> {
        let strip = self.strip_mut(handle)?;
        strip.spec.check_frame(grb.len())?;
        let leds = strip
            .grb
            .get_mut(..grb.len())
            .ok_or(AbiError::InvalidArgument)?;
        leds.copy_from_slice(grb);
        Ok(())
    }

    /// Closes `handle` and returns the pin it held.
    pub fn close(&mut self, handle: u8) -> Result<u8, AbiError> {
        let slot = self
            .slots
            .get_mut(usize::from(handle))
            .ok_or(AbiError::Busy)?;
        let strip = slot.take().ok_or(AbiError::Busy)?;
        Ok(strip.spec.pin)
    }

    /// The handle of the open strip on `pin`, if any.
    pub fn handle_of(&self, pin: u8) -> Option<u8> {
        self.iter()
            .find(|(_, spec, _)| spec.pin == pin)
            .map(|(h, _, _)| h)
    }

    /// Whether an open strip drives `pin`.
    pub fn owns_pin(&self, pin: u8) -> bool {
        self.handle_of(pin).is_some()
    }

    /// The G, R, B bytes every LED of `handle` last received.
    #[cfg(test)]
    pub fn colours(&self, handle: u8) -> Option<&[u8]> {
        let strip = self.slots.get(usize::from(handle))?.as_ref()?;
        Some(&strip.grb)
    }

    /// Every open strip: handle, spec and colours, by handle.
    pub fn iter(&self) -> impl Iterator<Item = (u8, LedStripSpec, &[u8])> + '_ {
        self.slots.iter().enumerate().filter_map(|(h, slot)| {
            let strip = slot.as_ref()?;
            Some((u8::try_from(h).ok()?, strip.spec, strip.grb.as_slice()))
        })
    }

    fn strip_mut(&mut self, handle: u8) -> Result<&mut Strip, AbiError> {
        self.slots
            .get_mut(usize::from(handle))
            .and_then(Option::as_mut)
            .ok_or(AbiError::Busy)
    }
}

/// Renders G, R, B wire bytes as space-separated `#rrggbb` colours, one
/// per whole LED (a trailing partial LED is ignored).
pub fn render_rgb(grb: &[u8]) -> String {
    let mut out = String::with_capacity(grb.len() / BYTES_PER_LED * 8);
    for led in grb.chunks_exact(BYTES_PER_LED) {
        if let [g, r, b] = *led {
            if !out.is_empty() {
                out.push(' ');
            }
            let _ = write!(out, "#{r:02x}{g:02x}{b:02x}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::wasm_abi::ledstrip::MAX_LEDS;

    fn spec(pin: u8, count: i32) -> LedStripSpec {
        LedStripSpec::parse(i32::from(pin), count).expect("valid spec")
    }

    #[test]
    fn open_takes_free_handles_then_runs_out() {
        let mut s = LedStrips::default();
        assert_eq!(s.open(spec(5, 3)), Ok(0));
        assert_eq!(s.open(spec(6, 1)), Ok(1));
        assert_eq!(s.open(spec(7, 1)), Err(AbiError::NoResources));
        assert!(s.owns_pin(5) && s.owns_pin(6) && !s.owns_pin(7));
        assert_eq!(s.colours(0), Some(&[0u8; 9][..]), "opens dark");
    }

    #[test]
    fn reopening_a_pin_keeps_the_handle_and_resizes() {
        let mut s = LedStrips::default();
        s.open(spec(5, 2)).unwrap();
        s.write(0, &[1, 2, 3, 4, 5, 6]).unwrap();
        assert_eq!(s.open(spec(5, 3)), Ok(0), "same handle");
        assert_eq!(s.colours(0), Some(&[1, 2, 3, 4, 5, 6, 0, 0, 0][..]));
        assert_eq!(s.open(spec(5, 1)), Ok(0));
        assert_eq!(s.colours(0), Some(&[1, 2, 3][..]));
        // Reopening works even with every handle taken.
        s.open(spec(6, 1)).unwrap();
        assert_eq!(s.open(spec(6, MAX_LEDS as i32)), Ok(1));
    }

    #[test]
    fn writes_update_the_first_leds_within_the_strip() {
        let mut s = LedStrips::default();
        assert_eq!(s.write(0, &[1, 2, 3]), Err(AbiError::Busy), "not open");
        s.open(spec(5, 2)).unwrap();
        s.write(0, &[1, 2, 3, 4, 5, 6]).unwrap();
        s.write(0, &[9, 9, 9]).unwrap();
        assert_eq!(s.colours(0), Some(&[9, 9, 9, 4, 5, 6][..]));
        assert_eq!(s.write(0, &[0; 9]), Err(AbiError::InvalidArgument));
        assert_eq!(s.write(1, &[0; 3]), Err(AbiError::Busy));
        assert_eq!(s.write(9, &[0; 3]), Err(AbiError::Busy));
    }

    #[test]
    fn close_frees_the_handle_and_returns_the_pin() {
        let mut s = LedStrips::default();
        s.open(spec(5, 1)).unwrap();
        s.open(spec(6, 1)).unwrap();
        assert_eq!(s.close(0), Ok(5));
        assert_eq!(s.close(0), Err(AbiError::Busy));
        assert_eq!(s.close(7), Err(AbiError::Busy));
        assert!(!s.owns_pin(5));
        assert_eq!(s.open(spec(8, 1)), Ok(0), "handle reused");
        let open: Vec<_> = s.iter().map(|(h, sp, _)| (h, sp.pin)).collect();
        assert_eq!(open, [(0, 8), (1, 6)]);
    }

    #[test]
    fn renders_wire_order_as_rgb() {
        assert_eq!(
            render_rgb(&[0, 0xff, 0, 0xff, 0, 0, 0, 0, 0x80]),
            "#ff0000 #00ff00 #000080"
        );
        assert_eq!(render_rgb(&[]), "");
        assert_eq!(render_rgb(&[1, 2, 3, 4]), "#020103");
    }
}
