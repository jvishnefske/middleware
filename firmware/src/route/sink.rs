//! Sink routes: what to put on the SPI bus for each received chunk.
//!
//! A [`SinkRoute`] is the state of one running sink: its validated
//! [`SinkConfig`], the stream [`Sink`] state machine and its
//! [`RouteStats`]. [`SinkRoute::receive`] takes one message received on the
//! route's key and returns the next state with a [`Plan`]: the sink
//! [`Step`] and the chunk data to forward (empty when the chunk is
//! dropped). [`transfers`] turns a plan into the bus transfers, in order:
//!
//! 1. on [`Action::StartFrame`], every preamble segment, each with the D/C
//!    pin at the segment's level (e.g. `CASET` low, its parameters high);
//! 2. on `StartFrame` or [`Action::Forward`], the chunk data with D/C high.
//!
//! A dropped chunk makes no transfer. A plan makes at most
//! [`MAX_TRANSFERS`] transfers, so the shell's loop over them is bounded.
//! [`Plan::report`] gives the frames completed and abandoned for the
//! route's events; the counters are already folded into the next state.

use middleware::stream::{Action, RouteStats, Sink, Step};
use middleware::wasm_abi::route::{MAX_SEGMENTS, Preamble, Segment, SinkConfig};

use super::Report;

/// Most bus transfers one received chunk causes: every preamble segment
/// and the data.
pub const MAX_TRANSFERS: usize = MAX_SEGMENTS + 1;

/// One running sink route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SinkRoute {
    cfg: SinkConfig,
    sink: Sink,
    stats: RouteStats,
}

/// What one received message asks of the bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan<'m> {
    /// The sink's decision.
    pub step: Step,
    /// The chunk data to send (empty unless the step forwards).
    pub data: &'m [u8],
}

impl Plan<'_> {
    /// Frames completed and abandoned by this message.
    pub const fn report(&self) -> Report {
        Report { frames: self.step.completed as u32, dropped: self.step.aborted as u32 }
    }

    /// Whether the preamble goes out before the data.
    pub const fn starts_frame(&self) -> bool {
        matches!(self.step.action, Action::StartFrame)
    }
}

impl SinkRoute {
    /// A sink that has seen nothing.
    pub const fn new(cfg: SinkConfig) -> Self {
        SinkRoute { cfg, sink: Sink::new(), stats: RouteStats { frames: 0, frames_dropped: 0, chunks: 0, chunks_dropped: 0 } }
    }

    /// The route's config.
    pub const fn config(&self) -> &SinkConfig {
        &self.cfg
    }

    /// The counters so far.
    pub const fn stats(&self) -> RouteStats {
        self.stats
    }

    /// Decodes `msg` as a chunk and decides what to send; the counters
    /// include this message.
    pub fn receive(self, msg: &[u8]) -> (Self, Plan<'_>) {
        let (step, data) = self.sink.receive(self.cfg.frame_len, msg);
        let next = SinkRoute { sink: step.sink, stats: self.stats.record(&step), ..self };
        (next, Plan { step, data })
    }
}

/// The transfers for `plan`, preamble segments (from `preamble`) first.
pub fn transfers<'a>(preamble: &'a Preamble, plan: &Plan<'a>) -> impl Iterator<Item = Segment<'a>> {
    let start = plan.starts_frame();
    let data = plan.step.forwards().then_some(Segment { dc: true, bytes: plan.data });
    preamble
        .segments()
        .filter(move |_| start)
        .chain(data)
        .filter(|s| !s.bytes.is_empty())
        .take(MAX_TRANSFERS)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use middleware::stream::{ChunkHeader, DropReason, HEADER_LEN};
    use std::vec::Vec;

    fn cfg() -> SinkConfig {
        SinkConfig::new(13, 6)
            .expect("valid")
            .with_preamble(Preamble::mipi_window(60, 60, 179, 179).expect("fits"))
    }

    fn msg(seq: u16, offset: u32, data: &[u8]) -> Vec<u8> {
        let mut out = std::vec![0u8; HEADER_LEN + data.len()];
        ChunkHeader { seq, offset, total: 6 }.write(data, &mut out).expect("valid");
        out
    }

    fn plan_of(cfg: &SinkConfig, plan: &Plan<'_>) -> Vec<(bool, Vec<u8>)> {
        transfers(&cfg.preamble, plan).map(|s| (s.dc, s.bytes.to_vec())).collect()
    }

    #[test]
    fn r12_frame_start_sends_the_preamble_with_dc_levels_then_data() {
        let r = SinkRoute::new(cfg());
        let m = msg(1, 0, &[1, 2, 3]);
        let (r, p) = r.receive(&m);
        assert!(p.starts_frame());
        assert_eq!(
            plan_of(r.config(), &p),
            [
                (false, std::vec![0x2A]),
                (true, std::vec![0, 60, 0, 179]),
                (false, std::vec![0x2B]),
                (true, std::vec![0, 60, 0, 179]),
                (false, std::vec![0x2C]),
                (true, std::vec![1, 2, 3]),
            ]
        );
        assert_eq!(p.report(), Report::NONE);
        // The continuation is data only, with D/C high; it completes the frame.
        let m = msg(1, 3, &[4, 5, 6]);
        let (r, p) = r.receive(&m);
        assert_eq!(plan_of(r.config(), &p), [(true, std::vec![4, 5, 6])]);
        assert_eq!(p.report(), Report { frames: 1, dropped: 0 });
        assert_eq!(r.stats(), RouteStats { frames: 1, chunks: 2, ..RouteStats::default() });
        // The next frame re-sends the preamble.
        let m = msg(2, 0, &[9]);
        let (_, p) = r.receive(&m);
        assert_eq!(plan_of(&cfg(), &p).len(), 6);
    }

    #[test]
    fn r12_dropped_chunks_make_no_transfer_and_gaps_abandon_the_frame() {
        let r = SinkRoute::new(cfg());
        let first = msg(1, 0, &[1, 2]);
        let (r, _) = r.receive(&first);
        // Duplicate: dropped, frame continues.
        let (r, p) = r.receive(&first);
        assert_eq!(p.step.action, Action::Drop(DropReason::Duplicate));
        assert!(plan_of(r.config(), &p).is_empty());
        // Gap: dropped and the frame is abandoned.
        let late = msg(1, 4, &[5, 6]);
        let (r, p) = r.receive(&late);
        assert!(plan_of(r.config(), &p).is_empty());
        assert_eq!(p.report(), Report { frames: 0, dropped: 1 });
        // Garbage is malformed and dropped.
        let (r, p) = r.receive(&[1, 2, 3]);
        assert!(matches!(p.step.action, Action::Drop(DropReason::Malformed(_))));
        assert_eq!(
            r.stats(),
            RouteStats { frames: 0, frames_dropped: 1, chunks: 1, chunks_dropped: 3 }
        );
    }

    #[test]
    fn r12_no_preamble_means_data_only_and_transfers_are_bounded() {
        let bare = SinkConfig::new(13, 6).expect("valid");
        let r = SinkRoute::new(bare);
        let m = msg(1, 0, &[1, 2, 3, 4, 5, 6]);
        let (r, p) = r.receive(&m);
        assert_eq!(plan_of(r.config(), &p), [(true, std::vec![1, 2, 3, 4, 5, 6])]);
        assert_eq!(p.report(), Report { frames: 1, dropped: 0 });
        // A full preamble of MAX_SEGMENTS segments plus data fits the bound.
        let full = (0..MAX_SEGMENTS).fold(Preamble::new(), |p, i| p.push(i % 2 == 1, &[i as u8]).expect("fits"));
        let (_, p) = SinkRoute::new(bare.with_preamble(full)).receive(&m);
        assert_eq!(transfers(&full, &p).count(), MAX_TRANSFERS);
    }
}
