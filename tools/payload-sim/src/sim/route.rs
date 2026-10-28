//! The simulator's ABI 4 DMA routes (requirement R10): a table of at most
//! [`MAX_ROUTES`] source and sink routes and the pure steps that run them.
//! [`super::SimDevice`] owns the table, publishes what a source produces,
//! sends what a sink forwards to its [`super::panel::Panel`], and turns the
//! outcomes into `on_irq` events and printed lines.
//!
//! * A [`SourceRoute`] (only [`wasm_host::SourceKind::TestPattern`]; the camera is
//!   `Unsupported`) is due every `period_ms` on the simulator clock, first
//!   one period after it starts. Its [`FrameSource`] comes from
//!   [`FrameSource::test_pattern`]: the RGB565 [`pattern`](middleware::stream::pattern) for an RGB565
//!   config, the baked JPEG frames (`stream::frames::fixtures`) for a JPEG
//!   config of 240×240 or 120×120 (other JPEG sizes are
//!   `InvalidArgument`). [`Routes::due_frames`] produces each due frame `n`
//!   (counting from 0, sequence number `n as u16`) as [`Chunker`](middleware::stream::Chunker) chunks
//!   of frame `n` ([`frame_chunks`]): generated pattern bytes, or stored
//!   frame `n mod 8` with its own length as each chunk's `total`. When
//!   the clock jumped over several periods, only the latest frame is
//!   produced and the earlier ones count as skipped
//!   ([`RouteStats::skipped`]), like a camera that overran.
//! * A [`SinkRoute`] owns an SPI bus and its D/C pin; every message on its
//!   key goes through a [`Sink`] ([`SinkRoute::receive`]) and its
//!   [`RouteStats`] record each step.

use middleware::stream::{FrameSource, HEADER_LEN, MAX_CHUNK_DATA, RouteStats, Sink, Step};
use middleware::wasm_abi::route::MAX_ROUTES;
use wasm_host::{AbiError, SinkConfig, SourceConfig};

/// A running test-pattern source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRoute {
    /// Key the chunks are published on.
    pub key: String,
    /// Frame geometry, period and chunk size.
    pub cfg: SourceConfig,
    /// Where the frame bytes come from.
    pub source: FrameSource<'static>,
    /// Number of the next frame.
    pub frame: u32,
    /// When the next frame is due, in microseconds.
    pub due_us: u64,
    /// Counters for `route_stat`.
    pub stats: RouteStats,
}

/// A running sink bound to an SPI bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkRoute {
    /// Key whose messages the route consumes.
    pub key: String,
    /// The SPI bus it owns.
    pub bus: u8,
    /// D/C pin, frame length and preamble.
    pub cfg: SinkConfig,
    /// Stream state.
    pub sink: Sink,
    /// Counters for `route_stat`.
    pub stats: RouteStats,
}

impl SinkRoute {
    /// Steps the sink with one message; returns the step and the data to
    /// send (empty unless the step forwards).
    pub fn receive<'m>(&mut self, msg: &'m [u8]) -> (Step, &'m [u8]) {
        let (step, data) = self.sink.receive(self.cfg.frame_len, msg);
        self.sink = step.sink;
        self.stats = self.stats.record(&step);
        (step, data)
    }
}

/// One route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Publishes frames.
    Source(SourceRoute),
    /// Draws frames.
    Sink(SinkRoute),
}

impl Route {
    fn stats(&self) -> RouteStats {
        match self {
            Route::Source(s) => s.stats,
            Route::Sink(s) => s.stats,
        }
    }
}

/// One frame a source produced: its chunk messages, ready to publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Route handle.
    pub handle: u8,
    /// Key to publish on.
    pub key: String,
    /// Frame number (the pattern's frame argument).
    pub number: u32,
    /// Frames skipped before this one because the clock jumped past them.
    pub skipped: u32,
    /// Encoded chunks (header and data), in order.
    pub chunks: Vec<Vec<u8>>,
}

/// The chunk messages of frame `number` of `source`, in chunks of at most
/// `chunk_data` bytes, with sequence number `number as u16`.
pub fn frame_chunks(
    source: &FrameSource<'_>,
    chunk_data: u16,
    number: u32,
) -> Result<Vec<Vec<u8>>, AbiError> {
    let chunker = source
        .chunker(number, number as u16, usize::from(chunk_data))
        .map_err(|_| AbiError::InvalidArgument)?;
    let mut data = [0u8; MAX_CHUNK_DATA];
    let mut msg = [0u8; HEADER_LEN + MAX_CHUNK_DATA];
    chunker
        .chunks()
        .map(|plan| {
            let body = data
                .get_mut(..plan.range.len())
                .ok_or(AbiError::InvalidArgument)?;
            if source.fill(number, plan.range.start, body) != body.len() {
                return Err(AbiError::InvalidArgument);
            }
            let n = plan
                .header
                .write(body, &mut msg)
                .map_err(|_| AbiError::InvalidArgument)?;
            Ok(msg.get(..n).ok_or(AbiError::InvalidArgument)?.to_vec())
        })
        .collect()
}

/// The route table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Routes {
    slots: [Option<Route>; MAX_ROUTES],
}

impl Routes {
    /// Puts `route` in the lowest free slot; `NoResources` when full.
    pub fn add(&mut self, route: Route) -> Result<u8, AbiError> {
        let (handle, slot) = self
            .slots
            .iter_mut()
            .enumerate()
            .find(|(_, s)| s.is_none())
            .ok_or(AbiError::NoResources)?;
        *slot = Some(route);
        u8::try_from(handle).map_err(|_| AbiError::NoResources)
    }

    /// Removes route `handle`; `Busy` if it is not running.
    pub fn stop(&mut self, handle: u8) -> Result<Route, AbiError> {
        self.slots
            .get_mut(usize::from(handle))
            .and_then(Option::take)
            .ok_or(AbiError::Busy)
    }

    /// The counters of route `handle`; `Busy` if it is not running.
    pub fn stats(&self, handle: u8) -> Result<RouteStats, AbiError> {
        self.slots
            .get(usize::from(handle))
            .and_then(Option::as_ref)
            .map(Route::stats)
            .ok_or(AbiError::Busy)
    }

    /// The running routes with their handles.
    pub fn iter(&self) -> impl Iterator<Item = (u8, &Route)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(h, r)| Some((h as u8, r.as_ref()?)))
    }

    /// The sink route consuming `key`, if any.
    pub fn sink_for(&mut self, key: &str) -> Option<(u8, &mut SinkRoute)> {
        self.slots
            .iter_mut()
            .enumerate()
            .find_map(|(h, r)| match r {
                Some(Route::Sink(s)) if s.key == key => Some((h as u8, s)),
                _ => None,
            })
    }

    /// Whether a sink route owns SPI `bus`.
    pub fn owns_bus(&self, bus: u8) -> bool {
        self.iter()
            .any(|(_, r)| matches!(r, Route::Sink(s) if s.bus == bus))
    }

    /// Whether a sink route owns `pin` as its D/C pin.
    pub fn owns_pin(&self, pin: u8) -> bool {
        self.iter()
            .any(|(_, r)| matches!(r, Route::Sink(s) if s.cfg.dc_pin == pin))
    }

    /// When the next source frame is due, in microseconds.
    pub fn next_due(&self) -> Option<u64> {
        self.iter()
            .filter_map(|(_, r)| match r {
                Route::Source(s) => Some(s.due_us),
                Route::Sink(_) => None,
            })
            .min()
    }

    /// Produces every source frame due at `now_us` (at most one per route)
    /// and advances those routes' schedules and counters.
    pub fn due_frames(&mut self, now_us: u64) -> Vec<Frame> {
        let mut frames = Vec::new();
        for (handle, slot) in self.slots.iter_mut().enumerate() {
            let Some(Route::Source(src)) = slot else {
                continue;
            };
            if src.due_us > now_us {
                continue;
            }
            let period = u64::from(src.cfg.period_ms).max(1) * 1000;
            let late = (now_us - src.due_us) / period;
            let skipped = u32::try_from(late).unwrap_or(u32::MAX);
            let number = src.frame.wrapping_add(skipped);
            let Ok(chunks) = frame_chunks(&src.source, src.cfg.chunk_data, number) else {
                continue;
            };
            let count = u32::try_from(chunks.len()).unwrap_or(u32::MAX);
            // Skipped frames were never captured: none of their chunks went out.
            let stats = RouteStats {
                frames_dropped: src.stats.frames_dropped.saturating_add(skipped),
                chunks_dropped: src
                    .stats
                    .chunks_dropped
                    .saturating_add(count.saturating_mul(skipped)),
                ..src.stats
            };
            src.stats = stats.sent(count);
            src.frame = number.wrapping_add(1);
            src.due_us += (late + 1) * period;
            frames.push(Frame {
                handle: handle as u8,
                key: src.key.clone(),
                number,
                skipped,
                chunks,
            });
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::stream::frames::fixtures;
    use middleware::stream::{Action, chunk, pattern};

    fn pattern_chunks(cfg: &SourceConfig, number: u32) -> Result<Vec<Vec<u8>>, AbiError> {
        frame_chunks(&FrameSource::test_pattern(cfg)?, cfg.chunk_data, number)
    }

    fn source(period_ms: u16) -> SourceRoute {
        let cfg = SourceConfig::new(4, 3, period_ms).unwrap();
        SourceRoute {
            key: "cam".into(),
            source: FrameSource::test_pattern(&cfg).unwrap(),
            cfg,
            frame: 0,
            due_us: u64::from(period_ms) * 1000,
            stats: RouteStats::default(),
        }
    }

    #[test]
    fn pattern_chunks_reassemble_the_pattern() {
        let cfg = SourceConfig::new(5, 4, 10)
            .unwrap()
            .with_chunk_data(7)
            .unwrap();
        let chunks = pattern_chunks(&cfg, 9).unwrap();
        assert_eq!(chunks.len(), 40usize.div_ceil(7));
        let mut frame = Vec::new();
        for c in &chunks {
            let c = chunk::decode(c).unwrap();
            assert_eq!(c.header.seq, 9);
            assert_eq!(c.header.offset as usize, frame.len());
            frame.extend_from_slice(c.data);
        }
        let mut want = vec![0; 40];
        pattern::fill(9, 5, 4, 0, &mut want);
        assert_eq!(frame, want);
    }

    /// A JPEG test-pattern source publishes the baked fixture frames in
    /// order, looping, each frame's chunks carrying its own length.
    #[test]
    fn jpeg_chunks_are_the_fixture_frames_in_order() {
        for size in [240, 120] {
            let cfg = SourceConfig::jpeg(size, size, 200).unwrap();
            let src = FrameSource::test_pattern(&cfg).unwrap();
            let set = fixtures::jpeg(size, size).unwrap();
            for number in 0..10u32 {
                let chunks = frame_chunks(&src, cfg.chunk_data, number).unwrap();
                let want = set.frame(number);
                assert_eq!(want, set.frame(number % 8));
                assert_eq!(chunks.len(), want.len().div_ceil(166));
                let mut frame = Vec::new();
                for c in &chunks {
                    let c = chunk::decode(c).unwrap();
                    assert_eq!(c.header.total as usize, want.len());
                    assert_eq!(c.header.seq, number as u16);
                    frame.extend_from_slice(c.data);
                }
                assert_eq!(frame, want);
            }
        }
        let odd = SourceConfig::jpeg(200, 200, 200).unwrap();
        assert_eq!(
            FrameSource::test_pattern(&odd),
            Err(AbiError::InvalidArgument)
        );
    }

    #[test]
    fn handles_fill_up_and_stop_frees_them() {
        let mut routes = Routes::default();
        for h in 0..MAX_ROUTES {
            assert_eq!(routes.add(Route::Source(source(10))), Ok(h as u8));
        }
        assert_eq!(
            routes.add(Route::Source(source(10))),
            Err(AbiError::NoResources)
        );
        assert!(routes.stop(2).is_ok());
        assert_eq!(routes.stop(2), Err(AbiError::Busy));
        assert_eq!(routes.stats(2), Err(AbiError::Busy));
        assert_eq!(routes.add(Route::Source(source(10))), Ok(2));
    }

    #[test]
    fn sources_follow_the_clock_and_count_skipped_frames() {
        let mut routes = Routes::default();
        routes.add(Route::Source(source(10))).unwrap();
        assert_eq!(routes.next_due(), Some(10_000));
        assert!(routes.due_frames(9_999).is_empty());
        let f = routes.due_frames(10_000);
        assert_eq!((f.len(), f[0].number, f[0].skipped), (1, 0, 0));
        assert_eq!(f[0].chunks.len(), 1, "24 bytes fit one chunk");
        assert_eq!(routes.next_due(), Some(20_000));
        // Jump past frames 1 and 2: frame 3 is produced, two are skipped.
        let f = routes.due_frames(45_000);
        assert_eq!((f[0].number, f[0].skipped), (3, 2));
        assert_eq!(routes.next_due(), Some(50_000));
        let s = routes.stats(0).unwrap();
        assert_eq!(
            (s.frames, s.frames_dropped, s.chunks, s.chunks_dropped),
            (2, 2, 2, 2)
        );
    }

    #[test]
    fn sink_route_records_steps() {
        let cfg = SourceConfig::new(4, 3, 10)
            .unwrap()
            .with_chunk_data(10)
            .unwrap();
        let mut sink = SinkRoute {
            key: "cam".into(),
            bus: 0,
            cfg: SinkConfig::new(13, cfg.frame_len()).unwrap(),
            sink: Sink::new(),
            stats: RouteStats::default(),
        };
        let chunks = pattern_chunks(&cfg, 0).unwrap();
        assert_eq!(chunks.len(), 3);
        let (step, data) = sink.receive(&chunks[0]);
        assert_eq!((step.action, data.len()), (Action::StartFrame, 10));
        let (step, data) = sink.receive(&chunks[2]);
        assert!(step.aborted && data.is_empty(), "gap abandons the frame");
        let s = sink.stats;
        assert_eq!(
            (s.frames, s.frames_dropped, s.chunks, s.chunks_dropped),
            (0, 1, 1, 1)
        );
        let mut routes = Routes::default();
        routes.add(Route::Sink(sink)).unwrap();
        assert!(routes.owns_bus(0) && !routes.owns_bus(1));
        assert!(routes.owns_pin(13) && !routes.owns_pin(12));
        assert!(routes.sink_for("cam").is_some() && routes.sink_for("other").is_none());
        assert_eq!(routes.next_due(), None);
    }
}
