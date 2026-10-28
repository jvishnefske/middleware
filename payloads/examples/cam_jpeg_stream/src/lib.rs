//! JPEG camera stream (ESP32-CAM, ABI 4, requirement R14): publishes
//! 240×240 baseline JPEG frames on [`FRAME_KEY`] through a host *source
//! route*, so the bytes never pass through the payload. The badge's
//! `cam_jpeg_view` reassembles and decodes them in WASM.
//!
//! * `init` starts a [`SOURCE`] route on [`FRAME_KEY`] with
//!   [`source_config`]: [`WIDTH`]×[`HEIGHT`] JPEG ([`SourceConfig::jpeg`]),
//!   one frame every [`PERIOD_MS`], chunks of [`ESPNOW_CHUNK_DATA`] (166)
//!   bytes so each one fits a single ESP-NOW message. Until the OV2640
//!   driver lands, the firmware's `TestPattern` source serves the baked
//!   fixture frames (`fixtures/cam/jpeg240`, feature `cam-fixtures`) for a
//!   JPEG config. It also sets a [`STATS_TICK_MS`] tick.
//! * `on_irq` folds the route's `RouteFrames` / `RouteDropped` events into
//!   [`Stats`] ([`Stats::apply`]). A `RouteStopped` event makes
//!   [`Stats::apply`] ask for a restart, attempted at once and, if that
//!   fails, on every tick.
//! * Every tick publishes [`Stats::encode`] (12 bytes: frames, dropped,
//!   stops as `u32` LE) on [`STATS_KEY`] and logs one line.
//!
//! `cam_stream` (120×120 RGB565 for the sink-route viewer) is unchanged;
//! run one or the other.
//!
//! # Frame period and ESP-NOW budget
//!
//! A JPEG frame varies in length. The baked 240×240 quality-60 frames are
//! 4 068–4 728 bytes: 25–29 chunks of 166 bytes, [`FIXTURE_MAX_CHUNKS`] at
//! most. The stream gets the same [`ESPNOW_MSG_BUDGET`] of 1 000 messages a
//! second as `cam_stream` (ESP-NOW at its 1 Mbit/s default PHY rate
//! sustains well under a thousand ~180-byte messages a second, and the
//! badge must hear its other traffic). At [`PERIOD_MS`] = 100 ms (10 frames
//! a second) the fixtures need at most 290 messages a second, under a
//! third of the budget, and a frame may grow to [`MAX_CHUNKS_IN_BUDGET`]
//! chunks (16.6 KB, room for a real OV2640 frame) before the stream leaves
//! it. [`messages_per_second`] computes the load and a unit test keeps the
//! fixtures within budget.
//!
//! The badge decodes far fewer frames than that (each 240×240 frame is
//! about 13 M fuel of wasmi work); it skips to the newest complete frame,
//! so the extra network frames only cost air time, and a lost chunk costs
//! one frame instead of the stream stalling.
//!
//! # Seam for the real camera
//!
//! As in `cam_stream`, the data source is the single constant [`SOURCE`]:
//! once the firmware drives the OV2640 behind [`SourceKind::Camera`]
//! (whose native output is JPEG), changing it is the whole payload change.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use mw_payload::irq::Event;
use mw_payload::route::{ESPNOW_CHUNK_DATA, Route, RouteEvent, SourceConfig, SourceKind};
use mw_payload::{AbiError, LogLine, Payload};

/// Key the frames are published on (as `middleware::stream` chunks).
pub const FRAME_KEY: &str = "app/cam/jpeg";
/// Key the [`Stats`] are published on.
pub const STATS_KEY: &str = "app/cam/stats";
/// The route's data source; see "Seam for the real camera" above.
pub const SOURCE: SourceKind = SourceKind::TestPattern;
/// Frame width in pixels.
pub const WIDTH: u16 = 240;
/// Frame height in pixels.
pub const HEIGHT: u16 = 240;
/// Frame period in milliseconds (10 frames/s; see the module docs).
pub const PERIOD_MS: u16 = 100;
/// Data bytes per chunk: the ESP-NOW default, one message per chunk.
pub const CHUNK_DATA: u16 = ESPNOW_CHUNK_DATA as u16;
/// ESP-NOW messages per second the stream may use.
pub const ESPNOW_MSG_BUDGET: u32 = 1_000;
/// Chunks of the largest baked 240×240 fixture frame (4 728 bytes).
pub const FIXTURE_MAX_CHUNKS: u32 = 29;
/// The most chunks a frame may have for the stream to stay within
/// [`ESPNOW_MSG_BUDGET`] at [`PERIOD_MS`].
pub const MAX_CHUNKS_IN_BUDGET: u32 = ESPNOW_MSG_BUDGET * PERIOD_MS as u32 / 1000;
/// Period of the stats tick.
pub const STATS_TICK_MS: u32 = 2_000;

/// The route config: [`WIDTH`]×[`HEIGHT`] JPEG every [`PERIOD_MS`] in
/// [`CHUNK_DATA`]-byte chunks, validated by the ABI's rules.
pub fn source_config() -> Result<SourceConfig, AbiError> {
    SourceConfig::jpeg(WIDTH, HEIGHT, PERIOD_MS)?.with_chunk_data(CHUNK_DATA)
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

/// What the shell should do after an event.
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
    /// Frames the source skipped.
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
            dst.copy_from_slice(&n.to_le_bytes());
        }
        out
    }
}

/// Logs `"cam_jpeg_stream: <what> error <code>"`.
fn log_err(what: &str, err: AbiError) {
    let mut line = LogLine::new();
    line.push_str("cam_jpeg_stream: ")
        .push_str(what)
        .push_str(" error ")
        .push_u32(err.code().unsigned_abs());
    line.log();
}

/// Starts the route, logging a failure.
fn start() -> Option<Route> {
    source_config()
        .and_then(|cfg| Route::source(SOURCE, FRAME_KEY, &cfg))
        .map_err(|e| log_err("route_source", e))
        .ok()
}

/// Application state: the route while it runs, and the counters.
pub struct CamJpegStream {
    route: Option<Route>,
    stats: Stats,
}

impl Payload for CamJpegStream {
    fn init() -> Self {
        if let Err(e) = mw_payload::set_tick(STATS_TICK_MS) {
            log_err("set_tick", e);
        }
        let route = start();
        mw_payload::log("cam_jpeg_stream: streaming 240x240 JPEG on app/cam/jpeg");
        CamJpegStream {
            route,
            stats: Stats::default(),
        }
    }

    fn tick(&mut self, _now_ms: u32) {
        if self.route.is_none() {
            self.route = start();
        }
        if let Err(e) = mw_payload::publish(STATS_KEY, &self.stats.encode()) {
            log_err("publish", e);
        }
        let mut line = LogLine::new();
        line.push_str("cam_jpeg_stream: frames ")
            .push_u32(self.stats.frames)
            .push_str(" dropped ")
            .push_u32(self.stats.dropped)
            .push_str(" stops ")
            .push_u32(self.stats.stops);
        line.log();
    }

    fn on_irq(&mut self, event: Event) {
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
            self.route = start();
        }
    }
}

// The exports need a single-threaded wasm32 module; host builds only run
// the unit tests below.
#[cfg(target_arch = "wasm32")]
mw_payload::payload!(CamJpegStream);

#[cfg(test)]
mod tests {
    use super::*;
    use mw_payload::route::PixelFormat;

    /// Lengths of the baked 240×240 fixture frames.
    fn fixture_lens() -> Vec<u32> {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures/cam/jpeg240");
        (0..8)
            .map(|i| {
                let path = dir.join(format!("frame_{i:02}.jpg"));
                std::fs::metadata(&path).expect("fixture").len() as u32
            })
            .collect()
    }

    #[test]
    fn config_bytes_are_a_240_jpeg_at_100_ms() {
        let cfg = source_config().expect("valid");
        assert_eq!(cfg.format, PixelFormat::Jpeg);
        assert_eq!(cfg.fixed_frame_len(), None, "JPEG frames vary in length");
        assert_eq!(cfg.encode(), [240, 0, 240, 0, 1, 0, 100, 0, 166, 0]);
        assert_eq!(SourceConfig::parse(&cfg.encode()), Ok(cfg));
    }

    #[test]
    fn fixture_frames_are_25_to_29_chunks_within_budget() {
        let cfg = source_config().expect("valid");
        let counts: Vec<u32> = fixture_lens().iter().map(|&n| chunks(&cfg, n)).collect();
        assert_eq!(counts.iter().min(), Some(&25));
        assert_eq!(counts.iter().max(), Some(&FIXTURE_MAX_CHUNKS));
        assert_eq!(messages_per_second(&cfg, FIXTURE_MAX_CHUNKS), 290);
        assert!(messages_per_second(&cfg, FIXTURE_MAX_CHUNKS) <= ESPNOW_MSG_BUDGET);
        assert_eq!(MAX_CHUNKS_IN_BUDGET, 100);
        assert!(messages_per_second(&cfg, MAX_CHUNKS_IN_BUDGET) <= ESPNOW_MSG_BUDGET);
        assert!(messages_per_second(&cfg, MAX_CHUNKS_IN_BUDGET + 1) > ESPNOW_MSG_BUDGET);
    }

    #[test]
    fn source_is_the_test_pattern_until_the_camera_lands() {
        assert_eq!(SOURCE, SourceKind::TestPattern);
        assert_eq!(FRAME_KEY, "app/cam/jpeg");
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
    fn host_stub_start_fails_cleanly() {
        assert!(start().is_none());
    }
}
