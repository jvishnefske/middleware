//! Interpreter benchmark: measures how fast a host runs WASM, in fuel per
//! second, with two fixed workloads of known fuel cost.
//!
//! A chain of one-shot timer events ([`STEP_TIMER`], [`STEP_GAP_US`]
//! apart, the pacing `cam_jpeg_view` uses for its decoder) runs one step
//! each, alternating between:
//!
//! * **mix** — [`mix`]: [`MIX_ROUNDS`] rounds of integer mixing with a
//!   load and a store into a 4 KiB table in linear memory (multiply,
//!   rotate, xor, data-dependent addresses). About [`MIX_FUEL`] fuel per
//!   step.
//! * **jpeg** — the badge's real workload: `mw-jpeg` decoding a 240×240
//!   4:2:0 frame [`JPEG`] to RGB565, [`MCUS_PER_STEP`] MCUs per step as
//!   `cam_jpeg_view` does, then starting over. The frame is the fake
//!   camera's `fixtures/cam/jpeg240/frame_03.jpg` (4 579 bytes), the one
//!   `cam_jpeg_view`'s tests decode, copied next to this crate so the
//!   deploy build stays reproducible from `payloads/` alone
//!   (`scripts/check-reproducible.sh` copies only that). A bigger frame
//!   would raise the heap bound by about 2.75 bytes per image byte; this
//!   one keeps the bound below `cam_jpeg_view`'s. A frame is
//!   [`JPEG_STEPS_PER_FRAME`] steps and about [`JPEG_FRAME_FUEL`] fuel.
//!
//! Each step times itself with `now_us`. Every [`REPORT_MS`] the `tick`
//! logs, per workload, the steps run, their average and largest duration,
//! and the fuel rate that implies (the fuel constant over the measured
//! time), then starts a new window:
//!
//! ```text
//! bench: mix <steps> steps, avg <us> us, max <us> us, <k>k fuel/s
//! bench: jpeg <steps> steps, <n> frames, <us> us/step, max <us> us, <ms> ms/frame, <k>k fuel/s, rx <n>
//! ```
//!
//! `ms/frame` is the wall time of the last frame from its first step to its
//! last, gaps and the interleaved mix steps included; the fuel rates count
//! only the time inside the steps.
//!
//! The host's own report (esp-base's `perf:` lines on `sys/log`) gives the
//! measured fuel and busy time of the same calls; the two should agree.
//! The payload also subscribes to [`RX_KEY`] (the JPEG camera stream) and
//! only counts what arrives (`rx`), so with a camera streaming nearby the
//! host delivers the same message load as to `cam_jpeg_view`, and the
//! rates show what that load costs the interpreter.
//!
//! The fuel constants are measured in `payload-sim`
//! (`tools/payload-sim/tests/bench.rs`, which fails if they drift by more
//! than 5 %); they include each step's own overhead (the `on_irq` call, the
//! timer re-arm and the two clock reads). Measured: 196 564 fuel per mix
//! step (28 per round), 12 786 673 per JPEG frame (`cam_jpeg_view`'s
//! 13.2 M per frame is this plus reception and drawing), at most 255 360 in
//! one step.
//!
//! # Memory
//!
//! Deploy the byte-page build (`payloads/scripts/build-byte-pages.sh -p
//! bench`): linear memory fixed at the `memory` of
//! `[package.metadata.byte-pages]` in `Cargo.toml` with an 8 KiB stack,
//! enough for the decoder (`cam_jpeg_view` runs it in 3 KiB). The
//! decoder (4 352 bytes), the strip (7 680) and the mix table (4 096) are
//! all-zero statics in `.bss`; the frame is in the data section.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use mw_jpeg::{Decoder, Rgb565Be, Scale, Step};
use mw_payload::LogLine;

/// The timer that paces the steps.
pub const STEP_TIMER: u8 = 0;
/// Gap between two steps, in microseconds.
pub const STEP_GAP_US: u32 = 1_000;
/// Report period (the `tick`), in milliseconds.
pub const REPORT_MS: u32 = 5_000;
/// Words in the mix table (4 KiB).
pub const MIX_WORDS: usize = 1024;
/// Mixing rounds per mix step.
pub const MIX_ROUNDS: u32 = 7_000;
/// Fuel of one mix step (see the crate docs).
pub const MIX_FUEL: u32 = 196_600;
/// MCUs per JPEG step.
pub const MCUS_PER_STEP: u16 = 4;
/// JPEG steps per frame: one `start`, four calls per 15-MCU row for 15
/// rows (60), and the call that returns `Done`.
pub const JPEG_STEPS_PER_FRAME: u32 = 62;
/// Fuel of one whole frame's JPEG steps (see the crate docs).
pub const JPEG_FRAME_FUEL: u32 = 12_790_000;
/// The stream counted, not decoded: `cam_jpeg_stream`'s key.
pub const RX_KEY: &str = "app/cam/jpeg";
/// Strip buffer: 16 rows of 240 RGB565 pixels.
pub const STRIP_BYTES: usize = 240 * 16 * 2;

/// The frame decoded by the jpeg steps.
pub static JPEG: &[u8] = include_bytes!("../frame_03.jpg");

/// One mix step: `rounds` rounds over `table` starting from `seed`;
/// returns the final state (the next seed). Every round reads and writes
/// one word at a data-dependent index, so neither the loop nor the memory
/// traffic can be optimised away.
pub fn mix(table: &mut [u32; MIX_WORDS], seed: u32, rounds: u32) -> u32 {
    let mut x = seed;
    for _ in 0..rounds {
        let i = (x as usize) & (MIX_WORDS - 1);
        let w = table[i].wrapping_mul(0x9E37_79B1) ^ x;
        table[i] = w;
        x = x.rotate_left(5).wrapping_add(w) ^ 0x7F4A_7C15;
    }
    x
}

/// Durations of one workload's steps in a window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Timing {
    /// Steps run.
    pub steps: u32,
    /// Their total duration, in microseconds.
    pub total_us: u64,
    /// The longest one, in microseconds.
    pub max_us: u32,
}

impl Timing {
    /// One more step of `us` microseconds.
    #[must_use]
    pub fn with_step(self, us: u32) -> Timing {
        Timing {
            steps: self.steps.saturating_add(1),
            total_us: self.total_us.saturating_add(u64::from(us)),
            max_us: self.max_us.max(us),
        }
    }

    /// Average step duration, in microseconds (0 without steps).
    pub fn avg_us(&self) -> u32 {
        if self.steps == 0 {
            return 0;
        }
        u32::try_from(self.total_us / u64::from(self.steps)).unwrap_or(u32::MAX)
    }
}

/// Fuel per second when `fuel` took `us` microseconds (0 when `us` is 0).
pub fn fuel_per_s(fuel: u64, us: u64) -> u32 {
    if us == 0 {
        return 0;
    }
    u32::try_from(u128::from(fuel) * 1_000_000 / u128::from(us)).unwrap_or(u32::MAX)
}

/// Which workload the next step runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Work {
    /// A [`mix`] step.
    Mix,
    /// A JPEG step.
    Jpeg,
}

/// The JPEG workload's progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frame {
    /// The next JPEG step starts a frame.
    Start,
    /// Decoding; `us` is the step time spent on this frame so far, `at_ms`
    /// when it started.
    Decoding {
        /// Step time spent on this frame so far, in microseconds.
        us: u32,
        /// When its first step ran.
        at_ms: u32,
    },
}

/// One report window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Window {
    /// The mix steps.
    pub mix: Timing,
    /// The JPEG steps.
    pub jpeg: Timing,
    /// Frames decoded completely.
    pub frames: u32,
    /// Step time of those frames, in microseconds.
    pub frames_us: u64,
    /// Wall time of the last frame, first step to last, in milliseconds.
    pub last_frame_ms: u32,
    /// Messages received on [`RX_KEY`].
    pub rx: u32,
}

/// Benchmark state; the buffers are passed in (statics on wasm32).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bench {
    /// The next step's workload.
    pub next: Work,
    /// The JPEG workload's progress.
    pub frame: Frame,
    /// The mix state.
    pub seed: u32,
    /// The current window.
    pub window: Window,
    /// Decoder errors so far (none expected: the frame is a fixture).
    pub errors: u32,
}

impl Default for Bench {
    fn default() -> Self {
        Bench::new()
    }
}

impl Bench {
    /// Before the first step.
    pub const fn new() -> Self {
        Bench {
            next: Work::Mix,
            frame: Frame::Start,
            seed: 1,
            window: Window {
                mix: Timing { steps: 0, total_us: 0, max_us: 0 },
                jpeg: Timing { steps: 0, total_us: 0, max_us: 0 },
                frames: 0,
                frames_us: 0,
                last_frame_ms: 0,
                rx: 0,
            },
            errors: 0,
        }
    }

    /// Runs one step; `clock()` is read before and after it (microseconds,
    /// wrapping), `now_ms` is the time of the step.
    pub fn step(
        &mut self,
        bufs: &mut Buffers,
        now_ms: u32,
        mut clock: impl FnMut() -> u32,
    ) {
        let t0 = clock();
        match self.next {
            Work::Mix => {
                self.seed = mix(&mut bufs.table, self.seed, MIX_ROUNDS);
                let us = clock().wrapping_sub(t0);
                self.window.mix = self.window.mix.with_step(us);
                self.next = Work::Jpeg;
            }
            Work::Jpeg => {
                let done = self.jpeg_step(bufs);
                let us = clock().wrapping_sub(t0);
                self.window.jpeg = self.window.jpeg.with_step(us);
                self.next = Work::Mix;
                self.frame = match (self.frame, done) {
                    (Frame::Start, false) => Frame::Decoding { us, at_ms: now_ms },
                    (Frame::Start, true) => Frame::Start,
                    (Frame::Decoding { us: so_far, at_ms }, false) => Frame::Decoding {
                        us: so_far.saturating_add(us),
                        at_ms,
                    },
                    (Frame::Decoding { us: so_far, at_ms }, true) => {
                        let w = &mut self.window;
                        w.frames = w.frames.saturating_add(1);
                        w.frames_us = w.frames_us.saturating_add(u64::from(so_far.saturating_add(us)));
                        w.last_frame_ms = now_ms.wrapping_sub(at_ms);
                        Frame::Start
                    }
                };
            }
        }
    }

    /// One JPEG step: `start` when no frame runs, else one bounded decode
    /// call. Returns whether the frame ended (done, or failed: counted in
    /// [`Bench::errors`] and started over).
    fn jpeg_step(&mut self, bufs: &mut Buffers) -> bool {
        let result = match self.frame {
            Frame::Start => bufs.dec.start(JPEG, Scale::Full).map(|()| false),
            Frame::Decoding { .. } => bufs
                .dec
                .decode_mcus::<Rgb565Be>(JPEG, &mut bufs.strip, MCUS_PER_STEP)
                .map(|s| s == Step::Done),
        };
        match result {
            Ok(done) => done,
            Err(_) => {
                self.errors = self.errors.saturating_add(1);
                true
            }
        }
    }

    /// Counts a message on [`RX_KEY`].
    pub fn received(&mut self) {
        self.window.rx = self.window.rx.saturating_add(1);
    }

    /// Logs the window and starts a new one.
    pub fn report(&mut self) {
        let w = self.window;
        let mix_fuel = u64::from(w.mix.steps) * u64::from(MIX_FUEL);
        let mut line = LogLine::new();
        line.push_str("bench: mix ")
            .push_u32(w.mix.steps)
            .push_str(" steps, avg ")
            .push_u32(w.mix.avg_us())
            .push_str(" us, max ")
            .push_u32(w.mix.max_us)
            .push_str(" us, ")
            .push_u32(fuel_per_s(mix_fuel, w.mix.total_us) / 1000)
            .push_str("k fuel/s");
        line.log();
        let jpeg_fuel = u64::from(w.frames) * u64::from(JPEG_FRAME_FUEL);
        let mut line = LogLine::new();
        line.push_str("bench: jpeg ")
            .push_u32(w.jpeg.steps)
            .push_str(" steps, ")
            .push_u32(w.frames)
            .push_str(" frames, ")
            .push_u32(w.jpeg.avg_us())
            .push_str(" us/step, max ")
            .push_u32(w.jpeg.max_us)
            .push_str(" us, ")
            .push_u32(w.last_frame_ms)
            .push_str(" ms/frame, ")
            .push_u32(fuel_per_s(jpeg_fuel, w.frames_us) / 1000)
            .push_str("k fuel/s, rx ")
            .push_u32(w.rx);
        if self.errors > 0 {
            line.push_str(", errors ").push_u32(self.errors);
        }
        line.log();
        self.window = Window::default();
    }
}

/// The large buffers, kept out of the payload state.
pub struct Buffers {
    /// The mix table.
    pub table: [u32; MIX_WORDS],
    /// The decoder's output strip.
    pub strip: [u8; STRIP_BYTES],
    /// The JPEG decoder.
    pub dec: Decoder,
}

impl Buffers {
    /// All zero bytes, so a static of it is `.bss`.
    pub const EMPTY: Buffers = Buffers {
        table: [0; MIX_WORDS],
        strip: [0; STRIP_BYTES],
        dec: Decoder::IDLE,
    };
}

// The exports need a single-threaded wasm32 module (where a `GuestCell`
// may be a static); host builds only run the unit tests below.
#[cfg(target_arch = "wasm32")]
mod exports {
    use super::{Bench, Buffers, LogLine, REPORT_MS, RX_KEY, STEP_GAP_US, STEP_TIMER};
    use mw_payload::irq::Event;
    use mw_payload::timer::Timer;
    use mw_payload::{AbiError, GuestCell, Payload, Slot};

    /// The large buffers (`.bss`).
    static BUFS: GuestCell<Buffers> = GuestCell::new(Buffers::EMPTY);

    /// The payload: the benchmark and its step timer.
    pub struct App {
        bench: Bench,
        timer: Option<Timer>,
    }

    fn log_err(what: &str, err: AbiError) {
        let mut line = LogLine::new();
        line.push_str("bench: ")
            .push_str(what)
            .push_str(" error ")
            .push_u32(err.code().unsigned_abs());
        line.log();
    }

    impl App {
        /// Arms the next step.
        fn arm(&mut self) {
            if let Some(t) = self.timer.take() {
                let _ = t.stop();
            }
            match Timer::start_once(STEP_TIMER, STEP_GAP_US) {
                Ok(t) => self.timer = Some(t),
                Err(e) => log_err("timer", e),
            }
        }
    }

    impl Payload for App {
        fn init() -> Self {
            if let Err(e) = mw_payload::set_tick(REPORT_MS) {
                log_err("set_tick", e);
            }
            if let Err(e) = mw_payload::subscribe(RX_KEY) {
                log_err("subscribe", e);
            }
            mw_payload::log("bench: started");
            let mut app = App { bench: Bench::new(), timer: None };
            app.arm();
            app
        }

        fn tick(&mut self, _now_ms: u32) {
            self.bench.report();
        }

        fn on_message(&mut self, _slot: Slot, _data: &[u8]) {
            self.bench.received();
        }

        fn on_irq(&mut self, event: Event) {
            if let Event::Timer { id: STEP_TIMER, .. } = event {
                if let Some(mut bufs) = BUFS.borrow_mut() {
                    self.bench.step(&mut bufs, mw_payload::now_ms(), mw_payload::irq::now_us);
                }
                self.arm();
            }
        }
    }

    mw_payload::payload!(App);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_is_deterministic_and_touches_the_table() {
        let mut a = Box::new([0u32; MIX_WORDS]);
        let mut b = Box::new([0u32; MIX_WORDS]);
        let x = mix(&mut a, 1, MIX_ROUNDS);
        assert_eq!(mix(&mut b, 1, MIX_ROUNDS), x);
        assert_eq!(a, b);
        assert!(a.iter().filter(|&&w| w != 0).count() > MIX_WORDS / 2);
    }

    #[test]
    fn steps_alternate_and_a_frame_takes_the_documented_steps() {
        let mut bench = Bench::new();
        let mut bufs = Box::new(Buffers::EMPTY);
        let mut t = 0u32;
        // Each clock read advances 10 us: every step measures 10 us.
        let mut clock = || {
            t += 10;
            t
        };
        for i in 0..2 * JPEG_STEPS_PER_FRAME {
            assert_eq!(bench.next, if i % 2 == 0 { Work::Mix } else { Work::Jpeg });
            bench.step(&mut bufs, i, &mut clock);
        }
        let w = bench.window;
        assert_eq!(bench.errors, 0);
        assert_eq!((w.mix.steps, w.jpeg.steps), (JPEG_STEPS_PER_FRAME, JPEG_STEPS_PER_FRAME));
        assert_eq!(w.frames, 1, "one frame in exactly JPEG_STEPS_PER_FRAME steps");
        assert_eq!(w.frames_us, 10 * u64::from(JPEG_STEPS_PER_FRAME));
        assert_eq!(w.last_frame_ms, 2 * (JPEG_STEPS_PER_FRAME - 1));
        assert_eq!(bench.frame, Frame::Start);
        assert_eq!((w.mix.avg_us(), w.mix.max_us), (10, 10));
    }

    #[test]
    fn jpeg_is_the_fake_camera_frame() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../fixtures/cam/jpeg240/frame_03.jpg");
        assert_eq!(JPEG, &std::fs::read(path).expect("fixture")[..]);
    }

    #[test]
    fn rates_and_reports() {
        assert_eq!(fuel_per_s(200_000, 1_000), 200_000_000);
        assert_eq!(fuel_per_s(1, 0), 0);
        assert_eq!(Timing::default().avg_us(), 0);
        let mut bench = Bench::new();
        bench.received();
        assert_eq!(bench.window.rx, 1);
        // Off wasm32 the log import is a stub; the window still resets.
        bench.report();
        assert_eq!(bench.window, Window::default());
    }
}
