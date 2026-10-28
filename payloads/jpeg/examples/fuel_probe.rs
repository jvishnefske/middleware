//! Fuel and code-size probe: a minimal payload that decodes the embedded
//! 240×240 4:2:0 frame (`tests/fixtures/fuel.jpg`) to RGB565, one step per
//! tick, forever.
//!
//! * The first tick of each frame runs `Decoder::new` (marker parsing).
//! * The second decodes one block: wasmi translates functions lazily and
//!   charges 7 fuel per code byte to the first call that runs them, so this
//!   call carries the translation of the whole decode path.
//! * Every following tick runs `decode_blocks` with a budget of
//!   `FUEL_PROBE_BLOCKS` blocks (a compile-time environment variable,
//!   default 90: one full MCU row of the 240-wide frame, 15 MCUs of 6
//!   blocks) and logs `row <y>` for each completed strip and `done` at the
//!   end of the frame.
//!
//! Setting `FUEL_PROBE_HALF` (to anything) decodes at `Scale::Half`.
//!
//! Build with `cargo build --release --example fuel_probe -p mw-jpeg`; the
//! `.wasm` size is the decoder plus a few hundred bytes of payload glue.
//! Bisect the fuel one call needs by running
//! `payload-sim fuel_probe.wasm --virtual --ticks 40 --fuel F` for several
//! `F`: the smallest `F` without a `trap:` line is the cost of the most
//! expensive call (the crate docs list the results).
//!
//! On a host target this file compiles to an empty library, so
//! `cargo test` on the host still builds.
#![cfg_attr(target_arch = "wasm32", no_std)]
#![forbid(unsafe_code)]

#[cfg(target_arch = "wasm32")]
mod probe {
    use mw_jpeg::{Decoder, Rgb565Be, Scale, Step};
    use mw_payload::{GuestCell, LogLine, Payload};

    /// The frame (11 KB, libjpeg quality 80).
    static JPEG: &[u8] = include_bytes!("../tests/fixtures/fuel.jpg");

    /// The decoder, in `.bss` thanks to the all-zero [`Decoder::IDLE`].
    static DEC: GuestCell<Decoder> = GuestCell::new(Decoder::IDLE);

    /// Strip buffer: 240 × 16 RGB565 pixels.
    static STRIP: GuestCell<[u8; 240 * 16 * 2]> = GuestCell::new([0; 240 * 16 * 2]);

    // The decoder size documented in the crate docs.
    const _: () = assert!(core::mem::size_of::<Decoder>() == 4352);

    /// Output scale: `Scale::Half` if `FUEL_PROBE_HALF` is set at compile
    /// time.
    const SCALE: Scale = if option_env!("FUEL_PROBE_HALF").is_some() {
        Scale::Half
    } else {
        Scale::Full
    };

    /// Blocks per `decode_blocks` call.
    const BLOCKS: u16 = parse_u16(option_env!("FUEL_PROBE_BLOCKS"), 90);

    /// Parses a decimal environment value at compile time.
    const fn parse_u16(s: Option<&str>, default: u16) -> u16 {
        let Some(s) = s else { return default };
        let b = s.as_bytes();
        let mut v: u16 = 0;
        let mut i = 0;
        while i < b.len() {
            v = v * 10 + (b[i] - b'0') as u16;
            i += 1;
        }
        v
    }

    /// Probe state.
    pub struct Probe {
        /// The next tick starts a frame.
        between_frames: bool,
        /// The next tick decodes a single block (the first decode of a frame),
        /// so that wasmi's lazy translation of the decode path (charged to
        /// the first call that reaches it) is measured apart from the
        /// steady-state calls.
        warm_up: bool,
    }

    /// Logs `"<what> <n>"`.
    fn log_n(what: &str, n: u32) {
        let mut line = LogLine::new();
        line.push_str(what).push_str(" ").push_u32(n);
        line.log();
    }

    impl Payload for Probe {
        fn init() -> Self {
            if mw_payload::set_tick(10).is_err() {
                mw_payload::log("fuel_probe: set_tick failed");
            }
            log_n("fuel_probe: blocks per call", u32::from(BLOCKS));
            Probe {
                between_frames: true,
                warm_up: false,
            }
        }

        fn tick(&mut self, _now_ms: u32) {
            let (Some(mut dec), Some(mut strip)) = (DEC.borrow_mut(), STRIP.borrow_mut()) else {
                return;
            };
            if self.between_frames {
                self.between_frames = false;
                self.warm_up = true;
                if dec.start(JPEG, SCALE).is_err() {
                    mw_payload::log("header error");
                }
                return;
            }
            let budget = if self.warm_up { 1 } else { BLOCKS };
            self.warm_up = false;
            match dec.decode_blocks::<Rgb565Be>(JPEG, &mut strip[..], budget) {
                Ok(Step::Partial) => {}
                Ok(Step::Row(s)) => log_n("row", u32::from(s.y)),
                Ok(Step::Done) => {
                    mw_payload::log("done");
                    self.between_frames = true;
                }
                Err(_) => {
                    mw_payload::log("decode error");
                    self.between_frames = true;
                }
            }
        }
    }

    mw_payload::payload!(Probe);
}
