//! Source routes: frame pacing and the ESP-NOW frame of one chunk.
//!
//! A [`SourceRoute`] is the state of one running source: where its bytes
//! come from (a [`FrameSource`]: the generated RGB565 pattern, or the baked
//! JPEG frames for a JPEG config), which frame is being sent, which chunk
//! is next, when the frame is due, and its [`RouteStats`]. The radio task asks [`SourceRoute::next`] what to do
//! and reports each broadcast with [`SourceRoute::published`] or
//! [`SourceRoute::failed`]; each returns the next state and a [`Report`].
//!
//! # Pacing
//!
//! * A frame's first chunk waits until the frame is due; the first frame is
//!   due when the route starts, each next one `period_ms` after the
//!   previous one was due.
//! * Within a frame, chunks go out back to back, but each broadcast is
//!   awaited until esp-radio's send callback (the frame has left the air),
//!   so at most one frame sits in the Wi-Fi driver's queue and the TX queue
//!   cannot overrun. The radio rate therefore bounds the frame rate.
//! * When a frame finishes later than the next frame's due time, the next
//!   frame starts at once; whole periods that already passed are skipped
//!   and counted as dropped frames (a capture overrun), so a slow link
//!   loses frames instead of falling ever further behind.
//! * A failed or timed-out broadcast abandons the rest of the frame
//!   (counted with [`RouteStats::skipped`]); the next frame is scheduled as
//!   above.
//!
//! The frame sequence number of the chunk header advances by one per frame
//! attempted, while the source's frame number advances by one per period
//! (so skipped periods show as a jump in the counter band, or in the JPEG
//! loop). Each frame gets its own [`Chunker`] from
//! [`FrameSource::chunker`], so a JPEG frame's chunks carry that frame's
//! length as `total`; the chunks of skipped periods are counted with the
//! length of the frame that follows them.
//!
//! # Camera routes (R15)
//!
//! A camera route ([`SourceRoute::camera`]) has no stored frame: when a
//! frame is due, [`SourceRoute::next`] answers [`Next::Capture`] and the
//! shell captures one JPEG into its frame buffer, then reports the length
//! (or the failure) with [`SourceRoute::captured`]. A captured frame gets
//! its chunker then (the length is every chunk header's `total`) and is
//! sent like any other, its bytes copied from the shell's frame buffer
//! ([`Feed::Camera`]); a failed capture drops the frame
//! (`RouteDropped`) and the next one is due a period later. The period is
//! thus the shortest interval between frames: capture time and air time
//! add to it only when they overrun it.
//!
//! # One chunk, in place
//!
//! [`write_frame`] fills one ESP-NOW frame buffer: the middleware ESP-NOW
//! header and key ([`middleware::espnow::Frame`]), the 12-byte chunk header
//! ([`ChunkHeader::encode`](middleware::stream::ChunkHeader::encode)), then
//! the chunk's bytes written directly into the frame by
//! [`FrameSource::fill`]: test-pattern pixels generated in place, or JPEG
//! bytes copied from the flash-resident `&'static [u8]` fixture. There is
//! no frame buffer.

use middleware::espnow::{Frame, FrameError, MAX_FRAME};
use middleware::stream::{ChunkError, ChunkPlan, Chunker, FrameSource, HEADER_LEN, RouteStats};
use middleware::wasm_abi::AbiError;
use middleware::wasm_abi::route::SourceConfig;
use middleware::Key;

use super::{AIR_CHUNK_DATA, Report};
use crate::dispatch::tick_delay;

/// Where a source route's frame bytes come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feed {
    /// Generated or stored frames: the test pattern or the JPEG fixtures.
    Stored(FrameSource<'static>),
    /// The camera: the shell's frame buffer holds the current frame's
    /// `len` bytes (0 while none is captured).
    Camera {
        /// Bytes of the captured frame.
        len: u32,
    },
}

/// Why a camera capture produced no frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureFault {
    /// The frame did not fit the frame buffer.
    Overflow,
    /// No complete frame arrived in time (or the DMA ring overran).
    Timeout,
    /// The camera hardware is missing or failed.
    Hardware,
}

/// What a source route does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// Capture a camera frame now, then report it with
    /// [`SourceRoute::captured`].
    Capture,
    /// Publish this chunk now.
    Send(ChunkPlan),
    /// Nothing until this many milliseconds have passed.
    Wait(u32),
}

/// One running source route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceRoute {
    cfg: SourceConfig,
    feed: Feed,
    /// Chunker of the current frame.
    chunker: Chunker,
    /// Source frame number (one per period).
    frame: u32,
    /// Index of the next chunk of the current frame.
    chunk: u32,
    /// When the current frame is due (ms, wrapping clock).
    due: u32,
    stats: RouteStats,
}

impl SourceRoute {
    /// A `TestPattern` route for `cfg` whose first frame is due at
    /// `now_ms`, serving [`FrameSource::test_pattern`]: the pattern for
    /// RGB565, the 240×240 or 120×120 fixture frames for JPEG
    /// (`InvalidArgument` for another JPEG size, `Unsupported` without the
    /// `cam-fixtures` feature). `Unsupported` too when the chunk size does
    /// not fit one ESP-NOW message ([`AIR_CHUNK_DATA`]).
    pub fn new(cfg: SourceConfig, now_ms: u32) -> Result<Self, AbiError> {
        Self::with_source(cfg, FrameSource::test_pattern(&cfg)?, now_ms)
    }

    /// A route for `cfg` serving `source`, first frame due at `now_ms`.
    /// `Unsupported` when the chunk size does not fit one ESP-NOW message.
    pub fn with_source(cfg: SourceConfig, source: FrameSource<'static>, now_ms: u32) -> Result<Self, AbiError> {
        let data = usize::from(cfg.chunk_data);
        if data > AIR_CHUNK_DATA {
            return Err(AbiError::Unsupported);
        }
        let chunker = source.chunker(0, 0, data).map_err(|_| AbiError::InvalidArgument)?;
        Ok(SourceRoute {
            cfg,
            feed: Feed::Stored(source),
            chunker,
            frame: 0,
            chunk: 0,
            due: now_ms,
            stats: RouteStats::default(),
        })
    }

    /// A camera route for `cfg` (JPEG), first frame due at `now_ms`.
    /// `InvalidArgument` for a raw format, `Unsupported` when the chunk
    /// size does not fit one ESP-NOW message.
    pub fn camera(cfg: SourceConfig, now_ms: u32) -> Result<Self, AbiError> {
        if cfg.fixed_frame_len().is_some() {
            return Err(AbiError::InvalidArgument);
        }
        let data = usize::from(cfg.chunk_data);
        if data > AIR_CHUNK_DATA {
            return Err(AbiError::Unsupported);
        }
        Ok(SourceRoute {
            cfg,
            feed: Feed::Camera { len: 0 },
            chunker: placeholder(0, data).ok_or(AbiError::InvalidArgument)?,
            frame: 0,
            chunk: 0,
            due: now_ms,
            stats: RouteStats::default(),
        })
    }

    /// The route's config.
    pub const fn config(&self) -> SourceConfig {
        self.cfg
    }

    /// Where the route's frame bytes come from.
    pub const fn feed(&self) -> Feed {
        self.feed
    }

    /// The stored frames, `None` for a camera route.
    pub const fn source(&self) -> Option<FrameSource<'static>> {
        match self.feed {
            Feed::Stored(s) => Some(s),
            Feed::Camera { .. } => None,
        }
    }

    /// The source frame number being sent.
    pub const fn frame(&self) -> u32 {
        self.frame
    }

    /// The counters so far.
    pub const fn stats(&self) -> RouteStats {
        self.stats
    }

    /// Chunks in the current frame.
    pub const fn chunks_per_frame(&self) -> u32 {
        self.chunker.count()
    }

    /// What to do at `now_ms`.
    pub fn next(&self, now_ms: u32) -> Next {
        if self.chunk == 0 {
            let wait = tick_delay(now_ms, self.due);
            if wait > 0 {
                return Next::Wait(wait);
            }
            if self.feed == (Feed::Camera { len: 0 }) {
                return Next::Capture;
            }
        }
        match self.chunker.chunk(self.chunk) {
            Some(plan) => Next::Send(plan),
            // Unreachable: `chunk` stays below `count`.
            None => Next::Wait(0),
        }
    }

    /// The state after the chunk from [`SourceRoute::next`] was broadcast.
    pub fn published(self, now_ms: u32) -> (Self, Report) {
        let count = self.chunker.count();
        let chunk = self.chunk.saturating_add(1);
        if chunk < count {
            return (SourceRoute { chunk, ..self }, Report::NONE);
        }
        self.finish(now_ms, self.stats.sent(count), Report { frames: 1, dropped: 0 })
    }

    /// The state after the capture asked for by [`Next::Capture`]: a frame
    /// of `len` bytes is in the shell's frame buffer and its chunks follow,
    /// or the capture failed and the frame is dropped. A result for a route
    /// that is not waiting for a capture changes nothing.
    pub fn captured(self, now_ms: u32, result: Result<u32, CaptureFault>) -> (Self, Report) {
        if self.feed != (Feed::Camera { len: 0 }) || self.chunk != 0 {
            return (self, Report::NONE);
        }
        let data = usize::from(self.cfg.chunk_data);
        match result.ok().and_then(|len| Chunker::new(self.chunker.seq(), len, data).ok()) {
            Some(chunker) => {
                (SourceRoute { feed: Feed::Camera { len: chunker.total() }, chunker, ..self }, Report::NONE)
            }
            None => {
                let stats = self.stats.skipped(0, 0);
                self.finish(now_ms, stats, Report { frames: 0, dropped: 1 })
            }
        }
    }

    /// The state after the broadcast of the chunk from
    /// [`SourceRoute::next`] failed: the rest of the frame is abandoned.
    pub fn failed(self, now_ms: u32) -> (Self, Report) {
        let stats = self.stats.skipped(self.chunk, self.chunker.count());
        self.finish(now_ms, stats, Report { frames: 0, dropped: 1 })
    }

    /// Schedules the next frame after the current one ended at `now_ms`,
    /// skipping whole periods that already passed.
    fn finish(self, now_ms: u32, stats: RouteStats, report: Report) -> (Self, Report) {
        let period = u32::from(self.cfg.period_ms).max(1);
        let due = self.due.wrapping_add(period);
        let late = if tick_delay(now_ms, due) == 0 { now_ms.wrapping_sub(due) } else { 0 };
        let missed = late / period;
        let count = self.chunker.count();
        let stats = RouteStats {
            frames_dropped: stats.frames_dropped.saturating_add(missed),
            chunks_dropped: stats.chunks_dropped.saturating_add(missed.saturating_mul(count)),
            ..stats
        };
        let seq = middleware::stream::source::next_seq(self.chunker.seq());
        let frame = self.frame.wrapping_add(1).wrapping_add(missed);
        // Every frame of a source was checked when it was built (pattern
        // size by the config, stored frames by `FrameSet::new`), so this
        // cannot fail; if it ever did, the old chunker's length would not
        // match the frame and `write_frame` would refuse its chunks.
        let data = usize::from(self.cfg.chunk_data);
        let (feed, chunker) = match self.feed {
            Feed::Stored(source) => (self.feed, source.chunker(frame, seq, data).unwrap_or(self.chunker)),
            // The next frame is captured when due; its chunker comes then.
            Feed::Camera { .. } => (Feed::Camera { len: 0 }, placeholder(seq, data).unwrap_or(self.chunker)),
        };
        let next = SourceRoute {
            feed,
            chunker,
            frame,
            chunk: 0,
            due: due.wrapping_add(missed.wrapping_mul(period)),
            stats,
            ..self
        };
        (next, Report { dropped: report.dropped.saturating_add(missed), ..report })
    }
}

/// The chunker a camera route holds while no frame is captured: one
/// byte, so it is valid; it is replaced before any chunk is sent.
fn placeholder(seq: u16, data: usize) -> Option<Chunker> {
    Chunker::new(seq, 1, data).ok()
}

/// Why [`write_frame`] could not build a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// The key or chunk does not fit an ESP-NOW frame.
    Frame(FrameError),
    /// The chunk header is invalid for its data.
    Chunk(ChunkError),
}

/// Writes the ESP-NOW frame carrying chunk `plan` of frame `frame` of
/// `source` on `key`, with radio sequence number `radio_seq`, into `out`;
/// returns the frame length. The chunk's bytes are generated or copied
/// straight into `out`.
pub fn write_frame(
    out: &mut [u8; MAX_FRAME],
    radio_seq: u8,
    key: Key<'_>,
    frame: u32,
    source: &FrameSource<'_>,
    plan: &ChunkPlan,
) -> Result<usize, WireError> {
    let len = plan.range.len();
    if len > AIR_CHUNK_DATA {
        return Err(WireError::Frame(FrameError::PayloadTooLarge));
    }
    let header = plan.header.encode(len).map_err(WireError::Chunk)?;
    // The radio header and key, with no payload yet: the chunk follows.
    let at = Frame { seq: radio_seq, key, payload: &[] }.encode(out).map_err(WireError::Frame)?;
    let end = at + HEADER_LEN + len;
    let slot = out.get_mut(at..end).ok_or(WireError::Frame(FrameError::PayloadTooLarge))?;
    let (head, data) = slot.split_at_mut(HEADER_LEN);
    head.copy_from_slice(&header);
    let filled = source.fill(frame, plan.range.start, data);
    if filled != len {
        return Err(WireError::Chunk(ChunkError::Bounds));
    }
    Ok(end)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use middleware::espnow::MAX_KEY;
    use middleware::stream::{ESPNOW_CHUNK_DATA, FrameSet, Sink, chunk, pattern};
    use std::vec;
    use std::vec::Vec;

    const KEY: Key<'static> = Key::from_static("app/cam/frame");
    const PATTERN: FrameSource<'static> = FrameSource::Pattern { width: 120, height: 120 };

    /// Three stored "JPEG" frames of different lengths: 1, 2 and 3 chunks.
    const F0: &[u8] = &[0xA0; 100];
    const F1: &[u8] = &[0xA1; 300];
    const F2: &[u8] = &[0xA2; 400];
    const STORED: &[&[u8]] = &[F0, F1, F2];

    fn jpeg_route(now: u32) -> SourceRoute {
        let cfg = SourceConfig::jpeg(240, 240, 200).expect("valid");
        let set = FrameSet::new(STORED).expect("valid");
        SourceRoute::with_source(cfg, FrameSource::Frames(set), now).expect("valid")
    }

    fn cfg() -> SourceConfig {
        SourceConfig::new(120, 120, 200).expect("valid")
    }

    /// Sends every chunk of the current frame at `now`; returns the state,
    /// the reports and the plans.
    fn send_frame(mut r: SourceRoute, now: u32) -> (SourceRoute, Report, u32) {
        let mut sent = 0;
        for _ in 0..r.chunks_per_frame() {
            match r.next(now) {
                Next::Send(_) => {
                    let (n, rep) = r.published(now);
                    r = n;
                    sent += 1;
                    if rep != Report::NONE {
                        return (r, rep, sent);
                    }
                }
                Next::Wait(_) | Next::Capture => break,
            }
        }
        (r, Report::NONE, sent)
    }

    fn camera(period: u16, now: u32) -> SourceRoute {
        SourceRoute::camera(SourceConfig::jpeg(240, 240, period).expect("valid"), now).expect("valid")
    }

    #[test]
    fn r15_camera_frames_are_captured_when_due_then_sent_from_the_buffer() {
        let r = camera(500, 1000);
        assert_eq!(r.feed(), Feed::Camera { len: 0 });
        assert_eq!(r.source(), None);
        assert_eq!(r.next(900), Next::Wait(100));
        assert_eq!(r.next(1000), Next::Capture);
        // Capture took 80 ms; 400 bytes = 3 chunks of 166, 166, 68.
        let (r, rep) = r.captured(1080, Ok(400));
        assert_eq!(rep, Report::NONE);
        assert_eq!(r.feed(), Feed::Camera { len: 400 });
        assert_eq!(r.chunks_per_frame(), 3);
        let Next::Send(first) = r.next(1080) else { panic!("send") };
        assert_eq!((first.header.total, first.range), (400, 0..166));
        assert_eq!(first.header.seq, 0);
        let (r, rep, sent) = send_frame(r, 1100);
        assert_eq!((rep, sent), (Report { frames: 1, dropped: 0 }, 3));
        // The next frame is captured a period after the first was due.
        assert_eq!(r.feed(), Feed::Camera { len: 0 });
        assert_eq!(r.next(1100), Next::Wait(400));
        assert_eq!(r.next(1500), Next::Capture);
        let (r, _) = r.captured(1550, Ok(1000));
        let Next::Send(p) = r.next(1550) else { panic!("send") };
        assert_eq!((p.header.seq, p.header.total), (1, 1000), "one seq per frame");
        assert_eq!(r.stats().frames, 1);
        assert_eq!(r.stats().chunks, 3);
    }

    #[test]
    fn r15_a_failed_capture_drops_the_frame_and_waits_a_period() {
        for fault in [CaptureFault::Overflow, CaptureFault::Timeout, CaptureFault::Hardware] {
            let r = camera(500, 0);
            let (r, rep) = r.captured(300, Err(fault));
            assert_eq!(rep, Report { frames: 0, dropped: 1 }, "{fault:?}");
            assert_eq!(r.stats().frames_dropped, 1);
            assert_eq!(r.next(300), Next::Wait(200));
            assert_eq!(r.next(500), Next::Capture);
        }
        // A capture slower than whole periods skips them; the next frame,
        // already late, is captured at once.
        let (r, rep) = camera(100, 0).captured(350, Err(CaptureFault::Timeout));
        assert_eq!(rep, Report { frames: 0, dropped: 3 });
        assert_eq!(r.next(350), Next::Capture);
        // An empty or oversize "frame" is a failed capture.
        let (_, rep) = camera(100, 0).captured(10, Ok(0));
        assert_eq!(rep.dropped, 1);
        let (_, rep) = camera(100, 0).captured(10, Ok(middleware::stream::MAX_FRAME_BYTES + 1));
        assert_eq!(rep.dropped, 1);
        // A result nobody asked for changes nothing.
        let (r, _) = camera(100, 0).captured(10, Ok(200));
        assert_eq!(r.captured(20, Ok(300)), (r, Report::NONE));
    }

    #[test]
    fn r15_camera_routes_need_jpeg_and_air_sized_chunks() {
        assert_eq!(SourceRoute::camera(cfg(), 0), Err(AbiError::InvalidArgument), "RGB565");
        let big = SourceConfig::jpeg(240, 240, 100).and_then(|c| c.with_chunk_data(200)).expect("valid");
        assert_eq!(SourceRoute::camera(big, 0), Err(AbiError::Unsupported));
        // A failed broadcast abandons a captured frame like any other.
        let (r, _) = camera(100, 0).captured(5, Ok(400));
        let (r, rep) = r.failed(6);
        assert_eq!(rep, Report { frames: 0, dropped: 1 });
        assert_eq!(r.feed(), Feed::Camera { len: 0 });
        assert_eq!(r.stats().chunks_dropped, 3);
    }

    #[test]
    fn r11_first_frame_is_due_at_start_then_every_period() {
        let r = SourceRoute::new(cfg(), 1000).expect("valid");
        assert_eq!(r.chunks_per_frame(), 174);
        assert!(matches!(r.next(1000), Next::Send(ref p) if p.range == (0..166)));
        let (r, rep, sent) = send_frame(r, 1050);
        assert_eq!((rep, sent), (Report { frames: 1, dropped: 0 }, 174));
        assert_eq!(r.stats(), RouteStats { frames: 1, chunks: 174, ..RouteStats::default() });
        assert_eq!(r.frame(), 1);
        // Next frame due at 1200: wait 150 ms.
        assert_eq!(r.next(1050), Next::Wait(150));
        assert!(matches!(r.next(1200), Next::Send(ref p) if p.header.seq == 1 && p.header.offset == 0));
    }

    #[test]
    fn r11_chunks_follow_back_to_back_within_a_frame() {
        let r = SourceRoute::new(cfg(), 0).expect("valid");
        let (r, rep) = r.published(0);
        assert_eq!(rep, Report::NONE);
        // Mid-frame chunks never wait for the period.
        assert!(matches!(r.next(0), Next::Send(ref p) if p.range == (166..332)));
        let last = (0..172).fold(r, |r, _| r.published(5).0);
        assert!(matches!(last.next(5), Next::Send(ref p) if p.range == (28_718..28_800)));
    }

    #[test]
    fn r11_a_late_frame_starts_at_once_and_whole_missed_periods_are_dropped() {
        let r = SourceRoute::new(cfg(), 0).expect("valid");
        // Finishes at 250: frame 1 (due 200) is late but not missed.
        let (r, rep, _) = send_frame(r, 250);
        assert_eq!(rep, Report { frames: 1, dropped: 0 });
        assert!(matches!(r.next(250), Next::Send(_)));
        assert_eq!(r.frame(), 1);
        // Frame 1 finishes at 1000: periods due 400, 600 and 800 have
        // passed; 3 frames are skipped and frame 5 (due 1000) starts.
        let (r, rep, _) = send_frame(r, 1000);
        assert_eq!(rep, Report { frames: 1, dropped: 3 });
        assert_eq!(r.frame(), 5);
        assert_eq!(r.stats().frames_dropped, 3);
        assert_eq!(r.stats().chunks_dropped, 3 * 174);
        assert!(matches!(r.next(1000), Next::Send(ref p) if p.header.seq == 2), "seq counts attempts");
        assert_eq!(r.next(999), Next::Wait(1));
    }

    #[test]
    fn r11_a_failed_broadcast_abandons_the_frame() {
        let r = SourceRoute::new(cfg(), 0).expect("valid");
        let r = (0..10).fold(r, |r, _| r.published(0).0);
        let (r, rep) = r.failed(20);
        assert_eq!(rep, Report { frames: 0, dropped: 1 });
        assert_eq!(
            r.stats(),
            RouteStats { frames: 0, frames_dropped: 1, chunks: 10, chunks_dropped: 164 }
        );
        assert_eq!(r.next(20), Next::Wait(180));
        assert!(matches!(r.next(200), Next::Send(ref p) if p.header.offset == 0 && p.header.seq == 1));
    }

    #[test]
    fn r11_clock_wrap_and_large_chunks() {
        let r = SourceRoute::new(cfg(), u32::MAX - 10).expect("valid");
        let (r, _, _) = send_frame(r, u32::MAX - 5);
        assert_eq!(r.next(u32::MAX - 5), Next::Wait(200 - 5));
        assert!(matches!(r.next(189), Next::Send(_)), "due across the wrap");
        // 167 data bytes fit one ESP-NOW message; 168 do not.
        let big = cfg().with_chunk_data(167).expect("valid");
        assert!(SourceRoute::new(big, 0).is_ok());
        let too_big = cfg().with_chunk_data(168).expect("valid ABI config");
        assert_eq!(SourceRoute::new(too_big, 0), Err(AbiError::Unsupported));
    }

    #[test]
    fn r11_write_frame_builds_a_radio_frame_with_the_pattern_in_place() {
        let c = cfg();
        let r = SourceRoute::new(c, 0).expect("valid");
        let Next::Send(plan) = r.next(0) else { panic!("due") };
        let mut out = [0u8; MAX_FRAME];
        let n = write_frame(&mut out, 7, KEY, 3, &r.source().expect("stored"), &plan).expect("fits");
        assert_eq!(n, 7 + KEY.as_str().len() + 12 + ESPNOW_CHUNK_DATA);
        let f = Frame::decode(&out[..n]).expect("radio frame");
        assert_eq!((f.seq, f.key), (7, KEY));
        let ch = chunk::decode(f.payload).expect("chunk");
        assert_eq!(ch.header, plan.header);
        let mut want = [0u8; ESPNOW_CHUNK_DATA];
        pattern::fill(3, 120, 120, 0, &mut want);
        assert_eq!(ch.data, &want[..]);
    }

    #[test]
    fn r11_written_frames_rebuild_the_pattern_through_a_sink() {
        let c = cfg();
        let mut r = SourceRoute::new(c, 0).expect("valid");
        let mut rebuilt = vec![0u8; c.frame_len() as usize];
        let mut sink = Sink::new();
        let mut out = [0u8; MAX_FRAME];
        for i in 0..r.chunks_per_frame() {
            let Next::Send(plan) = r.next(0) else { panic!("due") };
            let n = write_frame(&mut out, i as u8, KEY, r.frame(), &r.source().expect("stored"), &plan).expect("fits");
            let f = Frame::decode(&out[..n]).expect("radio frame");
            let (step, data) = sink.receive(c.frame_len(), f.payload);
            assert!(step.forwards());
            rebuilt[plan.range.clone()].copy_from_slice(data);
            sink = step.sink;
            r = r.published(0).0;
        }
        let mut want = vec![0u8; c.frame_len() as usize];
        pattern::fill(0, 120, 120, 0, &mut want);
        assert_eq!(rebuilt, want);
    }

    #[test]
    fn r11_write_frame_refuses_what_does_not_fit() {
        let c = cfg();
        let plan = ChunkPlan {
            header: middleware::stream::ChunkHeader { seq: 0, offset: 0, total: c.frame_len() },
            range: 0..168,
        };
        let mut out = [0u8; MAX_FRAME];
        assert_eq!(
            write_frame(&mut out, 0, KEY, 0, &PATTERN, &plan),
            Err(WireError::Frame(FrameError::PayloadTooLarge))
        );
        let long = std::string::String::from("app/") + &"k".repeat(MAX_KEY);
        let plan = ChunkPlan { range: 0..10, ..plan };
        assert_eq!(
            write_frame(&mut out, 0, Key::new(&long).expect("valid"), 0, &PATTERN, &plan),
            Err(WireError::Frame(FrameError::KeyTooLong))
        );
        // A plan past the frame end is a chunk error, not a short frame.
        let past = ChunkPlan {
            header: middleware::stream::ChunkHeader { seq: 0, offset: 28_790, total: 28_800 },
            range: 28_790..28_810,
        };
        assert_eq!(write_frame(&mut out, 0, KEY, 0, &PATTERN, &past), Err(WireError::Chunk(ChunkError::Bounds)));
    }

    #[test]
    fn r11_jpeg_frames_vary_in_length_and_loop() {
        let mut r = jpeg_route(0);
        let mut now = 0;
        for n in 0..6u32 {
            let want = STORED[n as usize % 3];
            assert_eq!(r.frame(), n);
            assert_eq!(r.chunks_per_frame() as usize, want.len().div_ceil(ESPNOW_CHUNK_DATA));
            let mut rebuilt = Vec::new();
            let mut out = [0u8; MAX_FRAME];
            for i in 0..r.chunks_per_frame() {
                let Next::Send(plan) = r.next(now) else { panic!("due") };
                assert_eq!((plan.header.total as usize, plan.header.seq), (want.len(), n as u16));
                let len = write_frame(&mut out, i as u8, KEY, r.frame(), &r.source().expect("stored"), &plan).expect("fits");
                let f = Frame::decode(&out[..len]).expect("radio frame");
                rebuilt.extend_from_slice(chunk::decode(f.payload).expect("chunk").data);
                r = r.published(now).0;
            }
            assert_eq!(rebuilt, want);
            now += 200;
        }
        assert_eq!(r.stats(), RouteStats { frames: 6, chunks: 2 * (1 + 2 + 3), ..RouteStats::default() });
    }

    #[test]
    fn r11_jpeg_skipped_periods_jump_through_the_loop() {
        let r = jpeg_route(0);
        // Frame 0 (one chunk) ends at 650: the periods due 200 and 400
        // passed, so frames 1 and 2 are skipped and frame 3 (stored frame
        // 0 again, due 600) starts at once.
        let (r, rep) = r.published(650);
        assert_eq!(rep, Report { frames: 1, dropped: 2 });
        assert_eq!(r.frame(), 3);
        assert!(matches!(r.next(650), Next::Send(ref p) if p.header.total == 100 && p.header.seq == 1));
        // The skipped chunks are counted with the next frame's length.
        assert_eq!(r.stats().chunks_dropped, 2);
    }

    #[test]
    fn r11_jpeg_write_frame_copies_the_stored_bytes() {
        let r = jpeg_route(0).published(0).0;
        let Next::Send(plan) = r.next(200) else { panic!("due") };
        let mut out = [0u8; MAX_FRAME];
        let n = write_frame(&mut out, 0, KEY, r.frame(), &r.source().expect("stored"), &plan).expect("fits");
        let ch = chunk::decode(Frame::decode(&out[..n]).expect("radio frame").payload).expect("chunk");
        assert_eq!(ch.data, &F1[..ESPNOW_CHUNK_DATA]);
        // A plan for a longer frame than the stored one is refused.
        let wrong = ChunkPlan {
            header: middleware::stream::ChunkHeader { seq: 0, offset: 166, total: 400 },
            range: 166..332,
        };
        assert_eq!(write_frame(&mut out, 0, KEY, 1, &r.source().expect("stored"), &wrong), Err(WireError::Chunk(ChunkError::Bounds)));
    }

    #[test]
    fn r11_test_pattern_jpeg_needs_the_fixtures() {
        let jpeg = SourceConfig::jpeg(240, 240, 200).expect("valid");
        let odd = SourceConfig::jpeg(200, 200, 200).expect("valid");
        #[cfg(not(feature = "cam-fixtures"))]
        {
            assert_eq!(SourceRoute::new(jpeg, 0), Err(AbiError::Unsupported));
            assert_eq!(SourceRoute::new(odd, 0), Err(AbiError::Unsupported));
        }
        #[cfg(feature = "cam-fixtures")]
        {
            use middleware::stream::frames::fixtures;
            assert_eq!(SourceRoute::new(odd, 0), Err(AbiError::InvalidArgument));
            for size in [240, 120] {
                let cfg = SourceConfig::jpeg(size, size, 200).expect("valid");
                let r = SourceRoute::new(cfg, 0).expect("fixtures");
                let set = fixtures::jpeg(size, size).expect("fixture set");
                assert_eq!(r.source(), Some(FrameSource::Frames(set)));
                assert_eq!(r.chunks_per_frame(), set.frame_len(0).div_ceil(166));
            }
            let _ = jpeg;
        }
        let too_big = jpeg.with_chunk_data(168).expect("valid ABI config");
        let set = FrameSet::new(STORED).expect("valid");
        assert_eq!(SourceRoute::with_source(too_big, FrameSource::Frames(set), 0), Err(AbiError::Unsupported));
    }
}
