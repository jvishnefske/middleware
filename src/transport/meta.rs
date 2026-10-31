//! Who a message came from and what it may cost: [`SenderId`],
//! [`SampleMeta`] for published samples and [`RequestMeta`] for requests.
//!
//! All three are plain `Copy` values filled in by the transport at the
//! boundary. A field the transport cannot know is `None`, never a guess.

/// Identifies the sender of a sample as far as the transport knows it (a MAC
/// address, a Zenoh session id, ...), zero-padded to 8 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SenderId(pub [u8; 8]);

impl SenderId {
    /// The id of a node with this 48-bit hardware address.
    pub const fn from_mac(mac: [u8; 6]) -> Self {
        SenderId([mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], 0, 0])
    }
}

/// What the transport knows about where a sample came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SampleMeta {
    /// The sender, if the transport identifies senders.
    pub sender: Option<SenderId>,
    /// The sender's sequence number for this message, if the transport
    /// carries one. Deliberately 8 bits: it is what the radio frames carry
    /// (one byte per frame matters at 31-byte advertising payloads), and
    /// it only has to distinguish repeats inside a short dedup window
    /// ([`crate::dedup`]), not order a long history. It wraps after 255;
    /// compare with wrapping arithmetic, never with `<`.
    pub seq: Option<u8>,
    /// How many samples from this sender are known to have been lost
    /// immediately before this one: `Some(0)` means none, `Some(n)` means
    /// `n` were skipped (by the transport's queue overflowing, or on the
    /// medium as told by a sequence jump). `None` means the transport does
    /// not know: it reports no gaps, or this is the first sample it has seen
    /// from the sender, or the sequence went backwards (a restart). Backends
    /// with 8-bit sequences compute it with
    /// [`SeenTable::check`](crate::dedup::SeenTable::check).
    pub gap: Option<u16>,
}

impl SampleMeta {
    /// Nothing known.
    pub const NONE: SampleMeta = SampleMeta {
        sender: None,
        seq: None,
        gap: None,
    };

    /// Metadata naming `sender` only.
    pub const fn from_sender(sender: SenderId) -> Self {
        SampleMeta {
            sender: Some(sender),
            ..Self::NONE
        }
    }

    /// The same metadata with sequence number `seq`.
    pub const fn with_seq(self, seq: u8) -> Self {
        SampleMeta {
            seq: Some(seq),
            ..self
        }
    }

    /// The same metadata with `gap` (see [`SampleMeta::gap`]).
    pub const fn with_gap(self, gap: Option<u16>) -> Self {
        SampleMeta { gap, ..self }
    }
}

/// What the transport knows about a request when it reaches a
/// [`Handler`](crate::transport::Handler).
///
/// * `sender` is the requesting node as the transport identifies it
///   (the same id it would put in [`SampleMeta::sender`]).
/// * `principal` is an **authenticated** name for the caller: a SPIFFE URI
///   from an Istio peer certificate (`spiffe://cluster.local/ns/x/sa/y`), a
///   gRPC client identity, or another name the transport verified. A
///   transport that cannot authenticate the caller leaves it `None`; a
///   handler making policy decisions treats `None` as anonymous.
/// * `deadline_ms` is the time the caller still allows for the reply,
///   in milliseconds, **measured when the transport handed the request to
///   the handler** (a relative budget: the nodes share no clock). `Some(0)`
///   means already expired. A handler that cannot finish in time returns
///   [`HandlerError::DeadlineExceeded`](crate::transport::HandlerError::DeadlineExceeded).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RequestMeta<'a> {
    /// The requesting node, if the transport identifies it.
    pub sender: Option<SenderId>,
    /// The authenticated caller, if the transport authenticated one.
    pub principal: Option<&'a str>,
    /// Remaining budget for the reply in milliseconds, if the caller set one.
    pub deadline_ms: Option<u32>,
}

impl<'a> RequestMeta<'a> {
    /// Nothing known.
    pub const NONE: Self = RequestMeta {
        sender: None,
        principal: None,
        deadline_ms: None,
    };

    /// The same metadata naming `sender`.
    pub const fn with_sender(self, sender: SenderId) -> Self {
        RequestMeta {
            sender: Some(sender),
            ..self
        }
    }

    /// The same metadata with an authenticated `principal`.
    pub const fn with_principal(self, principal: &'a str) -> Self {
        RequestMeta {
            principal: Some(principal),
            ..self
        }
    }

    /// The same metadata with `deadline_ms` milliseconds left.
    pub const fn with_deadline_ms(self, deadline_ms: u32) -> Self {
        RequestMeta {
            deadline_ms: Some(deadline_ms),
            ..self
        }
    }
}
