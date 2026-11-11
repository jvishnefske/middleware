//! Publish/subscribe: [`Publisher`], [`Subscriber`], runtime [`Subscribe`],
//! the [`Split`] of one link into both halves, and what each backend
//! promises about delivery ([`Limits`], [`Delivery`]).
//!
//! # Delivery
//!
//! Every subscriber declares [`Subscriber::DELIVERY`]: what happens when
//! samples arrive faster than the application calls `recv`
//! ([`Overflow`]), and whether samples from one sender arrive in the order
//! they were published. The declaration is a promise checked by the
//! conformance tests (`tests/conformance/pubsub.rs`); when samples are
//! dropped and the backend can tell, the next delivered sample says how many
//! in [`SampleMeta::gap`].

use crate::key::{Key, KeyFilter};

use super::meta::SampleMeta;

/// A message received by a [`Subscriber`]. `key` and `payload` borrow the
/// buffer passed to [`Subscriber::recv`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample<'a> {
    /// The key the message was published on.
    pub key: Key<'a>,
    /// The raw payload.
    pub payload: &'a [u8],
    /// Sender, sequence and gap, when known.
    pub meta: SampleMeta,
}

impl<'a> Sample<'a> {
    /// A sample with no sender information.
    pub const fn new(key: Key<'a>, payload: &'a [u8]) -> Self {
        Sample {
            key,
            payload,
            meta: SampleMeta::NONE,
        }
    }

    /// The same sample with `meta` attached.
    pub const fn with_meta(self, meta: SampleMeta) -> Self {
        Sample { meta, ..self }
    }
}

/// Size limits of a transport.
///
/// `max_key` bounds the key and `max_payload` is the largest payload that
/// fits with **any** allowed key, so it is always safe for sizing buffers.
/// When key and payload share one frame ([`Limits::shared`]), a shorter key
/// leaves room for a longer payload, up to `max_message` bytes together; use
/// [`Limits::fits`] to check an actual message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Longest key, in bytes.
    pub max_key: usize,
    /// Largest payload that fits with any allowed key.
    pub max_payload: usize,
    /// Largest key length plus payload length together.
    pub max_message: usize,
    /// True if key and payload share `max_message` (a shorter key frees
    /// payload room); false if the payload cap is independent of the key.
    pub shared: bool,
}

impl Limits {
    /// No transport-specific limit beyond [`crate::key::MAX_KEY_LEN`].
    pub const UNBOUNDED: Limits = Limits::new(crate::key::MAX_KEY_LEN, usize::MAX);

    /// Independent limits: the key does not use payload room (e.g. it is
    /// hashed into a header or a UUID).
    pub const fn new(max_key: usize, max_payload: usize) -> Limits {
        Limits {
            max_key,
            max_payload,
            max_message: max_key.saturating_add(max_payload),
            shared: false,
        }
    }

    /// Key and payload share `max_message` bytes; `max_payload` is what is
    /// left with the longest key.
    pub const fn shared(max_key: usize, max_message: usize) -> Limits {
        Limits {
            max_key,
            max_payload: max_message.saturating_sub(max_key),
            max_message,
            shared: true,
        }
    }

    /// True if a message with these key and payload lengths fits.
    pub const fn fits(&self, key_len: usize, payload_len: usize) -> bool {
        if key_len > self.max_key {
            return false;
        }
        if self.shared {
            payload_len <= self.max_message.saturating_sub(key_len)
        } else {
            payload_len <= self.max_payload
        }
    }
}

/// What a subscriber's queue does when it is full and another sample
/// arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overflow {
    /// The arriving sample is dropped; queued ones are kept. Radios with a
    /// fixed receive queue behave like this.
    DropNewest,
    /// The oldest queued sample is dropped to make room: the subscriber
    /// always sees the most recent samples.
    DropOldest,
    /// Nothing is dropped: the publisher (or the link) waits until the
    /// subscriber makes room (flow control end to end, e.g. a byte stream
    /// that stops being read).
    Block,
}

/// The delivery guarantee a [`Subscriber`] declares as
/// [`Subscriber::DELIVERY`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delivery {
    /// Behaviour of a full queue.
    pub overflow: Overflow,
    /// True if the samples of any one sender are delivered in the order
    /// they were published (those not dropped). Nothing is promised about
    /// the interleaving of different senders.
    pub per_sender_order: bool,
}

impl Delivery {
    /// A declaration.
    pub const fn new(overflow: Overflow, per_sender_order: bool) -> Self {
        Delivery {
            overflow,
            per_sender_order,
        }
    }
}

/// Fire-and-forget delivery of a payload to everyone subscribed to a key.
pub trait Publisher {
    /// Transport-specific failure.
    type Error: core::fmt::Debug;

    /// What this transport can carry.
    const LIMITS: Limits = Limits::UNBOUNDED;

    /// Sends `payload` on `key`.
    async fn publish(&mut self, key: Key<'_>, payload: &[u8]) -> Result<(), Self::Error>;
}

/// Receives payloads published on the key(s) this subscriber was created for.
///
/// Both constants are required, so every backend states what it delivers.
pub trait Subscriber {
    /// Transport-specific failure.
    type Error: core::fmt::Debug;

    /// Behaviour under overload and ordering (see the module docs).
    const DELIVERY: Delivery;

    /// The largest key and payload a received sample can have; size the
    /// `recv` buffer from it (`max_message`, plus any framing the backend
    /// documents).
    const LIMITS: Limits;

    /// Waits for the next sample and writes its key and payload into `buf`.
    /// Cancel-safe (see [`crate::transport`]).
    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, Self::Error>;

    /// Returns the next sample if one is already available, without waiting.
    /// For tick-driven callers that cannot await.
    fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, Self::Error>;
}

/// Creates subscriptions at runtime.
///
/// The subscriber may borrow the transport (`Subscriber<'a>` with
/// `Self: 'a`): a single-stream transport (a serial link, one radio) hands
/// out a filtered view of itself, valid while it is borrowed, while
/// multiplexing transports (Zenoh, gRPC bus, in-process buses) hand out
/// independent owned subscribers and ignore the lifetime.
pub trait Subscribe {
    /// The subscriber handed out, possibly borrowing `self`.
    type Subscriber<'a>: Subscriber
    where
        Self: 'a;
    /// Transport-specific failure.
    type Error: core::fmt::Debug;

    /// Starts receiving samples whose key matches `filter`. The filter is
    /// copied if the subscriber needs to keep it.
    async fn subscribe<'a>(
        &'a mut self,
        filter: KeyFilter<'_>,
    ) -> Result<Self::Subscriber<'a>, Self::Error>;
}

/// Splits one link into a publishing half and a receiving half that can be
/// used at the same time (one task sends while another waits in `recv`),
/// each borrowing the link.
///
/// For single-stream transports (a UART, one radio) whose `Publisher` and
/// `Subscriber` impls would otherwise both need `&mut self`. The halves
/// share the link's state through the borrow, not through shared
/// ownership.
pub trait Split {
    /// The publishing half.
    type Tx<'a>: Publisher
    where
        Self: 'a;
    /// The receiving half.
    type Rx<'a>: Subscriber
    where
        Self: 'a;

    /// Borrows the link as its two halves.
    fn split(&mut self) -> (Self::Tx<'_>, Self::Rx<'_>);
}
