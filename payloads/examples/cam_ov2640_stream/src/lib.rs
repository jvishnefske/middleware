//! OV2640 camera stream (AI-Thinker ESP32-CAM, ABI 4): sets the board's
//! OV2640 up for 240×240 JPEG over SCCB, then publishes its frames on
//! [`FRAME_KEY`] through a host *camera route*, so the bytes never pass
//! through the payload. The badge's `cam_jpeg_view` reassembles and
//! decodes them. `cam_jpeg_stream` (the baked fixture frames through the
//! test-pattern source) is unchanged; run one or the other.
//!
//! # Wiring (the ESP32 board profile of esp-base)
//!
//! | signal | payload resource | ESP32-CAM pin |
//! |---|---|---|
//! | PWDN (active high) | GPIO output [`PWDN_PIN`] | GPIO 32 |
//! | XCLK [`XCLK_HZ`] | PWM timer [`XCLK_PWM`] channel [`XCLK_CHANNEL`] | GPIO 0 |
//! | SCCB SDA / SCL | I2C bus [`I2C_BUS`] at [`I2C_HZ`] | GPIO 26 / 27 |
//! | D0..D7, VSYNC, HREF, PCLK | the route's [`CameraPins::AI_THINKER`] | 5, 18, 19, 21, 36, 39, 34, 35; 25, 23, 22 |
//!
//! # What runs when
//!
//! 1. `init` drives PWDN low (sensor on), starts XCLK at 50 % duty and
//!    opens the I2C bus, sets the [`STATS_TICK_MS`] tick, and starts the
//!    [`bringup::Bringup`] sequencer. If any of the three cannot be opened
//!    it logs that and does nothing more.
//! 2. The sequencer (pure; see [`bringup`] for the order and the delays)
//!    runs one step per one-shot timer [`SEQ_TIMER`] alarm: read and log
//!    the sensor ID (`cam_ov2640_stream: sensor PID 0x26 VER 0x42`), soft
//!    reset, then the [`ov2640::TABLE`] register writes at most
//!    [`bringup::WRITES_PER_STEP`] per call. A wrong ID or an SCCB error
//!    restarts the bring-up after a back-off; after
//!    [`bringup::MAX_FAILURES`] in a row it logs that it gives up.
//! 3. Then it starts [`Route::camera`] on [`FRAME_KEY`] with
//!    [`source_config`] and [`CameraPins::AI_THINKER`]. A refused start is
//!    retried on every tick; a `RouteStopped` event restarts the route at
//!    once and, if that fails, on every tick (the sensor keeps its
//!    registers).
//! 4. `on_irq` folds `RouteFrames` / `RouteDropped` / `RouteStopped` into
//!    [`Stats`]; every tick publishes [`Stats::encode`] (12 bytes:
//!    frames, dropped, stops as `u32` LE) on [`STATS_KEY`] and logs them.
//!
//! An SCCB register read is two transfers, the register address written
//! (`tx = [reg]`), then one byte read (`rx = [_]`): SCCB has no repeated
//! start. A write is one transfer, `tx = [reg, value]`.
//!
//! # Frame period and ESP-NOW budget
//!
//! On the ESP32-CAM 240×240 frames measured 2–5 KB, but some scenes went
//! over 8 KB at quality scale 12, hence [`ov2640::QUALITY`] = 15; the
//! badge's viewer drops frames over its 8 KiB slots ([`VIEWER_FRAME_CAP`]).
//! An 8 KiB frame is 50 chunks of 166 bytes ([`CHUNK_DATA`], one
//! ESP-NOW message each). At [`PERIOD_MS`] = 500 ms (2 frames a second)
//! that is 100 messages a second, a tenth of the [`ESPNOW_MSG_BUDGET`] of
//! 1 000 that `cam_jpeg_stream` uses (ESP-NOW at its 1 Mbit/s default
//! sustains well under a thousand ~180-byte messages a second, and the
//! badge must hear its other traffic). The badge decodes one frame in
//! about 3 s, so faster frames would only cost air time.
//! [`messages_per_second`] computes the load; a unit test keeps an 8 KiB
//! frame within budget.
//!
//! # Work per call (fuel)
//!
//! A call may use 1 000 000 fuel on the ESP32. The heaviest calls are the
//! table slices of [`bringup::WRITES_PER_STEP`] = 32 SCCB writes. Measured
//! in `payload-sim` (`tools/payload-sim/tests/ov2640.rs`, the deploy build
//! at 1 000 000 fuel per call): at most 5 917 fuel in one call, about
//! 60 000 for the whole bring-up (14 calls including `init` and two ticks).
//!
//! # Memory
//!
//! No buffers: the frames stay in the host, and the state is a few words.
//! **Deploy the byte-page build**
//! (`payloads/scripts/build-byte-pages.sh -p cam_ov2640_stream`, output in
//! `payloads/target/byte-pages/wasm32-unknown-unknown/release/`): linear
//! memory fixed at MEMORY = 6 144 bytes (initial = maximum, 1-byte pages)
//! with STACK = 4 096, from `[package.metadata.byte-pages]` in
//! `Cargo.toml`: stack + data + `.bss` (1 696 bytes) = 5 792
//! (`__heap_base`), rounded up to 1 KiB. The stack is four times what a
//! `payload-sim` run needs (an overflow traps with 512 bytes; 1 024 run
//! clean), leaving room for the error paths the simulator does not take.
//! The ESP32's heap bound then charges 6 144 bytes instead of a 64 KiB
//! page. The image is about 7.8 KB, far under the ESP32's 24 KiB limit
//! (`payloads/abi-check/tests/payloads.rs` checks both).
//!
//! # Simulator
//!
//! `payload-sim` models the OV2640's SCCB registers on I2C bus 0 at
//! [`ov2640::SCCB_ADDR`] (ID, banks, soft reset) but has no image sensor,
//! so the camera route is `Unsupported` there: the bring-up completes and
//! the route start is retried on every tick.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod bringup;
pub mod ov2640;

use bringup::{Action, Bringup, Failure, Outcome, Phase, Sccb, Wait};
use mw_payload::gpio::Output;
use mw_payload::i2c::I2c;
use mw_payload::irq::Event;
use mw_payload::pwm::Pwm;
use mw_payload::route::{
    CAMERA_CFG_LEN, CameraPins, ESPNOW_CHUNK_DATA, Route, RouteEvent, Source, SourceConfig,
};
use mw_payload::timer::Timer;
use mw_payload::{AbiError, LogLine, Payload};

/// Key the frames are published on (as `middleware::stream` chunks).
pub const FRAME_KEY: &str = "app/cam/jpeg";
/// Key the [`Stats`] are published on.
pub const STATS_KEY: &str = "app/cam/stats";
/// Frame width in pixels (the sensor's output size, [`ov2640::WINDOW`]).
pub const WIDTH: u16 = ov2640::WINDOW.out_width;
/// Frame height in pixels.
pub const HEIGHT: u16 = ov2640::WINDOW.out_height;
/// Shortest interval between published frames (2 frames/s; see the
/// module docs).
pub const PERIOD_MS: u16 = 500;
/// Data bytes per chunk: the ESP-NOW default, one message per chunk.
pub const CHUNK_DATA: u16 = ESPNOW_CHUNK_DATA as u16;
/// ESP-NOW messages per second the stream may use.
pub const ESPNOW_MSG_BUDGET: u32 = 1_000;
/// The badge viewer's frame slot: larger frames are dropped there.
pub const VIEWER_FRAME_CAP: u32 = 8 * 1024;
/// The most chunks a frame may have for the stream to stay within
/// [`ESPNOW_MSG_BUDGET`] at [`PERIOD_MS`].
pub const MAX_CHUNKS_IN_BUDGET: u32 = ESPNOW_MSG_BUDGET * PERIOD_MS as u32 / 1000;
/// Period of the stats tick (also the route retry period).
pub const STATS_TICK_MS: u32 = 2_000;
/// GPIO driving the sensor's PWDN (high = powered down).
pub const PWDN_PIN: u8 = 32;
/// PWM timer generating XCLK.
pub const XCLK_PWM: u8 = 0;
/// Its channel (GPIO 0 on the ESP32 board profile).
pub const XCLK_CHANNEL: u8 = 0;
/// XCLK frequency.
pub const XCLK_HZ: u32 = 20_000_000;
/// XCLK duty: 50 % of `u16::MAX` full scale.
pub const XCLK_DUTY: u16 = 0x8000;
/// I2C bus of the SCCB port.
pub const I2C_BUS: u8 = 0;
/// SCCB clock.
pub const I2C_HZ: u32 = 100_000;
/// The hardware timer used, as a one-shot, for every bring-up delay.
pub const SEQ_TIMER: u8 = 0;

/// The route config: [`WIDTH`]×[`HEIGHT`] JPEG at most every
/// [`PERIOD_MS`], in [`CHUNK_DATA`]-byte chunks.
pub fn source_config() -> Result<SourceConfig, AbiError> {
    SourceConfig::jpeg(WIDTH, HEIGHT, PERIOD_MS)?.with_chunk_data(CHUNK_DATA)
}

/// The [`CAMERA_CFG_LEN`] bytes `route_source` receives for the camera:
/// [`source_config`] and [`CameraPins::AI_THINKER`].
pub fn camera_cfg_bytes() -> Result<[u8; CAMERA_CFG_LEN], AbiError> {
    let source = Source::Camera(CameraPins::AI_THINKER.validated()?);
    let (bytes, len) = source.encode(&source_config()?);
    if len == CAMERA_CFG_LEN {
        Ok(bytes)
    } else {
        Err(AbiError::InvalidArgument)
    }
}

/// Chunks of a frame of `len` bytes with `cfg`'s chunk size.
pub fn chunks(cfg: &SourceConfig, len: u32) -> u32 {
    len.div_ceil(u32::from(cfg.chunk_data.max(1)))
}

/// ESP-NOW messages per second a source route with `cfg` sends when every
/// frame is `chunks` chunks.
pub fn messages_per_second(cfg: &SourceConfig, chunks: u32) -> u32 {
    chunks
        .saturating_mul(1000)
        .div_ceil(u32::from(cfg.period_ms.max(1)))
}

/// What the shell should do after a route event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reaction {
    /// Nothing.
    None,
    /// The route stopped by itself: start it again.
    Restart,
}

/// Counters since `init`, all saturating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    /// Frames published.
    pub frames: u32,
    /// Frames the camera skipped.
    pub dropped: u32,
    /// Times the route stopped by itself.
    pub stops: u32,
}

impl Stats {
    /// Folds one route event in and says how to react.
    pub fn apply(self, event: RouteEvent) -> (Self, Reaction) {
        match event {
            RouteEvent::Frames(n) => (
                Stats {
                    frames: self.frames.saturating_add(n),
                    ..self
                },
                Reaction::None,
            ),
            RouteEvent::Dropped(n) => (
                Stats {
                    dropped: self.dropped.saturating_add(n),
                    ..self
                },
                Reaction::None,
            ),
            RouteEvent::Stopped(_) => (
                Stats {
                    stops: self.stops.saturating_add(1),
                    ..self
                },
                Reaction::Restart,
            ),
        }
    }

    /// The [`STATS_KEY`] payload: `frames`, `dropped`, `stops` as `u32`
    /// little-endian.
    pub fn encode(&self) -> [u8; 12] {
        let mut out = [0u8; 12];
        for (dst, n) in out
            .chunks_exact_mut(4)
            .zip([self.frames, self.dropped, self.stops])
        {
            for (d, s) in dst.iter_mut().zip(n.to_le_bytes()) {
                *d = s;
            }
        }
        out
    }
}

/// `"0x"` and two hex digits of `b`.
pub fn hex(b: u8) -> [u8; 4] {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    [
        b'0',
        b'x',
        DIGITS[usize::from(b >> 4)],
        DIGITS[usize::from(b & 0xF)],
    ]
}

/// Logs `"cam_ov2640_stream: <what> error <code>"`.
fn log_err(what: &str, err: AbiError) {
    let mut line = LogLine::new();
    line.push_str("cam_ov2640_stream: ")
        .push_str(what)
        .push_str(" error ")
        .push_u32(err.code().unsigned_abs());
    line.log();
}

/// Logs `"cam_ov2640_stream: sensor PID 0x26 VER 0x42"`, with
/// `" (not an OV2640)"` appended for another part.
fn log_id(pid: u8, ver: u8) {
    let mut line = LogLine::new();
    line.push_str("cam_ov2640_stream: sensor PID ")
        .push_bytes_lossy(&hex(pid))
        .push_str(" VER ")
        .push_bytes_lossy(&hex(ver));
    if !ov2640::is_ov2640(pid, ver) {
        line.push_str(" (not an OV2640)");
    }
    line.log();
}

/// The SCCB port over the I2C import.
struct SensorBus {
    i2c: I2c,
}

impl Sccb for SensorBus {
    fn write(&mut self, reg: u8, value: u8) -> Result<(), AbiError> {
        self.i2c.transfer(ov2640::SCCB_ADDR, &[reg, value], &mut [])
    }

    fn read(&mut self, reg: u8) -> Result<u8, AbiError> {
        let mut b = [0u8; 1];
        self.i2c.transfer(ov2640::SCCB_ADDR, &[reg], &mut [])?;
        self.i2c.transfer(ov2640::SCCB_ADDR, &[], &mut b)?;
        Ok(b[0])
    }
}

/// The sensor's power, clock and bus, held for the payload's lifetime.
struct Hw {
    _pwdn: Output,
    _xclk: Pwm,
    bus: SensorBus,
}

/// The resource a failed [`power_up`] could not set up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Pwdn,
    Xclk,
    Bus,
}

impl Part {
    /// Its name in the log (one literal per arm, rather than literals in
    /// closures, whose order in the image varied between builds).
    fn name(self) -> &'static str {
        match self {
            Part::Pwdn => "pwdn",
            Part::Xclk => "xclk",
            Part::Bus => "i2c",
        }
    }
}

/// Powers the sensor (PWDN low), starts XCLK and opens the SCCB bus; on
/// failure, which resource failed and how.
fn power_up() -> Result<Hw, (Part, AbiError)> {
    let mut pwdn = Output::new(PWDN_PIN).map_err(|e| (Part::Pwdn, e))?;
    pwdn.set(false).map_err(|e| (Part::Pwdn, e))?;
    let xclk = Pwm::open(XCLK_PWM, XCLK_HZ).map_err(|e| (Part::Xclk, e))?;
    xclk.channel(XCLK_CHANNEL)
        .and_then(|mut ch| ch.set_duty(XCLK_DUTY))
        .map_err(|e| (Part::Xclk, e))?;
    let i2c = I2c::open(I2C_BUS, I2C_HZ).map_err(|e| (Part::Bus, e))?;
    Ok(Hw {
        _pwdn: pwdn,
        _xclk: xclk,
        bus: SensorBus { i2c },
    })
}

/// Starts the camera route.
fn start_route() -> Result<Route, AbiError> {
    Route::camera(FRAME_KEY, &source_config()?, CameraPins::AI_THINKER)
}

/// Application state.
pub struct CamOv2640Stream {
    hw: Option<Hw>,
    bringup: Bringup,
    /// When the bring-up's next action is due.
    wait: Wait,
    route: Option<Route>,
    stats: Stats,
}

impl CamOv2640Stream {
    /// Arms [`SEQ_TIMER`] for `ms`; if that fails, the next tick runs the
    /// step instead.
    fn arm(&mut self, ms: u32) {
        let us = ms.saturating_mul(1000);
        if let Err(e) = Timer::start_once(SEQ_TIMER, us) {
            log_err("timer_start", e);
            self.wait = Wait::Tick;
        }
    }

    /// Runs the bring-up's current action, advances it and schedules the
    /// next one.
    fn run_step(&mut self) {
        let action = self.bringup.action();
        let outcome = match action {
            Action::Nothing => return,
            Action::StartRoute => match start_route() {
                Ok(route) => {
                    self.route = Some(route);
                    mw_payload::log("cam_ov2640_stream: streaming 240x240 JPEG on app/cam/jpeg");
                    Outcome::Started
                }
                Err(e) => {
                    log_err("route_camera", e);
                    Outcome::Failed(Failure::Route(e))
                }
            },
            Action::Identify | Action::Write(_) => {
                let Some(outcome) = self
                    .hw
                    .as_mut()
                    .and_then(|hw| bringup::run(action, &mut hw.bus))
                else {
                    return;
                };
                match outcome {
                    Outcome::Id { pid, ver } => log_id(pid, ver),
                    Outcome::Failed(Failure::Bus(e)) => log_err("sccb", e),
                    _ => {}
                }
                outcome
            }
        };
        let step = self.bringup.advance(outcome);
        self.bringup = step.state;
        self.wait = step.wait;
        if self.bringup.phase() == Phase::GaveUp {
            let mut line = LogLine::new();
            line.push_str("cam_ov2640_stream: giving up after ")
                .push_u32(u32::from(self.bringup.failures()))
                .push_str(" failed bring-ups");
            line.log();
        }
        if let Wait::Ms(ms) = step.wait {
            self.arm(ms);
        }
    }
}

impl Payload for CamOv2640Stream {
    fn init() -> Self {
        if let Err(e) = mw_payload::set_tick(STATS_TICK_MS) {
            log_err("set_tick", e);
        }
        let mut app = CamOv2640Stream {
            hw: None,
            bringup: Bringup::gave_up(),
            wait: Wait::Idle,
            route: None,
            stats: Stats::default(),
        };
        match power_up() {
            Ok(hw) => {
                mw_payload::log("cam_ov2640_stream: OV2640 powered, XCLK 20 MHz, SCCB 100 kHz");
                let step = Bringup::start();
                app.hw = Some(hw);
                app.bringup = step.state;
                app.wait = step.wait;
                if let Wait::Ms(ms) = step.wait {
                    app.arm(ms);
                }
            }
            Err((part, e)) => log_err(part.name(), e),
        }
        app
    }

    fn tick(&mut self, _now_ms: u32) {
        if self.wait == Wait::Tick {
            self.wait = Wait::Idle;
            self.run_step();
        }
        if let Err(e) = mw_payload::publish(STATS_KEY, &self.stats.encode()) {
            log_err("publish", e);
        }
        let mut line = LogLine::new();
        line.push_str("cam_ov2640_stream: frames ")
            .push_u32(self.stats.frames)
            .push_str(" dropped ")
            .push_u32(self.stats.dropped)
            .push_str(" stops ")
            .push_u32(self.stats.stops);
        line.log();
    }

    fn on_irq(&mut self, event: Event) {
        if let Event::Timer { id: SEQ_TIMER, .. } = event {
            if matches!(self.wait, Wait::Ms(_)) {
                self.wait = Wait::Idle;
                self.run_step();
            }
            return;
        }
        let Some(ev) = self.route.as_ref().and_then(|r| r.on_event(&event)) else {
            return;
        };
        let (stats, reaction) = self.stats.apply(ev);
        self.stats = stats;
        if reaction == Reaction::Restart {
            if let RouteEvent::Stopped(e) = ev {
                log_err("route stopped", e);
            }
            // The stopped route's handle is already free; drop it.
            self.route = None;
            self.bringup = self.bringup.route_stopped();
            self.run_step();
        }
    }
}

// The exports need a single-threaded wasm32 module; host builds only run
// the unit tests below.
#[cfg(target_arch = "wasm32")]
mw_payload::payload!(CamOv2640Stream);

#[cfg(test)]
mod tests {
    use super::*;
    use mw_payload::route::PixelFormat;

    #[test]
    fn config_is_a_240_jpeg_at_500_ms_in_espnow_chunks() {
        let cfg = source_config().expect("valid");
        assert_eq!(cfg.format, PixelFormat::Jpeg);
        assert_eq!(cfg.fixed_frame_len(), None, "JPEG frames vary in length");
        assert_eq!(cfg.chunk_data, CHUNK_DATA);
        assert_eq!(cfg.encode(), [240, 0, 240, 0, 1, 0, 0xF4, 0x01, 166, 0]);
        assert_eq!((WIDTH, HEIGHT), (240, 240));
    }

    #[test]
    fn camera_config_is_the_21_bytes_with_the_ai_thinker_pins() {
        let bytes = camera_cfg_bytes().expect("valid");
        assert_eq!(CAMERA_CFG_LEN, 21);
        assert_eq!(
            bytes,
            [
                240, 0, 240, 0, 1, 0, 0xF4, 0x01, 166, 0, // SourceConfig
                5, 18, 19, 21, 36, 39, 34, 35, // D0..D7
                25, 23, 22, // VSYNC, HREF, PCLK
            ]
        );
        let parsed = middleware_parse(&bytes);
        assert_eq!(
            parsed,
            Ok((
                Source::Camera(CameraPins::AI_THINKER),
                source_config().unwrap()
            ))
        );
    }

    /// The ABI's own parser, as the host runs it.
    fn middleware_parse(bytes: &[u8]) -> Result<(Source, SourceConfig), AbiError> {
        middleware::wasm_abi::route::parse_source(mw_payload::route::SourceKind::Camera, bytes)
    }

    #[test]
    fn an_8k_frame_at_500_ms_stays_within_the_espnow_budget() {
        let cfg = source_config().expect("valid");
        let n = chunks(&cfg, VIEWER_FRAME_CAP);
        assert_eq!(n, 50);
        assert_eq!(messages_per_second(&cfg, n), 100);
        assert!(messages_per_second(&cfg, n) <= ESPNOW_MSG_BUDGET);
        // A frame may grow to MAX_CHUNKS_IN_BUDGET chunks (83 KB, ten
        // times a viewer slot) before the stream leaves the budget.
        assert_eq!(MAX_CHUNKS_IN_BUDGET, 500);
        assert!(messages_per_second(&cfg, MAX_CHUNKS_IN_BUDGET) <= ESPNOW_MSG_BUDGET);
        assert!(messages_per_second(&cfg, MAX_CHUNKS_IN_BUDGET + 1) > ESPNOW_MSG_BUDGET);
    }

    #[test]
    fn events_accumulate_and_a_stop_asks_for_a_restart() {
        let (s, r) = Stats::default().apply(RouteEvent::Frames(3));
        assert_eq!(r, Reaction::None);
        let (s, r) = s.apply(RouteEvent::Dropped(2));
        assert_eq!(r, Reaction::None);
        let (s, r) = s.apply(RouteEvent::Stopped(AbiError::Transport));
        assert_eq!(r, Reaction::Restart);
        assert_eq!(
            s,
            Stats {
                frames: 3,
                dropped: 2,
                stops: 1
            }
        );
        let full = Stats {
            frames: u32::MAX,
            dropped: u32::MAX,
            stops: u32::MAX,
        };
        assert_eq!(full.apply(RouteEvent::Frames(1)).0, full);
        assert_eq!(full.apply(RouteEvent::Stopped(AbiError::Busy)).0, full);
    }

    #[test]
    fn stats_encoding_is_three_le_words() {
        let s = Stats {
            frames: 0x0102_0304,
            dropped: 5,
            stops: 0xFFFF_FFFF,
        };
        assert_eq!(s.encode(), [4, 3, 2, 1, 5, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn hex_digits() {
        assert_eq!(&hex(0x26), b"0x26");
        assert_eq!(&hex(0x42), b"0x42");
        assert_eq!(&hex(0xAF), b"0xaf");
        assert_eq!(&hex(0), b"0x00");
    }

    #[test]
    fn host_stub_init_fails_cleanly() {
        // Without a host every import is Unsupported: init logs the
        // power-up failure and the payload stays idle, without panicking.
        let mut app = CamOv2640Stream::init();
        assert!(app.hw.is_none());
        assert_eq!(app.bringup.phase(), Phase::GaveUp);
        assert_eq!(app.wait, Wait::Idle);
        app.tick(2_000);
        app.on_irq(Event::Timer {
            id: SEQ_TIMER,
            periods: 1,
            time_us: 0,
        });
        assert_eq!(app.bringup.phase(), Phase::GaveUp);
        assert!(app.route.is_none());
    }

    #[test]
    fn host_stub_route_start_fails_cleanly() {
        assert_eq!(start_route(), Err(AbiError::Unsupported));
    }
}
