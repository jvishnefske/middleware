//! The sink side: deciding what to do with each received chunk.
//!
//! A sink forwards chunk data, in arrival order, to a device that only
//! accepts a contiguous byte stream (a display in `RAMWR` mode). The
//! preamble (the window command) is sent before a frame's first chunk;
//! after that the device's write position advances with every forwarded
//! byte, so a chunk may be forwarded only if it starts exactly where the
//! previous one ended.
//!
//! [`Sink`] is the state; [`Sink::step`] takes it with one chunk's header
//! and data length and returns a [`Step`]: the next state, the [`Action`]
//! and whether a frame completed or was abandoned. It never forwards a
//! chunk that is not the exact continuation of the frame in progress, so
//! lost, reordered or repeated chunks leave holes, never misplaced bytes.
//!
//! ```text
//!            first chunk of a new frame (total == frame_len)
//!  Idle ───────────────────────────────────────────────────▶ Receiving
//!   ▲  StartFrame (preamble + data)                            │  │
//!   │                                                           │  │ offset == next:
//!   │  last byte forwarded (completed)                          │  │ Forward
//!   ├───────────────────────────────────────────────────────────┘  │
//!   │  offset > next (gap) or a chunk of another frame (aborted)    │
//!   └───────────────────────────────────────────────────────────────┘
//! ```
//!
//! Rules, in order, for a chunk that decodes:
//!
//! 1. While `Receiving` frame `s`, a chunk of another frame abandons `s`
//!    (`aborted`) and is then handled as in `Idle`. A newer frame that
//!    began while `s` was incomplete therefore takes over at its first
//!    chunk.
//! 2. In `Idle`, only a first chunk (`offset == 0`) is accepted, unless its
//!    `seq` is that of the frame started last (a repeat:
//!    [`DropReason::Duplicate`]) or its `total` is not the sink's frame
//!    length ([`DropReason::WrongSize`]). Accepting it is
//!    [`Action::StartFrame`]: send the preamble, then the data.
//! 3. In `Receiving` frame `s`, a chunk of `s` is forwarded if `offset ==
//!    next`; `offset < next` is a repeat (dropped, frame continues);
//!    `offset > next` is a gap (dropped, frame abandoned); a different
//!    `total` is [`DropReason::WrongSize`] (dropped, frame continues).
//!
//! When the forwarded bytes reach `total`, the step reports `completed` and
//! the sink returns to `Idle`. Sequence numbers are only compared for
//! equality, so wrapping from `u16::MAX` to 0 needs no special case. A
//! source that restarts its numbering loses at most one frame (when its
//! first `seq` equals the last one seen).

use super::chunk::{self, ChunkError, ChunkHeader};

/// What a sink knows about the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sink {
    /// Between frames: waiting for a frame's first chunk.
    Idle {
        /// Sequence number of the frame started last, to recognise its
        /// repeated first chunk.
        last: Option<u16>,
    },
    /// Forwarded bytes `0..next` of frame `seq`, with `0 < next < total`.
    Receiving {
        /// The frame in progress.
        seq: u16,
        /// Offset the next forwarded chunk must start at.
        next: u32,
    },
}

impl Default for Sink {
    fn default() -> Self {
        Self::new()
    }
}

/// What to do with a chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Send the preamble, then the chunk data: a new frame starts.
    StartFrame,
    /// Send the chunk data; it continues the frame in progress.
    Forward,
    /// Discard the chunk; nothing is sent.
    Drop(DropReason),
}

/// Why a chunk was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// The message is not a valid chunk.
    Malformed(ChunkError),
    /// Its frame length differs from the sink's.
    WrongSize,
    /// Already forwarded (or a repeat of the frame started last).
    Duplicate,
    /// Not a first chunk, and no frame is in progress.
    NoFrame,
    /// It starts beyond the next expected offset; the frame is abandoned.
    Gap,
}

/// The outcome of [`Sink::step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    /// The state after this chunk.
    pub sink: Sink,
    /// What to do with the chunk.
    pub action: Action,
    /// This chunk was the last byte range of its frame: the frame is
    /// complete once the chunk is sent.
    pub completed: bool,
    /// A frame in progress was abandoned (gap, or another frame arrived).
    pub aborted: bool,
}

impl Step {
    const fn drop(sink: Sink, reason: DropReason) -> Self {
        Step {
            sink,
            action: Action::Drop(reason),
            completed: false,
            aborted: false,
        }
    }

    /// Whether the chunk's data is sent ([`Action::StartFrame`] or
    /// [`Action::Forward`]).
    pub const fn forwards(&self) -> bool {
        matches!(self.action, Action::StartFrame | Action::Forward)
    }
}

impl Sink {
    /// A sink that has seen nothing.
    pub const fn new() -> Self {
        Sink::Idle { last: None }
    }

    /// Decides what to do with a chunk of `len` data bytes for a sink
    /// expecting frames of `frame_len` bytes. The header must have passed
    /// [`ChunkHeader::check`] (as every [`chunk::decode`] result has); one
    /// that does not is dropped as malformed.
    pub const fn step(self, frame_len: u32, header: &ChunkHeader, len: usize) -> Step {
        if let Err(e) = header.check(len) {
            return Step::drop(self, DropReason::Malformed(e));
        }
        match self {
            Sink::Receiving { seq, next } if seq == header.seq => {
                Self::continue_frame(seq, next, frame_len, header, len)
            }
            Sink::Receiving { seq, .. } => {
                let step = Sink::Idle { last: Some(seq) }.start(frame_len, header, len);
                Step {
                    aborted: true,
                    ..step
                }
            }
            Sink::Idle { .. } => self.start(frame_len, header, len),
        }
    }

    /// Decodes `msg` as a chunk and steps; returns the step and the data to
    /// send (empty when the chunk is dropped).
    pub fn receive(self, frame_len: u32, msg: &[u8]) -> (Step, &[u8]) {
        match chunk::decode(msg) {
            Ok(c) => {
                let step = self.step(frame_len, &c.header, c.data.len());
                let data = if step.forwards() { c.data } else { &[] };
                (step, data)
            }
            Err(e) => (Step::drop(self, DropReason::Malformed(e)), &[]),
        }
    }

    /// Rule 2: an `Idle` sink (or one that just abandoned a frame).
    const fn start(self, frame_len: u32, h: &ChunkHeader, len: usize) -> Step {
        let last = match self {
            Sink::Idle { last } => last,
            Sink::Receiving { seq, .. } => Some(seq),
        };
        if !h.is_first() {
            return Step::drop(self, DropReason::NoFrame);
        }
        if matches!(last, Some(l) if l == h.seq) {
            return Step::drop(self, DropReason::Duplicate);
        }
        if h.total != frame_len {
            return Step::drop(self, DropReason::WrongSize);
        }
        // `check` passed, so `len` fits the frame and `u32`.
        let next = len as u32;
        Step {
            sink: Self::after(h.seq, next, h.total),
            action: Action::StartFrame,
            completed: next == h.total,
            aborted: false,
        }
    }

    /// Rule 3: a chunk of the frame in progress.
    const fn continue_frame(
        seq: u16,
        next: u32,
        frame_len: u32,
        h: &ChunkHeader,
        len: usize,
    ) -> Step {
        let here = Sink::Receiving { seq, next };
        if h.total != frame_len {
            return Step::drop(here, DropReason::WrongSize);
        }
        if h.offset < next {
            return Step::drop(here, DropReason::Duplicate);
        }
        if h.offset > next {
            return Step {
                sink: Sink::Idle { last: Some(seq) },
                action: Action::Drop(DropReason::Gap),
                completed: false,
                aborted: true,
            };
        }
        let end = next + len as u32; // `check` bounds it by `total`
        Step {
            sink: Self::after(seq, end, h.total),
            action: Action::Forward,
            completed: end == h.total,
            aborted: false,
        }
    }

    /// The state once bytes `0..end` of frame `seq` are forwarded.
    const fn after(seq: u16, end: u32, total: u32) -> Sink {
        if end >= total {
            Sink::Idle { last: Some(seq) }
        } else {
            Sink::Receiving { seq, next: end }
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::stream::{Chunker, HEADER_LEN};
    use std::vec::Vec;

    const LEN: u32 = 10;

    fn h(seq: u16, offset: u32) -> ChunkHeader {
        ChunkHeader {
            seq,
            offset,
            total: LEN,
        }
    }

    /// What was sent: `None` for a preamble, `Some((seq, offset))` per
    /// forwarded data byte.
    type Sent = Vec<Option<(u16, u32)>>;

    /// Feeds `(seq, offset, len)` chunks; returns the bytes "sent", the
    /// completed and aborted counts, and the final state.
    fn run(sink: Sink, chunks: &[(u16, u32, usize)]) -> (Sent, u32, u32, Sink) {
        let mut sent = Vec::new();
        let (mut done, mut aborted) = (0, 0);
        let mut s = sink;
        for &(seq, off, len) in chunks {
            let step = s.step(LEN, &h(seq, off), len);
            if step.action == Action::StartFrame {
                sent.push(None);
            }
            if step.forwards() {
                for i in 0..len as u32 {
                    sent.push(Some((seq, off + i)));
                }
            }
            done += u32::from(step.completed);
            aborted += u32::from(step.aborted);
            s = step.sink;
        }
        (sent, done, aborted, s)
    }

    /// The core safety property: within each preamble-delimited run, the
    /// forwarded bytes are offsets 0, 1, 2, ... of a single frame.
    fn assert_never_misplaced(sent: &[Option<(u16, u32)>]) {
        let mut expect: Option<(u16, u32)> = None;
        for b in sent {
            match (b, expect) {
                (None, _) => expect = Some((u16::MAX, 0)),
                (Some((seq, off)), Some((_, 0))) => {
                    assert_eq!(*off, 0, "frame starts at offset 0");
                    expect = Some((*seq, 1));
                }
                (Some((seq, off)), Some((eseq, eoff))) => {
                    assert_eq!((*seq, *off), (eseq, eoff), "contiguous bytes of one frame");
                    expect = Some((eseq, eoff + 1));
                }
                (Some(_), None) => panic!("data before any preamble"),
            }
        }
    }

    #[test]
    fn in_order_frame_completes() {
        let (sent, done, aborted, s) = run(Sink::new(), &[(1, 0, 4), (1, 4, 4), (1, 8, 2)]);
        assert_eq!(sent.len(), 11);
        assert_eq!(sent[0], None);
        assert_eq!((done, aborted), (1, 0));
        assert_eq!(s, Sink::Idle { last: Some(1) });
        assert_never_misplaced(&sent);
    }

    #[test]
    fn single_chunk_frames() {
        let one = |seq| (seq, 0, LEN as usize);
        let (sent, done, aborted, _) = run(Sink::new(), &[one(1), one(2), one(2), one(3)]);
        assert_eq!((done, aborted), (3, 0), "the repeat of frame 2 is dropped");
        assert_eq!(sent.iter().filter(|b| b.is_none()).count(), 3);
        assert_never_misplaced(&sent);
    }

    #[test]
    fn loss_of_a_middle_chunk_abandons_the_frame() {
        let s = Sink::new().step(LEN, &h(1, 0), 4).sink;
        let gap = s.step(LEN, &h(1, 8), 2);
        assert_eq!(gap.action, Action::Drop(DropReason::Gap));
        assert!(gap.aborted && !gap.completed);
        assert_eq!(gap.sink, Sink::Idle { last: Some(1) });
        // The late chunk can no longer be placed.
        let late = gap.sink.step(LEN, &h(1, 4), 4);
        assert_eq!(late.action, Action::Drop(DropReason::NoFrame));
        // The next frame starts cleanly.
        let next = late.sink.step(LEN, &h(2, 0), 4);
        assert_eq!(next.action, Action::StartFrame);
    }

    #[test]
    fn loss_of_the_first_chunk_skips_the_frame() {
        let (sent, done, aborted, _) =
            run(Sink::new(), &[(1, 4, 4), (1, 8, 2), (2, 0, 4), (2, 4, 6)]);
        assert_eq!((done, aborted), (1, 0));
        assert_eq!(sent[0], None);
        assert_eq!(sent[1], Some((2, 0)));
        assert_never_misplaced(&sent);
    }

    #[test]
    fn loss_of_the_last_chunk_is_abandoned_by_the_next_frame() {
        let s = Sink::new().step(LEN, &h(1, 0), 4).sink;
        let s = s.step(LEN, &h(1, 4), 4).sink;
        let next = s.step(LEN, &h(2, 0), 4);
        assert_eq!(next.action, Action::StartFrame);
        assert!(next.aborted, "frame 1 never completed");
        assert_eq!(next.sink, Sink::Receiving { seq: 2, next: 4 });
    }

    #[test]
    fn duplicates_are_dropped_without_disturbing_the_frame() {
        let (sent, done, aborted, _) = run(
            Sink::new(),
            &[
                (1, 0, 4),
                (1, 0, 4),
                (1, 4, 4),
                (1, 4, 4),
                (1, 0, 4),
                (1, 8, 2),
                (1, 8, 2),
            ],
        );
        assert_eq!((done, aborted), (1, 0));
        assert_eq!(sent.len(), 11);
        assert_never_misplaced(&sent);
        let s = Sink::new().step(LEN, &h(1, 0), 4).sink;
        assert_eq!(
            s.step(LEN, &h(1, 0), 4).action,
            Action::Drop(DropReason::Duplicate)
        );
    }

    #[test]
    fn reordering_never_forwards_a_misplaced_chunk() {
        // Middle chunks swapped: the frame is abandoned, nothing misplaced.
        let (sent, done, aborted, _) = run(Sink::new(), &[(1, 0, 4), (1, 8, 2), (1, 4, 4)]);
        assert_eq!((done, aborted), (0, 1));
        assert_eq!(sent.len(), 5);
        assert_never_misplaced(&sent);
        // A stale chunk of an older frame arriving mid-frame abandons it; a
        // stale first chunk restarts the window at 0 (still correct).
        let (sent, _, aborted, _) = run(Sink::new(), &[(5, 0, 4), (4, 8, 2), (5, 4, 4), (4, 0, 4)]);
        assert_eq!(aborted, 1);
        assert_never_misplaced(&sent);
    }

    #[test]
    fn exhaustive_small_permutations_never_misplace() {
        // Every ordering (with repeats) of three frames' chunks drawn from a
        // fixed pool, 6 chunks long: the invariant must hold for all.
        let pool: [(u16, u32, usize); 7] = [
            (1, 0, 5),
            (1, 5, 5),
            (2, 0, 4),
            (2, 4, 4),
            (2, 8, 2),
            (u16::MAX, 0, 10),
            (0, 0, 10),
        ];
        let n = pool.len();
        let mut seqs = 0;
        for code in 0..n.pow(5) {
            let mut c = code;
            let mut chunks = [(0u16, 0u32, 0usize); 5];
            for slot in &mut chunks {
                *slot = pool[c % n];
                c /= n;
            }
            let (sent, done, _, _) = run(Sink::new(), &chunks);
            assert_never_misplaced(&sent);
            seqs += done;
        }
        assert!(seqs > 0);
    }

    #[test]
    fn seq_wrap_is_a_new_frame() {
        let (sent, done, aborted, s) = run(
            Sink::new(),
            &[(u16::MAX, 0, 6), (u16::MAX, 6, 4), (0, 0, 6), (0, 6, 4)],
        );
        assert_eq!((done, aborted), (2, 0));
        assert_eq!(s, Sink::Idle { last: Some(0) });
        assert_never_misplaced(&sent);
    }

    #[test]
    fn wrong_size_frames_are_dropped() {
        let other = ChunkHeader {
            seq: 1,
            offset: 0,
            total: LEN + 2,
        };
        let s = Sink::new().step(LEN, &other, 4);
        assert_eq!(s.action, Action::Drop(DropReason::WrongSize));
        assert_eq!(s.sink, Sink::new());
        // Mid-frame, a same-seq chunk with another total is dropped and the
        // frame continues.
        let r = Sink::new().step(LEN, &h(3, 0), 4).sink;
        let bad = r.step(
            LEN,
            &ChunkHeader {
                seq: 3,
                offset: 4,
                total: LEN + 2,
            },
            4,
        );
        assert_eq!(bad.action, Action::Drop(DropReason::WrongSize));
        assert_eq!(bad.sink, r);
    }

    #[test]
    fn malformed_zero_length_and_oversize_chunks_are_dropped() {
        let s = Sink::new().step(LEN, &h(1, 0), 4).sink;
        let empty = s.step(LEN, &h(1, 4), 0);
        assert_eq!(
            empty.action,
            Action::Drop(DropReason::Malformed(ChunkError::Empty))
        );
        assert_eq!(empty.sink, s);
        let over = s.step(LEN, &h(1, 4), 7);
        assert_eq!(
            over.action,
            Action::Drop(DropReason::Malformed(ChunkError::Bounds))
        );
        assert_eq!(over.sink, s, "an overrunning chunk changes nothing");
        let (step, data) = s.receive(LEN, &[1, 2, 3]);
        assert_eq!(
            step.action,
            Action::Drop(DropReason::Malformed(ChunkError::Truncated))
        );
        assert!(data.is_empty());
        assert_eq!(step.sink, s);
    }

    #[test]
    fn receive_returns_the_data_to_send() {
        let c = Chunker::new(9, LEN, 6).expect("valid");
        let mut msg = [0u8; HEADER_LEN + 6];
        let p = c.chunk(0).expect("first");
        let n = p.header.write(&[1, 2, 3, 4, 5, 6], &mut msg).expect("fits");
        let (step, data) = Sink::new().receive(LEN, &msg[..n]);
        assert_eq!(step.action, Action::StartFrame);
        assert_eq!(data, &[1, 2, 3, 4, 5, 6]);
        let (again, data) = step.sink.receive(LEN, &msg[..n]);
        assert_eq!(again.action, Action::Drop(DropReason::Duplicate));
        assert!(data.is_empty());
    }

    #[test]
    fn source_restart_with_the_same_seq_loses_one_frame() {
        let (_, done, _, s) = run(Sink::new(), &[(0, 0, 10), (0, 0, 10), (1, 0, 10)]);
        assert_eq!(done, 2);
        assert_eq!(s, Sink::Idle { last: Some(1) });
    }
}
