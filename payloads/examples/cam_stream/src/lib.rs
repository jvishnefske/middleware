//! Camera stream (ESP32-CAM, ABI 4): publishes 120×120 RGB565 frames on
//! [`FRAME_KEY`] through a host *source route*, so the pixels never pass
//! through the payload.
//!
//! * `init` starts a [`SOURCE`] route on [`FRAME_KEY`] with
//!   [`source_config`]: [`WIDTH`]×[`HEIGHT`] RGB565 big-endian, one frame
//!   every [`PERIOD_MS`], chunks of [`ESPNOW_CHUNK_DATA`] (166) bytes so each
//!   one fits a single ESP-NOW message ([`CHUNKS_PER_FRAME`] = 174 per
//!   frame). It also sets a [`STATS_TICK_MS`] tick.
//! * `on_irq` folds the route's `RouteFrames` / `RouteDropped` events into
//!   [`Stats`] ([`Stats::apply`]). A `RouteStopped` event (the host gave up
//!   on the route after a fault) makes [`Stats::apply`] ask for a restart,
//!   which is attempted at once; if that fails it is retried on every tick.
//! * Every tick publishes [`Stats::encode`] (12 bytes) on [`STATS_KEY`] and
//!   logs one line.
//!
//! # Frame period and ESP-NOW bandwidth
//!
//! A frame is 28 800 bytes = 174 ESP-NOW messages. ESP-NOW at its default
//! 1 Mbit/s PHY rate sustains well under a thousand ~180-byte messages a
//! second, and the badge must also hear its other traffic, so
//! [`PERIOD_MS`] is 200 ms (5 frames/s, 870 messages/s, about 145 kB/s of
//! pixels). [`messages_per_second`] computes the load and a unit test keeps
//! it under [`ESPNOW_MSG_BUDGET`]; lower [`PERIOD_MS`] (to the ABI minimum
//! of 10 ms) on a faster link.
//!
//! # Seam for the real camera
//!
//! The route's data source is the single constant [`SOURCE`]. The firmware
//! offers [`SourceKind::TestPattern`] (a moving synthetic pattern) today;
//! once it drives the OV2640 behind [`SourceKind::Camera`], changing
//! [`SOURCE`] is the whole payload change: the config, key, chunking and
//! event handling are identical, because the camera fills the same
//! host-owned DMA frame buffers. Nothing else in the ABI is needed (there
//! are no camera-specific imports; exposure and similar settings are the
//! firmware's business).
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use mw_payload::irq::Event;
use mw_payload::route::{ESPNOW_CHUNK_DATA, Route, RouteEvent, SourceConfig, SourceKind};
use mw_payload::{AbiError, LogLine, Payload};

/// Key the frames are published on (as `middleware::stream` chunks).
pub const FRAME_KEY: &str = "app/cam/frame";
/// Key the [`Stats`] are published on.
pub const STATS_KEY: &str = "app/cam/stats";
/// The route's data source; see "Seam for the real camera" above.
pub const SOURCE: SourceKind = SourceKind::TestPattern;
/// Frame width in pixels.
pub const WIDTH: u16 = 120;
/// Frame height in pixels.
pub const HEIGHT: u16 = 120;
/// Frame period in milliseconds (5 frames/s; see the module docs).
pub const PERIOD_MS: u16 = 200;
/// Data bytes per chunk: the ESP-NOW default, one message per chunk.
pub const CHUNK_DATA: u16 = ESPNOW_CHUNK_DATA as u16;
/// Bytes per frame (RGB565).
pub const FRAME_LEN: u32 = WIDTH as u32 * HEIGHT as u32 * 2;
/// ESP-NOW messages per frame.
pub const CHUNKS_PER_FRAME: u32 = FRAME_LEN.div_ceil(CHUNK_DATA as u32);
/// ESP-NOW messages per second the stream may use.
pub const ESPNOW_MSG_BUDGET: u32 = 1_000;
/// Period of the stats tick.
pub const STATS_TICK_MS: u32 = 2_000;

/// The route config: [`WIDTH`]×[`HEIGHT`] RGB565 every [`PERIOD_MS`] in
/// [`CHUNK_DATA`]-byte chunks, validated by the ABI's rules.
pub fn source_config() -> Result<SourceConfig, AbiError> {
    SourceConfig::new(WIDTH, HEIGHT, PERIOD_MS)?.with_chunk_data(CHUNK_DATA)
}

/// ESP-NOW messages per second a source route with `cfg` sends.
pub fn messages_per_second(cfg: &SourceConfig) -> u32 {
    let chunks = cfg.frame_len().div_ceil(u32::from(cfg.chunk_data));
    (chunks * 1000).div_ceil(u32::from(cfg.period_ms))
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

/// Logs `"cam_stream: <what> error <code>"`.
fn log_err(what: &str, err: AbiError) {
    let mut line = LogLine::new();
    line.push_str("cam_stream: ")
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
pub struct CamStream {
    route: Option<Route>,
    stats: Stats,
}

impl Payload for CamStream {
    fn init() -> Self {
        if let Err(e) = mw_payload::set_tick(STATS_TICK_MS) {
            log_err("set_tick", e);
        }
        let route = start();
        mw_payload::log("cam_stream: streaming 120x120 RGB565 on app/cam/frame");
        CamStream {
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
        line.push_str("cam_stream: frames ")
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
mw_payload::payload!(CamStream);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_bytes_match_the_abi_example() {
        // design.md "DMA routes": 120×120 RGB565, 166-byte chunks.
        let cfg = source_config().expect("valid");
        assert_eq!(cfg.frame_len(), 28_800);
        assert_eq!(cfg.chunk_data, 166);
        assert_eq!(cfg.encode(), [120, 0, 120, 0, 0, 0, 200, 0, 166, 0]);
        assert_eq!(SourceConfig::parse(&cfg.encode()), Ok(cfg));
    }

    #[test]
    fn a_frame_is_174_espnow_messages_within_budget() {
        assert_eq!(CHUNKS_PER_FRAME, 174);
        let cfg = source_config().expect("valid");
        assert_eq!(messages_per_second(&cfg), 870);
        assert!(messages_per_second(&cfg) <= ESPNOW_MSG_BUDGET);
        // 100 ms would need 1740 messages/s: over budget.
        let fast = SourceConfig::new(120, 120, 100).expect("valid");
        assert!(messages_per_second(&fast) > ESPNOW_MSG_BUDGET);
    }

    #[test]
    fn source_is_the_test_pattern_until_the_camera_lands() {
        assert_eq!(SOURCE, SourceKind::TestPattern);
    }

    #[test]
    fn events_accumulate_and_a_stop_asks_for_a_restart() {
        let s = Stats::default();
        let (s, r) = s.apply(RouteEvent::Frames(3));
        assert_eq!(r, Reaction::None);
        let (s, r) = s.apply(RouteEvent::Dropped(2));
        assert_eq!(r, Reaction::None);
        let (s, _) = s.apply(RouteEvent::Frames(4));
        let (s, r) = s.apply(RouteEvent::Stopped(AbiError::Transport));
        assert_eq!(r, Reaction::Restart);
        assert_eq!(
            s,
            Stats {
                frames: 7,
                dropped: 2,
                stops: 1
            }
        );
    }

    #[test]
    fn counters_saturate() {
        let s = Stats {
            frames: u32::MAX - 1,
            dropped: u32::MAX,
            stops: u32::MAX,
        };
        let (s, _) = s.apply(RouteEvent::Frames(5));
        let (s, _) = s.apply(RouteEvent::Dropped(5));
        let (s, _) = s.apply(RouteEvent::Stopped(AbiError::Busy));
        assert_eq!((s.frames, s.dropped, s.stops), (u32::MAX, u32::MAX, u32::MAX));
    }

    #[test]
    fn stats_encoding_is_three_le_words() {
        let s = Stats {
            frames: 0x0102_0304,
            dropped: 5,
            stops: 0xFFFF_FFFF,
        };
        assert_eq!(
            s.encode(),
            [4, 3, 2, 1, 5, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF]
        );
    }

    #[test]
    fn host_stub_start_fails_cleanly() {
        // Off wasm32 the imports return Unsupported; start logs and yields
        // no route instead of panicking.
        assert!(start().is_none());
    }
}
