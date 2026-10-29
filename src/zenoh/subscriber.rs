//! [`ZenohSubscriber`]: samples of a Zenoh subscriber, with sender,
//! sequence and exact per-stream gaps.
//!
//! # Delivery: `Block`, per-sender order
//!
//! The subscriber's samples go into a bounded FIFO
//! ([`ZenohTransport::queue_depth`](super::ZenohTransport::queue_depth)).
//! Zenoh's FIFO handler **blocks** when it is full instead of dropping: the
//! Zenoh thread delivering the sample waits until [`Subscriber::recv`] or
//! [`Subscriber::try_recv`] makes room. For a remote publisher that thread
//! reads the link, so the link stops being read, TCP flow control fills the
//! publisher's transmission queue, and because the backend publishes with
//! congestion control Block the publisher's `put` then waits too. Nothing
//! is dropped: [`Overflow::Block`]. Samples of one publisher travel one
//! link in order and leave the FIFO in order, so per-sender order holds.
//! Evidence: `tests/zenoh.rs` runs the conformance case
//! `overload_matches_declared_delivery` over two loopback sessions with a
//! FIFO of 4; the 7 samples published while nobody receives all arrive,
//! in order, once the subscriber drains.
//!
//! Caveats, all Zenoh's:
//!
//! * A blocked FIFO stalls that whole link: every other subscriber and
//!   queryable of the receiving session stops receiving from that peer
//!   until it drains (head-of-line blocking).
//! * Zenoh closes a link whose transmission queue stays blocked longer than
//!   `queue/congestion_control/block/wait_before_close` (5 s by default),
//!   after which the samples queued on it are lost; a subscriber stalled
//!   that long is disconnected rather than silently thinned.
//! * Publications from other Zenoh clients that use congestion control
//!   Drop (Zenoh's default for `put`) may be dropped at their own queue;
//!   samples with our attachment then show the loss in [`SampleMeta::gap`].
//!
//! # Gaps
//!
//! Each publication attachment numbers a stream (one key published by one
//! transport value, [`wire`]). The subscriber keeps a
//! [`SeenTable`] of [`SEEN_STREAMS`] (sender, stream) pairs and reports
//! [`SeqCheck::gap`](crate::dedup::SeqCheck::gap): `Some(0)` for the next
//! sample of a stream, `Some(n)` after `n` lost, `None` for the first
//! sample of a stream (or one forgotten when the table was full), a
//! repeat, a sequence going backwards (a restarted publisher), samples
//! from 0.4 publishers (no stream) and samples without our attachment. A
//! sample is recorded before the buffer check, so one refused with
//! [`ZenohError::BufferTooSmall`] does not count as lost for the next.

use ::zenoh::handlers::FifoChannelHandler;

use crate::dedup::SeenTable;
use crate::key::{Key, MAX_KEY_LEN};
use crate::transport::{Delivery, Limits, Overflow, Sample, SampleMeta, Subscriber, sample_into};

use super::{MAX_PAYLOAD, ZenohError, wire};

/// (sender, stream) pairs a [`ZenohSubscriber`] remembers for gaps.
pub const SEEN_STREAMS: usize = 32;

/// Receives samples matching the filter it was declared for.
#[derive(Debug)]
pub struct ZenohSubscriber {
    inner: ::zenoh::pubsub::Subscriber<FifoChannelHandler<::zenoh::sample::Sample>>,
    seen: SeenTable<SEEN_STREAMS>,
}

impl ZenohSubscriber {
    pub(super) fn new(
        inner: ::zenoh::pubsub::Subscriber<FifoChannelHandler<::zenoh::sample::Sample>>,
    ) -> Self {
        Self {
            inner,
            seen: SeenTable::new(),
        }
    }
}

/// The metadata of a sample with `attachment`, and the table after
/// recording it. Pure.
pub fn sample_meta(
    seen: SeenTable<SEEN_STREAMS>,
    attachment: Option<&[u8]>,
) -> (SeenTable<SEEN_STREAMS>, SampleMeta) {
    let Some(header) = attachment.and_then(wire::decode_pub) else {
        return (seen, SampleMeta::NONE);
    };
    match header.stream {
        Some(stream) => {
            let (seen, check) = seen.check_stream(header.sender, stream, header.seq);
            (seen, header.meta().with_gap(check.gap()))
        }
        None => (seen, header.meta()),
    }
}

/// Copies a received Zenoh sample into `buf` as a [`Sample`] with `meta`.
fn into_sample<'b>(
    sample: &::zenoh::sample::Sample,
    meta: SampleMeta,
    buf: &'b mut [u8],
) -> Result<Sample<'b>, ZenohError> {
    let key = sample.key_expr().as_str();
    let payload = sample.payload().to_bytes();
    let needed = key.len() + payload.len();
    if needed > buf.len() {
        return Err(ZenohError::BufferTooSmall {
            needed,
            capacity: buf.len(),
        });
    }
    Key::new(key).map_err(ZenohError::InvalidKey)?;
    // Size and key validity were checked above, so `None` is unreachable;
    // it is still mapped to an error rather than unwrapped.
    sample_into(buf, key, &payload)
        .map(|s| s.with_meta(meta))
        .ok_or(ZenohError::BufferTooSmall {
            needed,
            capacity: needed,
        })
}

impl ZenohSubscriber {
    fn deliver<'b>(
        &mut self,
        sample: &::zenoh::sample::Sample,
        buf: &'b mut [u8],
    ) -> Result<Sample<'b>, ZenohError> {
        let attachment = sample.attachment().map(|a| a.to_bytes());
        let (seen, meta) = sample_meta(self.seen, attachment.as_deref());
        self.seen = seen;
        into_sample(sample, meta, buf)
    }
}

impl Subscriber for ZenohSubscriber {
    type Error = ZenohError;

    /// `Block` with per-sender order; see the module docs for why and for
    /// Zenoh's caveats.
    const DELIVERY: Delivery = Delivery::new(Overflow::Block, true);
    const LIMITS: Limits = Limits::new(MAX_KEY_LEN, MAX_PAYLOAD);

    /// Waits on the subscriber's FIFO channel. Cancel safe: a dropped wait
    /// leaves queued samples in the channel.
    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, ZenohError> {
        let sample = self
            .inner
            .recv_async()
            .await
            .map_err(|_| ZenohError::Closed)?;
        self.deliver(&sample, buf)
    }

    /// Dequeues a sample if one is queued; `Ok(None)` when the channel is
    /// empty, [`ZenohError::Closed`] when it is empty and closed.
    fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, ZenohError> {
        match self.inner.try_recv() {
            Ok(Some(sample)) => self.deliver(&sample, buf).map(Some),
            Ok(None) => Ok(None),
            Err(_) => Err(ZenohError::Closed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dedup::Sequence;
    use crate::transport::SenderId;

    const A: SenderId = SenderId([1; 8]);
    const B: SenderId = SenderId([2; 8]);

    fn meta_of(
        seen: SeenTable<SEEN_STREAMS>,
        sender: SenderId,
        seq: u8,
        stream: u32,
    ) -> (SeenTable<SEEN_STREAMS>, SampleMeta) {
        sample_meta(
            seen,
            Some(&wire::encode_pub(sender, Sequence::new(seq), stream)),
        )
    }

    #[test]
    fn gaps_are_exact_per_sender_and_stream() {
        let t = SeenTable::new();
        let (t, m) = meta_of(t, A, 0, 7);
        assert_eq!(m, SampleMeta::from_sender(A).with_seq(0));
        let (t, m) = meta_of(t, A, 1, 7);
        assert_eq!(m.gap, Some(0));
        // Another stream of A and the same stream id from B are independent.
        let (t, m) = meta_of(t, A, 0, 8);
        assert_eq!(m.gap, None);
        let (t, m) = meta_of(t, B, 5, 7);
        assert_eq!(m.gap, None);
        let (t, m) = meta_of(t, A, 4, 7);
        assert_eq!((m.seq, m.gap), (Some(4), Some(2)));
        // A restarted publisher (sequence backwards) has an unknown gap.
        let (_, m) = meta_of(t, A, 0, 7);
        assert_eq!(m.gap, None);
    }

    #[test]
    fn foreign_and_v1_attachments_report_no_gap() {
        let t = SeenTable::new();
        assert_eq!(sample_meta(t, None).1, SampleMeta::NONE);
        assert_eq!(sample_meta(t, Some(b"other")).1, SampleMeta::NONE);
        let mut v1 = [0u8; 12];
        v1[..3].copy_from_slice(b"mw\x01");
        v1[3..11].copy_from_slice(&A.0);
        v1[11] = 3;
        let (t, m) = sample_meta(t, Some(&v1));
        assert_eq!(m, SampleMeta::from_sender(A).with_seq(3));
        assert_eq!(sample_meta(t, Some(&v1)).1.gap, None);
    }

    #[test]
    fn delivery_is_declared_block_in_order() {
        assert_eq!(
            <ZenohSubscriber as Subscriber>::DELIVERY,
            Delivery::new(Overflow::Block, true)
        );
    }
}
