//! LED strip: an ABI 5 example showing a moving rainbow on a WS2812-style
//! chain.
//!
//! * `init` opens a strip of [`COUNT`] LEDs on data pin [`PIN`] (the
//!   Conagotchi badge's eight WS2812 LEDs on GPIO45) and ticks every
//!   [`TICK_MS`] ms. A strip that cannot be opened is logged and the
//!   payload stays idle.
//! * Each tick sends one frame ([`rainbow`]): the LEDs spread evenly
//!   around an integer hue wheel ([`wheel`]) at [`BRIGHTNESS`], the whole
//!   pattern turning by [`HUE_STEP`] per tick, one revolution every
//!   256 / [`HUE_STEP`] ticks (6.4 s). The first failed write is logged;
//!   after a write succeeds again, the next failure is logged too.
//!
//! The frame is computed by pure functions tested on the host; the payload
//! only holds the strip and the current hue offset.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use core::fmt::Write as _;

use mw_payload::ledstrip::{LedStrip, Rgb};
use mw_payload::{AbiError, LogLine, Payload};

/// Data pin of the strip (GPIO45 on the Conagotchi badge).
pub const PIN: u8 = 45;
/// LEDs in the chain.
pub const COUNT: usize = 8;
/// Tick period, and so the frame period.
pub const TICK_MS: u32 = 50;
/// Hue advance per tick, in wheel positions (256 per revolution).
pub const HUE_STEP: u8 = 2;
/// Brightness of every LED, `0..=255` ([`Rgb::scale`]); modest, since
/// eight LEDs at full white draw about half an ampere.
pub const BRIGHTNESS: u8 = 32;

// COUNT must be a chain length the ABI accepts.
const _: () = assert!(COUNT >= 1 && COUNT <= mw_payload::ledstrip::MAX_LEDS);

/// The colour at position `pos` of a 256-step hue wheel: red at 0, green
/// at 85, blue at 170, fading linearly between them, so every colour has
/// a channel sum of 255.
pub const fn wheel(pos: u8) -> Rgb {
    // Each branch keeps `p * 3` at most 255.
    if pos < 85 {
        Rgb::new(255 - pos * 3, pos * 3, 0)
    } else if pos < 170 {
        let p = pos - 85;
        Rgb::new(0, 255 - p * 3, p * 3)
    } else {
        let p = pos - 170;
        Rgb::new(p * 3, 0, 255 - p * 3)
    }
}

/// The frame for hue offset `offset`: LED `i` shows the wheel at
/// `offset + i * 256 / COUNT`, dimmed to [`BRIGHTNESS`].
pub fn rainbow(offset: u8) -> [Rgb; COUNT] {
    let mut frame = [Rgb::BLACK; COUNT];
    for (i, led) in frame.iter_mut().enumerate() {
        // i < COUNT, so i * 256 / COUNT < 256.
        let spread = (i * 256 / COUNT) as u8;
        *led = wheel(offset.wrapping_add(spread)).scale(BRIGHTNESS);
    }
    frame
}

/// The application state.
pub struct Rainbow {
    strip: Option<LedStrip>,
    offset: u8,
    failing: bool,
}

/// Logs `what` failed with `e`.
fn log_err(what: &str, e: AbiError) {
    let mut line = LogLine::new();
    let _ = write!(line, "{what} failed: {e:?}");
    line.log();
}

impl Payload for Rainbow {
    fn init() -> Self {
        // COUNT fits u8 (checked above against MAX_LEDS).
        let strip = LedStrip::open(PIN, COUNT as u8)
            .map_err(|e| log_err("ledstrip_open", e))
            .ok();
        if strip.is_some()
            && let Err(e) = mw_payload::set_tick(TICK_MS)
        {
            log_err("set_tick", e);
        }
        Rainbow {
            strip,
            offset: 0,
            failing: false,
        }
    }

    fn tick(&mut self, _now_ms: u32) {
        let Some(strip) = self.strip.as_mut() else {
            return;
        };
        match strip.write(&rainbow(self.offset)) {
            Ok(()) => self.failing = false,
            Err(e) if !self.failing => {
                self.failing = true;
                log_err("ledstrip_write", e);
            }
            Err(_) => {}
        }
        self.offset = self.offset.wrapping_add(HUE_STEP);
    }
}

// The exports need a single-threaded wasm32 module; host builds only run
// the unit tests below.
#[cfg(target_arch = "wasm32")]
mw_payload::payload!(Rainbow);

#[cfg(test)]
mod tests {
    use super::*;

    fn sum(c: Rgb) -> u16 {
        u16::from(c.r) + u16::from(c.g) + u16::from(c.b)
    }

    #[test]
    fn wheel_hits_the_primaries() {
        assert_eq!(wheel(0), Rgb::new(255, 0, 0));
        assert_eq!(wheel(85), Rgb::new(0, 255, 0));
        assert_eq!(wheel(170), Rgb::new(0, 0, 255));
        assert_eq!(wheel(255), Rgb::new(255, 0, 0));
        assert_eq!(wheel(42), Rgb::new(129, 126, 0));
    }

    #[test]
    fn wheel_is_continuous_and_full_brightness() {
        for pos in 0..=255u8 {
            let c = wheel(pos);
            assert_eq!(sum(c), 255, "{pos}");
            let n = wheel(pos.wrapping_add(1));
            for (a, b) in [(c.r, n.r), (c.g, n.g), (c.b, n.b)] {
                assert!(a.abs_diff(b) <= 3, "{pos}: {c:?} -> {n:?}");
            }
        }
    }

    #[test]
    fn rainbow_spreads_the_wheel_over_the_strip() {
        let frame = rainbow(0);
        assert_eq!(frame[0], wheel(0).scale(BRIGHTNESS));
        assert_eq!(frame[COUNT / 2], wheel(128).scale(BRIGHTNESS));
        for (i, led) in frame.iter().enumerate() {
            assert_eq!(*led, wheel((i * 256 / COUNT) as u8).scale(BRIGHTNESS));
            assert!(sum(*led) <= 3 * u16::from(BRIGHTNESS), "{i}: modest");
            assert!(sum(*led) > 0, "{i}: lit");
        }
    }

    #[test]
    fn rainbow_moves_with_the_offset() {
        // Shifting the offset by one LED's spacing shifts the frame by one LED.
        let a = rainbow(10);
        let b = rainbow(10 + (256 / COUNT) as u8);
        assert_eq!(a[1..], b[..COUNT - 1]);
        assert_ne!(rainbow(0), rainbow(HUE_STEP));
    }

    #[test]
    fn host_stub_leaves_the_payload_idle() {
        let mut app = Rainbow::init();
        assert!(app.strip.is_none(), "no host: open fails");
        app.tick(0);
        assert_eq!(app.offset, 0, "an idle payload does not advance");
    }
}
