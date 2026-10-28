//! Frame reassembly: a pure state machine that decides where each received
//! chunk goes among three frame slots, so one frame can be decoded while
//! the next arrives and a newer complete frame waits.
//!
//! # Slots
//!
//! The [`SLOTS`] (3) buffers of [`crate::FRAME_CAP`] bytes each live in the
//! shell; this module only hands out indices. At any time each slot has
//! exactly one role, and the three roles are always a permutation of the
//! slot indices:
//!
//! * **fill** — the frame being received;
//! * **ready** — the newest complete frame not yet taken by the decoder
//!   (possibly empty);
//! * **decode** — the frame being decoded (possibly empty). Its bytes must
//!   stay unchanged while the decoder borrows them, and no chunk is ever
//!   stored into it.
//!
//! When the fill frame completes, fill and ready swap: the completed frame
//! becomes ready, and a ready frame that was never taken is *superseded*
//! (its slot is the next fill). [`Reassembler::take`] swaps ready and
//! decode when the decoder is free. So the decoder always gets the newest
//! complete frame, skipping older ones when it falls behind, and reception
//! never waits for the decoder. (Two slots would force a choice between
//! dropping every frame that arrives during a decode and overwriting the
//! only complete frame with one that may never complete.)
//!
//! # Chunk rules
//!
//! Chunks are `middleware::stream::chunk` messages; a frame's length is
//! each chunk header's `total`. A chunk is stored only when it is
//!
//! * the first chunk of a frame (`offset == 0`) whose `total` fits
//!   [`crate::FRAME_CAP`] — it abandons any frame in progress; or
//! * the exact continuation of the frame in progress: same sequence number
//!   and length, `offset` equal to the bytes received so far.
//!
//! A chunk of the frame in progress that lies before the received bytes is
//! a duplicate and is dropped without disturbing the frame. Any other chunk
//! (a gap, another frame's middle, a malformed message) abandons the frame
//! in progress, if any. So a stored byte is always at its place in the
//! frame, and a completed frame holds every byte of one source frame.

use middleware::stream::chunk;

/// Number of frame slots.
pub const SLOTS: usize = 3;

/// A complete frame in a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    /// Slot index, `0..SLOTS`.
    pub slot: u8,
    /// Frame length in bytes.
    pub len: u32,
    /// Frame sequence number from the chunk headers.
    pub seq: u16,
}

/// Why a chunk was not stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// Not a valid chunk (`stream::chunk::decode` refused it).
    Malformed,
    /// A frame's first chunk whose frame is longer than the slots.
    TooLarge,
    /// A chunk already received for the frame in progress.
    Duplicate,
    /// Not the continuation of the frame in progress (a lost chunk, or the
    /// middle of a frame whose start was missed).
    Gap,
}

/// What the shell does with a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Copy the message's data (everything after the
    /// [`chunk::HEADER_LEN`]-byte header) into slot `slot` at `offset`.
    /// `completed` is the frame this chunk completed, now ready.
    Store {
        /// Slot index.
        slot: u8,
        /// Byte offset in the slot.
        offset: u32,
        /// The completed frame, when this was its last chunk.
        completed: Option<Frame>,
    },
    /// Ignore the message.
    Drop(DropReason),
}

/// Reception counters, all saturating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RxStats {
    /// Frames received completely.
    pub received: u32,
    /// Frames abandoned after a lost chunk, or too large for a slot.
    pub lost: u32,
    /// Complete frames replaced by a newer one before the decoder took
    /// them.
    pub superseded: u32,
    /// Chunks not stored (every [`DropReason`]).
    pub chunks_dropped: u32,
}

/// Progress of the frame in the fill slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fill {
    /// Waiting for a frame's first chunk.
    Idle,
    /// `next` of `total` bytes of frame `seq` received.
    Receiving { seq: u16, total: u32, next: u32 },
}

/// The reassembler's state; see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reassembler {
    cap: u32,
    fill_slot: u8,
    ready_slot: u8,
    decode_slot: u8,
    fill: Fill,
    ready: Option<Frame>,
    decoding: Option<Frame>,
    stats: RxStats,
}

impl Reassembler {
    /// Empty slots of `cap` bytes each.
    pub const fn new(cap: u32) -> Self {
        Reassembler {
            cap,
            fill_slot: 0,
            ready_slot: 1,
            decode_slot: 2,
            fill: Fill::Idle,
            ready: None,
            decoding: None,
            stats: RxStats {
                received: 0,
                lost: 0,
                superseded: 0,
                chunks_dropped: 0,
            },
        }
    }

    /// The counters.
    pub const fn stats(&self) -> RxStats {
        self.stats
    }

    /// The frame waiting for the decoder, if any.
    pub const fn ready(&self) -> Option<Frame> {
        self.ready
    }

    /// The frame the decoder holds, if any.
    pub const fn decoding(&self) -> Option<Frame> {
        self.decoding
    }

    /// The slot chunks are stored into.
    pub const fn fill_slot(&self) -> u8 {
        self.fill_slot
    }

    /// Feeds one received message.
    pub fn on_message(self, msg: &[u8]) -> (Self, Verdict) {
        let Ok(c) = chunk::decode(msg) else {
            return self.dropped(DropReason::Malformed, true);
        };
        let (h, len) = (c.header, c.data.len() as u32);
        // `decode` guarantees `offset + len <= total`.
        let end = h.offset.saturating_add(len);
        let next = if h.is_first() {
            if h.total > self.cap {
                return self.dropped(DropReason::TooLarge, true);
            }
            let lost = matches!(self.fill, Fill::Receiving { .. });
            Reassembler {
                stats: self.counted_lost(lost),
                ..self
            }
        } else {
            match self.fill {
                Fill::Receiving { seq, total, next }
                    if seq == h.seq && total == h.total && h.offset == next =>
                {
                    self
                }
                Fill::Receiving { seq, total, next }
                    if seq == h.seq && total == h.total && end <= next =>
                {
                    return self.dropped(DropReason::Duplicate, false);
                }
                _ => return self.dropped(DropReason::Gap, true),
            }
        };
        let slot = next.fill_slot;
        if end < h.total {
            let state = Reassembler {
                fill: Fill::Receiving {
                    seq: h.seq,
                    total: h.total,
                    next: end,
                },
                ..next
            };
            let verdict = Verdict::Store {
                slot,
                offset: h.offset,
                completed: None,
            };
            return (state, verdict);
        }
        let frame = Frame {
            slot,
            len: h.total,
            seq: h.seq,
        };
        let stats = RxStats {
            received: next.stats.received.saturating_add(1),
            superseded: next
                .stats
                .superseded
                .saturating_add(next.ready.is_some() as u32),
            ..next.stats
        };
        let state = Reassembler {
            fill_slot: next.ready_slot,
            ready_slot: slot,
            fill: Fill::Idle,
            ready: Some(frame),
            stats,
            ..next
        };
        let verdict = Verdict::Store {
            slot,
            offset: h.offset,
            completed: Some(frame),
        };
        (state, verdict)
    }

    /// The decoder is free: hands it the ready frame, if any (the previous
    /// decode slot becomes the empty ready slot). Returns `None`, changing
    /// nothing, while a frame is still being decoded (call
    /// [`done`](Self::done) first) or none is ready.
    pub fn take(self) -> (Self, Option<Frame>) {
        match (self.decoding, self.ready) {
            (None, Some(frame)) => (
                Reassembler {
                    ready_slot: self.decode_slot,
                    decode_slot: self.ready_slot,
                    ready: None,
                    decoding: Some(frame),
                    ..self
                },
                Some(frame),
            ),
            _ => (self, None),
        }
    }

    /// The decoder finished (or gave up on) its frame.
    pub fn done(self) -> Self {
        Reassembler {
            decoding: None,
            ..self
        }
    }

    /// `stats` with one more lost frame when `lost`.
    fn counted_lost(&self, lost: bool) -> RxStats {
        RxStats {
            lost: self.stats.lost.saturating_add(lost as u32),
            ..self.stats
        }
    }

    /// Drops a chunk; `abandon` also gives up the frame in progress.
    fn dropped(self, why: DropReason, abandon: bool) -> (Self, Verdict) {
        let lost = abandon && matches!(self.fill, Fill::Receiving { .. });
        let lost = lost || why == DropReason::TooLarge;
        let stats = RxStats {
            chunks_dropped: self.stats.chunks_dropped.saturating_add(1),
            ..self.counted_lost(lost)
        };
        let fill = if abandon { Fill::Idle } else { self.fill };
        (
            Reassembler {
                fill,
                stats,
                ..self
            },
            Verdict::Drop(why),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::stream::{ChunkHeader, Chunker, HEADER_LEN};

    const CAP: u32 = 64;

    /// The chunks of a frame of `bytes` (sequence `seq`) at `max` data
    /// bytes each, as messages.
    fn chunks(seq: u16, bytes: &[u8], max: usize) -> Vec<Vec<u8>> {
        let chunker = Chunker::new(seq, bytes.len() as u32, max).expect("valid");
        (0..chunker.count())
            .map(|i| {
                let plan = chunker.chunk(i).expect("in range");
                let data = &bytes[plan.range];
                let mut out = vec![0u8; HEADER_LEN + data.len()];
                let n = plan.header.write(data, &mut out).expect("fits");
                out.truncate(n);
                out
            })
            .collect()
    }

    /// A shell with real slots: applies verdicts and checks the slot
    /// invariants after every step.
    struct Shell {
        rx: Reassembler,
        slots: [[u8; CAP as usize]; SLOTS],
    }

    impl Shell {
        fn new() -> Self {
            Shell {
                rx: Reassembler::new(CAP),
                slots: [[0; CAP as usize]; SLOTS],
            }
        }

        fn feed(&mut self, msg: &[u8]) -> Verdict {
            let decoding = self.rx.decoding();
            let before = decoding.map(|f| self.slots[f.slot as usize]);
            let (rx, v) = self.rx.on_message(msg);
            if let Verdict::Store { slot, offset, .. } = v {
                let data = &msg[HEADER_LEN..];
                let at = offset as usize;
                self.slots[slot as usize][at..at + data.len()].copy_from_slice(data);
            }
            self.rx = rx;
            let mut roles = [rx.fill_slot, rx.ready_slot, rx.decode_slot];
            roles.sort_unstable();
            assert_eq!(roles, [0, 1, 2], "roles are a permutation");
            if let (Some(f), Some(b)) = (decoding, before) {
                assert_eq!(
                    self.slots[f.slot as usize], b,
                    "the decode slot is never written"
                );
                assert_eq!(rx.decode_slot, f.slot);
            }
            if let Some(r) = rx.ready() {
                assert_eq!(r.slot, rx.ready_slot);
            }
            v
        }

        fn take(&mut self) -> Option<Vec<u8>> {
            let (rx, f) = self.rx.take();
            self.rx = rx;
            f.map(|f| self.slots[f.slot as usize][..f.len as usize].to_vec())
        }
    }

    fn frame(seq: u16, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(7).wrapping_add(seq as u8))
            .collect()
    }

    #[test]
    fn in_order_frames_complete_and_are_taken_newest_first() {
        let mut s = Shell::new();
        let a = frame(1, 40);
        for m in chunks(1, &a, 16) {
            s.feed(&m);
        }
        assert_eq!(s.rx.ready().map(|f| (f.len, f.seq)), Some((40, 1)));
        assert_eq!(s.take(), Some(a));
        // While decoding, two more frames complete: the older is superseded.
        let b = frame(2, 30);
        let c = frame(3, 50);
        for m in chunks(2, &b, 16).iter().chain(&chunks(3, &c, 16)) {
            s.feed(m);
        }
        assert_eq!(s.take(), None, "still decoding");
        s.rx = s.rx.done();
        assert_eq!(s.take(), Some(c));
        let st = s.rx.stats();
        assert_eq!((st.received, st.superseded, st.lost), (3, 1, 0));
        s.rx = s.rx.done();
        assert_eq!(s.take(), None, "nothing ready");
    }

    #[test]
    fn a_gap_abandons_the_frame_and_its_rest_is_dropped() {
        let mut s = Shell::new();
        let a = frame(5, 48);
        let m = chunks(5, &a, 16);
        assert!(matches!(s.feed(&m[0]), Verdict::Store { offset: 0, .. }));
        assert_eq!(s.feed(&m[2]), Verdict::Drop(DropReason::Gap));
        assert_eq!(
            s.feed(&m[1]),
            Verdict::Drop(DropReason::Gap),
            "frame abandoned"
        );
        assert_eq!(s.rx.ready(), None);
        assert_eq!(s.rx.stats().lost, 1);
        // The next frame is received normally.
        for m in chunks(6, &frame(6, 20), 16) {
            s.feed(&m);
        }
        assert_eq!(s.take(), Some(frame(6, 20)));
    }

    #[test]
    fn a_new_first_chunk_abandons_the_frame_in_progress() {
        let mut s = Shell::new();
        let a = chunks(1, &frame(1, 48), 16);
        s.feed(&a[0]);
        s.feed(&a[1]);
        let b = frame(2, 32);
        for m in chunks(2, &b, 16) {
            s.feed(&m);
        }
        assert_eq!(s.rx.stats().lost, 1);
        assert_eq!(s.take(), Some(b));
    }

    #[test]
    fn duplicates_are_dropped_without_losing_the_frame() {
        let mut s = Shell::new();
        let a = frame(9, 48);
        let m = chunks(9, &a, 16);
        s.feed(&m[0]);
        s.feed(&m[1]);
        assert_eq!(s.feed(&m[1]), Verdict::Drop(DropReason::Duplicate));
        // A repeated first chunk restarts the same frame.
        assert!(matches!(s.feed(&m[0]), Verdict::Store { offset: 0, .. }));
        s.feed(&m[1]);
        let last = s.feed(&m[2]);
        assert!(matches!(
            last,
            Verdict::Store {
                completed: Some(_),
                ..
            }
        ));
        assert_eq!(s.take(), Some(a));
    }

    #[test]
    fn oversize_malformed_and_foreign_chunks() {
        let mut s = Shell::new();
        let big = frame(1, CAP as usize + 1);
        assert_eq!(
            s.feed(&chunks(1, &big, 16)[0]),
            Verdict::Drop(DropReason::TooLarge)
        );
        assert_eq!(s.feed(&[1, 2, 3]), Verdict::Drop(DropReason::Malformed));
        let mut bad = chunks(2, &frame(2, 8), 16)[0].clone();
        bad[0] = 9; // unknown version
        assert_eq!(s.feed(&bad), Verdict::Drop(DropReason::Malformed));
        // Another frame's middle chunk without its start.
        let mid = &chunks(3, &frame(3, 40), 16)[1];
        assert_eq!(s.feed(mid), Verdict::Drop(DropReason::Gap));
        let st = s.rx.stats();
        assert_eq!((st.lost, st.chunks_dropped, st.received), (1, 4, 0));
        // A same-seq chunk with a different length is not a continuation.
        let a = chunks(4, &frame(4, 40), 16);
        s.feed(&a[0]);
        let h = ChunkHeader {
            seq: 4,
            offset: 16,
            total: 41,
        };
        let mut other = vec![0u8; HEADER_LEN + 16];
        h.write(&[0; 16], &mut other).expect("fits");
        assert_eq!(s.feed(&other), Verdict::Drop(DropReason::Gap));
    }

    #[test]
    fn single_chunk_frames_and_counters_saturate() {
        let mut s = Shell::new();
        let a = frame(1, 10);
        let v = s.feed(&chunks(1, &a, 16)[0]);
        assert!(matches!(
            v,
            Verdict::Store {
                completed: Some(Frame {
                    len: 10,
                    seq: 1,
                    ..
                }),
                ..
            }
        ));
        let mut rx = s.rx;
        rx.stats = RxStats {
            received: u32::MAX,
            lost: u32::MAX,
            superseded: u32::MAX,
            chunks_dropped: u32::MAX,
        };
        let (rx, _) = rx.on_message(&chunks(2, &a, 16)[0]);
        let (rx, _) = rx.on_message(&[0]);
        assert_eq!(rx.stats().received, u32::MAX);
        assert_eq!(rx.stats().superseded, u32::MAX);
        assert_eq!(rx.stats().chunks_dropped, u32::MAX);
    }

    /// A seeded xorshift generator for the shuffled-delivery test.
    fn rng(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    #[test]
    fn lossy_reordered_delivery_only_ever_yields_whole_frames() {
        let mut seed = 0x5EED_u64;
        let mut s = Shell::new();
        let frames: Vec<Vec<u8>> = (0..200u16)
            .map(|i| frame(i, 1 + (rng(&mut seed) % u64::from(CAP)) as usize))
            .collect();
        let mut taken = 0;
        for (i, f) in frames.iter().enumerate() {
            let mut msgs = chunks(i as u16, f, 1 + (rng(&mut seed) % 20) as usize);
            match rng(&mut seed) % 4 {
                0 if msgs.len() > 1 => {
                    let k = (rng(&mut seed) as usize) % msgs.len();
                    msgs.remove(k);
                }
                1 if msgs.len() > 1 => {
                    let k = (rng(&mut seed) as usize) % (msgs.len() - 1);
                    msgs.swap(k, k + 1);
                }
                2 => {
                    let k = (rng(&mut seed) as usize) % msgs.len();
                    let dup = msgs[k].clone();
                    msgs.insert(k, dup);
                }
                _ => {}
            }
            for m in &msgs {
                s.feed(m);
                if rng(&mut seed).is_multiple_of(8) {
                    s.rx = s.rx.done();
                    if let Some(got) = s.take() {
                        assert!(frames.contains(&got), "a taken frame is a whole sent frame");
                        taken += 1;
                    }
                }
            }
        }
        assert!(taken > 20, "{taken}");
        let st = s.rx.stats();
        assert!(
            st.lost > 0 && st.received > 0 && st.chunks_dropped > 0,
            "{st:?}"
        );
    }
}
