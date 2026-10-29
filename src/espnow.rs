//! ESP-NOW broadcast transport (`espnow` feature, `no_std`, no heap).
//!
//! # Frame format (version 1)
//!
//! Every publication is one broadcast ESP-NOW frame:
//!
//! | bytes | field                                                     |
//! |-------|-----------------------------------------------------------|
//! | 2     | magic [`MAGIC`] (`MW`)                                    |
//! | 1     | version [`VERSION`] (1)                                   |
//! | 2     | [`short_id`] of the key, u16 little-endian                |
//! | 1     | the sender's sequence number (wrapping)                   |
//! | 1     | key length `n` (1..=[`MAX_KEY`])                          |
//! | n     | key bytes (a valid middleware [`Key`])                    |
//! | rest  | payload (0..=[`MAX_PAYLOAD`] bytes on publish)            |
//!
//! The fixed header is [`HEADER_LEN`] (7) bytes and the key is carried in
//! full, because 16-bit short ids can collide and a receiver needs the key
//! text to build a [`Sample`]. The short id is still sent as a cheap
//! integrity check and keeps the header aligned with the BLE advertising
//! backend (it equals `ble::uuid::short_id`). Keys are limited to
//! [`MAX_KEY`] bytes so that every payload up to [`MAX_PAYLOAD`] fits one
//! ESP-NOW v1 frame ([`MAX_FRAME`] = 250 bytes) whatever the key; the
//! limits are strict, so what fits never depends on the key length. They
//! are advertised as [`Publisher::LIMITS`] ([`LIMITS`]).
//!
//! # Transport
//!
//! [`EspNowTransport`] owns an [`EspNowRadio`] and implements
//! [`Publisher`], [`Subscriber`] and [`Subscribe`]:
//!
//! - **Publish** encodes a frame carrying the transport's [`Sequence`] and
//!   broadcasts it. The sequence is per sender, shared by all keys, and
//!   advances as soon as the frame is encoded, before the radio is awaited:
//!   a broadcast that fails or is cancelled consumes its number, so a later
//!   message is never mistaken by receivers for a repeat of it.
//! - **Filters**: the transport receives keys matching any of up to `F`
//!   [`KeyFilter`]s ([`KeyFilter::Exact`] or segment-wise
//!   [`KeyFilter::Prefix`]), set with [`EspNowTransport::with_filter`] and
//!   changed at runtime with [`EspNowTransport::add_filter`] /
//!   [`EspNowTransport::remove_filter`]. With no filter nothing is received.
//! - **Subscribe**: [`Subscribe::subscribe`] borrows the transport as an
//!   [`EspNowSubscriber`] that owns a copy of one filter (a
//!   [`KeyFilterBuf`] of [`MAX_KEY`] bytes, so the caller's key need not
//!   outlive it) and receives exactly the keys it selects, *instead of* the
//!   transport's filter slots. Own-address skipping, dedup and the kept
//!   sample work as for the transport itself; the dedup table is shared, so
//!   a frame delivered through one is a repeat for the other. One radio
//!   is one stream: the view holds `&mut` the transport, so there is one
//!   subscriber at a time; drop it to publish. A filter on a key longer than
//!   [`MAX_KEY`] could never match a frame and is refused with
//!   [`FrameError::KeyTooLong`].
//! - **Dedupe**: a [`SeenTable`] of `K` entries remembers the last sequence
//!   number per (sender MAC, key); a frame repeating it is dropped. A full
//!   table replaces its least recently updated entry. A sender that
//!   restarts may lose one frame if its first sequence number equals the
//!   last one remembered for it.
//! - **Sender**: every [`Sample`] carries [`SampleMeta`] with the sender
//!   ([`SenderId::from_mac`]) and the frame's sequence number.
//! - **Ignored frames**: frames without the `MW` magic (other ESP-NOW
//!   users), from our own address ([`EspNowTransport::with_own_address`]),
//!   on keys no filter selects, or repeats are skipped. After
//!   [`MAX_IGNORED_FRAMES`] of them in one call, [`Subscriber::recv`] and
//!   [`Subscriber::try_recv`] return [`EspNowError::IgnoredLimit`], so every
//!   call is bounded in frames.
//! - **Errors**: a frame with the `MW` magic that does not decode is
//!   [`EspNowError::Frame`] (so the caller can count it); radio failures
//!   are [`EspNowError::Radio`]; an oversized key or payload on publish is
//!   [`EspNowError::Frame`] with [`FrameError::KeyTooLong`] or
//!   [`FrameError::PayloadTooLarge`]. A sample that does not fit the
//!   caller's buffer is [`EspNowError::BufferTooSmall`] and is **kept**
//!   (see below): the next `recv` or `try_recv` whose filters select it
//!   returns it first. After any error the transport is still usable.
//!
//! # The kept frame (`P`)
//!
//! Keeping a too-large sample costs a frame buffer of `P` bytes inside the
//! transport. The default `P` = [`MAX_FRAME`] keeps every frame (the
//! behaviour above). A receiver that always passes a buffer of at least
//! [`MAX_FRAME`] bytes never sees [`EspNowError::BufferTooSmall`] and can set
//! `P = 0`, which shrinks the kept slot from 258 to 8 bytes (the transport
//! by about [`MAX_FRAME`] bytes, depending on field layout); the trade-off is that with `P = 0` (or any `P` shorter than the
//! frame) a sample that does not fit is reported as
//! [`EspNowError::BufferTooSmall`] and **dropped**, so a retry with a larger
//! buffer does not get it back. `P` above [`MAX_FRAME`] is a compile error.
//!
//! There is one kept slot. A receive whose filters do not select the kept
//! sample leaves it in place and goes on to the radio; if that receive must
//! keep a sample of its own, it replaces (drops) the earlier one.
//!
//! # Cancel safety
//!
//! [`Subscriber::recv`] awaits only inside [`EspNowRadio::receive`], whose
//! contract is that dropping it consumes no frame. Every frame it does
//! return is filtered, deduplicated and either delivered or stashed before
//! the next await, so a dropped `recv` loses no sample: the next `recv` or
//! `try_recv` returns it. [`Publisher::publish`] dropped while
//! broadcasting may or may not have sent its frame and never sends it again
//! (see the sequence rule above).
//!
//! Everything here is pure except the radio calls; the transport is tested
//! on the host with an in-memory radio.

use crate::dedup::{SeenTable, Sequence};
use crate::key::{Key, KeyFilter, KeyFilterBuf};
use crate::transport::{
    Delivery, Limits, Overflow, Publisher, Sample, SampleMeta, SenderId, Subscribe, Subscriber,
    sample_into,
};

/// First two bytes of every middleware ESP-NOW frame.
pub const MAGIC: [u8; 2] = *b"MW";
/// Frame format version.
pub const VERSION: u8 = 1;
/// Fixed header: magic, version, short id, sequence, key length.
pub const HEADER_LEN: usize = 7;
/// Largest ESP-NOW v1 frame.
pub const MAX_FRAME: usize = 250;
/// Longest key this transport carries.
pub const MAX_KEY: usize = 64;
/// Largest payload this transport carries (fits with a [`MAX_KEY`] key).
pub const MAX_PAYLOAD: usize = MAX_FRAME - HEADER_LEN - MAX_KEY;
/// Frames skipped in one `recv` or `try_recv` before it gives up.
pub const MAX_IGNORED_FRAMES: usize = 256;
/// What [`EspNowTransport`] carries, as its [`Publisher::LIMITS`].
pub const LIMITS: Limits = Limits::shared(MAX_KEY, MAX_FRAME - HEADER_LEN);
/// What [`EspNowTransport`] subscribers deliver, as their
/// [`Subscriber::DELIVERY`]: frames wait in the radio binding's bounded
/// receive queue, which drops arriving frames when full; one sender's
/// frames arrive in the order sent.
pub const DELIVERY: Delivery = Delivery::new(Overflow::DropNewest, true);

/// A 48-bit ESP-NOW (Wi-Fi MAC) address.
pub type Mac = [u8; 6];

/// 16-bit id of a key carried in the header: the 64-bit FNV-1a of the key
/// folded to 16 bits, the same value as the BLE backend's
/// `ble::uuid::short_id`.
pub const fn short_id(key: Key<'_>) -> u16 {
    crate::dedup::short_id(key)
}

/// Why bytes are not a valid frame, or a publication cannot be framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// Shorter than the header plus the declared key.
    TooShort,
    /// Longer than [`MAX_FRAME`].
    TooLong,
    /// The first two bytes are not [`MAGIC`].
    BadMagic,
    /// A version this code does not understand.
    BadVersion(u8),
    /// Key length 0 or above [`MAX_KEY`] in a received frame.
    BadKeyLength,
    /// The key bytes are not a valid middleware key.
    BadKey,
    /// The short id does not match the key.
    IdMismatch,
    /// Publishing a key longer than [`MAX_KEY`].
    KeyTooLong,
    /// Publishing a payload longer than [`MAX_PAYLOAD`].
    PayloadTooLarge,
}

/// One decoded frame; `key` and `payload` borrow the frame bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame<'a> {
    /// The sender's sequence number.
    pub seq: u8,
    /// The key the payload was published on.
    pub key: Key<'a>,
    /// The payload.
    pub payload: &'a [u8],
}

impl<'a> Frame<'a> {
    /// Writes this frame into `out` and returns its length.
    ///
    /// # Errors
    ///
    /// [`FrameError::KeyTooLong`] or [`FrameError::PayloadTooLarge`] when the
    /// key or payload exceed [`MAX_KEY`] / [`MAX_PAYLOAD`].
    pub fn encode(&self, out: &mut [u8; MAX_FRAME]) -> Result<usize, FrameError> {
        let key = self.key.as_bytes();
        if key.len() > MAX_KEY {
            return Err(FrameError::KeyTooLong);
        }
        if self.payload.len() > MAX_PAYLOAD {
            return Err(FrameError::PayloadTooLarge);
        }
        let key_len = u8::try_from(key.len()).map_err(|_| FrameError::KeyTooLong)?;
        let id = short_id(self.key).to_le_bytes();
        let header = [MAGIC[0], MAGIC[1], VERSION, id[0], id[1], self.seq, key_len];
        let n = HEADER_LEN + key.len() + self.payload.len();
        let slot = out.get_mut(..n).ok_or(FrameError::PayloadTooLarge)?;
        let (head, rest) = slot.split_at_mut(HEADER_LEN);
        let (key_slot, payload_slot) = rest.split_at_mut(key.len());
        head.copy_from_slice(&header);
        key_slot.copy_from_slice(key);
        payload_slot.copy_from_slice(self.payload);
        Ok(n)
    }

    /// Parses a received frame. The payload may be longer than
    /// [`MAX_PAYLOAD`] when the key is short, as long as the frame fits
    /// [`MAX_FRAME`].
    ///
    /// # Errors
    ///
    /// Any [`FrameError`] other than the publish-only `KeyTooLong` and
    /// `PayloadTooLarge`.
    pub fn decode(bytes: &'a [u8]) -> Result<Frame<'a>, FrameError> {
        if bytes.len() > MAX_FRAME {
            return Err(FrameError::TooLong);
        }
        let [m0, m1, rest @ ..] = bytes else {
            return Err(FrameError::TooShort);
        };
        if [*m0, *m1] != MAGIC {
            return Err(FrameError::BadMagic);
        }
        let [version, id_lo, id_hi, seq, key_len, body @ ..] = rest else {
            return Err(FrameError::TooShort);
        };
        if *version != VERSION {
            return Err(FrameError::BadVersion(*version));
        }
        let key_len = usize::from(*key_len);
        if key_len == 0 || key_len > MAX_KEY {
            return Err(FrameError::BadKeyLength);
        }
        let (key, payload) = body.split_at_checked(key_len).ok_or(FrameError::TooShort)?;
        let key = core::str::from_utf8(key).map_err(|_| FrameError::BadKey)?;
        let key = Key::new(key).map_err(|_| FrameError::BadKey)?;
        if short_id(key) != u16::from_le_bytes([*id_lo, *id_hi]) {
            return Err(FrameError::IdMismatch);
        }
        Ok(Frame {
            seq: *seq,
            key,
            payload,
        })
    }
}

/// Radio side of the transport: broadcast frames and receive them.
///
/// Implementations copy a received frame into `buf` (the transport offers
/// [`MAX_FRAME`] bytes) and report one that does not fit as their own
/// error, never truncating.
pub trait EspNowRadio {
    /// Binding-specific failure.
    type Error: core::fmt::Debug;

    /// Broadcasts one frame to every listening station.
    async fn broadcast(&mut self, frame: &[u8]) -> Result<(), Self::Error>;

    /// Waits for the next frame, copies it into `buf` and returns it with
    /// the sender's MAC.
    ///
    /// Must be cancel-safe: dropping the future before it is ready consumes
    /// no frame (an embassy channel `receive` has this property).
    async fn receive<'b>(&mut self, buf: &'b mut [u8]) -> Result<(&'b [u8], Mac), Self::Error>;

    /// Returns the next frame if one is already queued, without waiting;
    /// `Ok(None)` when none is.
    fn try_receive<'b>(
        &mut self,
        buf: &'b mut [u8],
    ) -> Result<Option<(&'b [u8], Mac)>, Self::Error>;
}

/// Failure of the ESP-NOW transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EspNowError<E> {
    /// The radio failed.
    Radio(E),
    /// A received `MW` frame was malformed, or a publication cannot be
    /// framed (key or payload too large).
    Frame(FrameError),
    /// The caller's buffer cannot hold the sample's key and payload. The
    /// sample is kept and returned by the next receive call.
    BufferTooSmall,
    /// [`MAX_IGNORED_FRAMES`] frames passed without a matching one.
    IgnoredLimit,
}

/// All `F` filter slots of an [`EspNowTransport`] are in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FiltersFull;

/// A frame accepted for delivery that did not fit the caller's buffer,
/// kept in a buffer of `P` bytes (`len <= P <= MAX_FRAME`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pending<const P: usize> {
    frame: [u8; P],
    len: u8,
    src: Mac,
}

impl<const P: usize> Pending<P> {
    /// Copies `bytes`, or `None` when they do not fit `P` (or a `u8`).
    fn keep(bytes: &[u8], src: Mac) -> Option<Self> {
        let len = u8::try_from(bytes.len()).ok()?;
        let mut frame = [0u8; P];
        frame.get_mut(..bytes.len())?.copy_from_slice(bytes);
        Some(Pending { frame, len, src })
    }

    fn bytes(&self) -> &[u8] {
        self.frame.get(..usize::from(self.len)).unwrap_or(&[])
    }
}

/// Which keys one receive call accepts.
#[derive(Clone, Copy)]
enum Selection<'f> {
    /// The transport's filter slots.
    Slots,
    /// One subscriber's filter, instead of the slots.
    Only(KeyFilter<'f>),
}

/// Middleware publisher and subscriber over ESP-NOW broadcast, remembering
/// `K` (sender, key) pairs for duplicate suppression and holding up to `F`
/// key filters. All state is inline (no heap): the radio, `F` filter slots,
/// a `K`-entry [`SeenTable`] and one kept frame of `P` bytes (default
/// [`MAX_FRAME`]; `P = 0` disables keeping, see the module docs).
pub struct EspNowTransport<
    'a,
    R,
    const K: usize = 16,
    const F: usize = 8,
    const P: usize = { MAX_FRAME },
> {
    radio: R,
    filters: [Option<KeyFilter<'a>>; F],
    own: Option<Mac>,
    seq: Sequence,
    seen: SeenTable<K>,
    pending: Option<Pending<P>>,
}

impl<'a, R: EspNowRadio, const K: usize, const F: usize, const P: usize>
    EspNowTransport<'a, R, K, F, P>
{
    /// Compile-time check that the kept-frame buffer is at most a frame.
    const KEEP_OK: () = assert!(P <= MAX_FRAME, "EspNowTransport: P exceeds MAX_FRAME");

    /// A transport over `radio` with no filters (it receives nothing until
    /// one is added) and sequence number 0.
    pub const fn new(radio: R) -> Self {
        let () = Self::KEEP_OK;
        EspNowTransport {
            radio,
            filters: [None; F],
            own: None,
            seq: Sequence::new(0),
            seen: SeenTable::new(),
            pending: None,
        }
    }

    /// The same transport also receiving keys matching `filter`.
    ///
    /// # Errors
    ///
    /// [`FiltersFull`] when all `F` slots are taken.
    pub fn with_filter(mut self, filter: KeyFilter<'a>) -> Result<Self, FiltersFull> {
        self.add_filter(filter)?;
        Ok(self)
    }

    /// The same transport also receiving keys matching any of `filters`.
    ///
    /// # Errors
    ///
    /// [`FiltersFull`] when they do not all fit in the `F` slots.
    pub fn with_filters(self, filters: &[KeyFilter<'a>]) -> Result<Self, FiltersFull> {
        filters.iter().try_fold(self, |t, f| t.with_filter(*f))
    }

    /// The same transport ignoring frames whose source is `mac` (our own
    /// station address).
    pub fn with_own_address(self, mac: Mac) -> Self {
        EspNowTransport {
            own: Some(mac),
            ..self
        }
    }

    /// Starts receiving keys matching `filter`. Adding a filter already
    /// present is a no-op.
    ///
    /// # Errors
    ///
    /// [`FiltersFull`] when all `F` slots are taken.
    pub fn add_filter(&mut self, filter: KeyFilter<'a>) -> Result<(), FiltersFull> {
        if self.filters.contains(&Some(filter)) {
            return Ok(());
        }
        let slot = self
            .filters
            .iter_mut()
            .find(|f| f.is_none())
            .ok_or(FiltersFull)?;
        *slot = Some(filter);
        Ok(())
    }

    /// Stops receiving keys through `filter`; returns whether it was present.
    /// A sample already kept after [`EspNowError::BufferTooSmall`] is still
    /// delivered.
    pub fn remove_filter(&mut self, filter: KeyFilter<'a>) -> bool {
        match self.filters.iter_mut().find(|f| **f == Some(filter)) {
            Some(slot) => {
                *slot = None;
                true
            }
            None => false,
        }
    }

    /// True if some filter selects `key`.
    pub fn subscribed(&self, key: Key<'_>) -> bool {
        self.filters.iter().flatten().any(|f| f.matches(key))
    }

    /// True if `sel` selects `key`.
    fn selects(&self, sel: Selection<'_>, key: Key<'_>) -> bool {
        match sel {
            Selection::Slots => self.subscribed(key),
            Selection::Only(f) => f.matches(key),
        }
    }

    /// The sequence number the next publish will carry.
    pub const fn sequence(&self) -> Sequence {
        self.seq
    }

    /// Borrows the radio.
    pub fn radio_mut(&mut self) -> &mut R {
        &mut self.radio
    }

    /// Decides whether a received frame is delivered: skips our own,
    /// foreign, unfiltered and repeated frames, recording fresh ones in the
    /// seen table.
    fn accept(&mut self, sel: Selection<'_>, bytes: &[u8], src: Mac) -> Result<bool, FrameError> {
        if self.own == Some(src) {
            return Ok(false);
        }
        let frame = match Frame::decode(bytes) {
            Ok(frame) => frame,
            Err(_) if !bytes.starts_with(&MAGIC) => return Ok(false),
            Err(e) => return Err(e),
        };
        if !self.selects(sel, frame.key) {
            return Ok(false);
        }
        let (seen, fresh) = self
            .seen
            .observe(SenderId::from_mac(src), frame.key, frame.seq);
        self.seen = seen;
        Ok(fresh)
    }

    /// Writes an accepted frame into `buf` as a sample with sender meta, or
    /// keeps it for the next call when `buf` is too small (dropped instead
    /// when it does not fit the `P`-byte kept buffer).
    fn deliver<'b>(
        &mut self,
        buf: &'b mut [u8],
        bytes: &[u8],
        src: Mac,
    ) -> Result<Sample<'b>, EspNowError<R::Error>> {
        let frame = Frame::decode(bytes).map_err(EspNowError::Frame)?;
        // One sequence counts all keys of a sender, so no gap is computed.
        let meta = SampleMeta::from_sender(SenderId::from_mac(src)).with_seq(frame.seq);
        match sample_into(buf, frame.key.as_str(), frame.payload) {
            Some(sample) => Ok(sample.with_meta(meta)),
            None => {
                self.pending = Pending::keep(bytes, src).or(self.pending);
                Err(EspNowError::BufferTooSmall)
            }
        }
    }

    /// Takes the kept sample if `sel` selects its key; otherwise leaves it.
    fn take_pending(&mut self, sel: Selection<'_>) -> Option<Pending<P>> {
        let p = self.pending?;
        let key = Frame::decode(p.bytes()).ok()?.key;
        if self.selects(sel, key) {
            self.pending = None;
            Some(p)
        } else {
            None
        }
    }

    /// Delivers a sample kept by an earlier [`EspNowError::BufferTooSmall`].
    fn deliver_pending<'b>(
        &mut self,
        buf: &'b mut [u8],
        p: Pending<P>,
    ) -> Result<Sample<'b>, EspNowError<R::Error>> {
        self.deliver(buf, p.bytes(), p.src)
    }

    /// Waits for the next fresh sample `sel` selects.
    async fn recv_selected<'b>(
        &mut self,
        sel: Selection<'_>,
        buf: &'b mut [u8],
    ) -> Result<Sample<'b>, EspNowError<R::Error>> {
        if let Some(p) = self.take_pending(sel) {
            return self.deliver_pending(buf, p);
        }
        let mut raw = [0u8; MAX_FRAME];
        for _ in 0..MAX_IGNORED_FRAMES {
            let (bytes, src) = self
                .radio
                .receive(&mut raw)
                .await
                .map_err(EspNowError::Radio)?;
            if self.accept(sel, bytes, src).map_err(EspNowError::Frame)? {
                return self.deliver(buf, bytes, src);
            }
        }
        Err(EspNowError::IgnoredLimit)
    }

    /// Returns the next fresh sample `sel` selects among the frames already
    /// queued in the radio.
    fn try_recv_selected<'b>(
        &mut self,
        sel: Selection<'_>,
        buf: &'b mut [u8],
    ) -> Result<Option<Sample<'b>>, EspNowError<R::Error>> {
        if let Some(p) = self.take_pending(sel) {
            return self.deliver_pending(buf, p).map(Some);
        }
        let mut raw = [0u8; MAX_FRAME];
        for _ in 0..MAX_IGNORED_FRAMES {
            let Some((bytes, src)) = self
                .radio
                .try_receive(&mut raw)
                .map_err(EspNowError::Radio)?
            else {
                return Ok(None);
            };
            if self.accept(sel, bytes, src).map_err(EspNowError::Frame)? {
                return self.deliver(buf, bytes, src).map(Some);
            }
        }
        Err(EspNowError::IgnoredLimit)
    }
}

impl<R: EspNowRadio, const K: usize, const F: usize, const P: usize> Publisher
    for EspNowTransport<'_, R, K, F, P>
{
    type Error = EspNowError<R::Error>;

    const LIMITS: Limits = LIMITS;

    /// Broadcasts one frame. The sequence number advances once the frame is
    /// encoded, before the radio is awaited (see the module docs).
    async fn publish(&mut self, key: Key<'_>, payload: &[u8]) -> Result<(), Self::Error> {
        let mut out = [0u8; MAX_FRAME];
        let n = Frame {
            seq: self.seq.get(),
            key,
            payload,
        }
        .encode(&mut out)
        .map_err(EspNowError::Frame)?;
        let frame = out
            .get(..n)
            .ok_or(EspNowError::Frame(FrameError::TooLong))?;
        self.seq = self.seq.next();
        self.radio
            .broadcast(frame)
            .await
            .map_err(EspNowError::Radio)
    }
}

impl<R: EspNowRadio, const K: usize, const F: usize, const P: usize> Subscriber
    for EspNowTransport<'_, R, K, F, P>
{
    type Error = EspNowError<R::Error>;

    /// See [`DELIVERY`].
    const DELIVERY: Delivery = DELIVERY;
    const LIMITS: Limits = LIMITS;

    /// Waits for the next fresh sample selected by a filter slot.
    /// Cancel-safe if the radio's `receive` is (see the module docs).
    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, Self::Error> {
        self.recv_selected(Selection::Slots, buf).await
    }

    /// Returns the next fresh sample among the frames already queued in the
    /// radio, or `Ok(None)` once the radio has none.
    fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, Self::Error> {
        self.try_recv_selected(Selection::Slots, buf)
    }
}

impl<'a, R: EspNowRadio, const K: usize, const F: usize, const P: usize> Subscribe
    for EspNowTransport<'a, R, K, F, P>
{
    type Subscriber<'s>
        = EspNowSubscriber<'s, 'a, R, K, F, P>
    where
        Self: 's;
    type Error = EspNowError<R::Error>;

    /// Borrows the transport as an [`EspNowSubscriber`] owning a copy of
    /// `filter`. Never waits. [`EspNowError::Frame`] with
    /// [`FrameError::KeyTooLong`] when the filter's key is longer than
    /// [`MAX_KEY`] (no frame could match it).
    async fn subscribe<'s>(
        &'s mut self,
        filter: KeyFilter<'_>,
    ) -> Result<EspNowSubscriber<'s, 'a, R, K, F, P>, Self::Error> {
        let filter = filter
            .to_buf()
            .map_err(|_| EspNowError::Frame(FrameError::KeyTooLong))?;
        Ok(EspNowSubscriber {
            transport: self,
            filter,
        })
    }
}

/// An [`EspNowTransport`] borrowed as a subscriber to the keys one filter
/// selects; made by [`Subscribe::subscribe`]. The transport's own filter
/// slots do not apply while receiving through it (see the module docs).
pub struct EspNowSubscriber<
    's,
    'a,
    R,
    const K: usize = 16,
    const F: usize = 8,
    const P: usize = { MAX_FRAME },
> {
    transport: &'s mut EspNowTransport<'a, R, K, F, P>,
    filter: KeyFilterBuf<MAX_KEY>,
}

impl<R, const K: usize, const F: usize, const P: usize> EspNowSubscriber<'_, '_, R, K, F, P> {
    /// The filter this view applies.
    pub fn filter(&self) -> KeyFilter<'_> {
        self.filter.as_filter()
    }
}

impl<R: EspNowRadio, const K: usize, const F: usize, const P: usize> Subscriber
    for EspNowSubscriber<'_, '_, R, K, F, P>
{
    type Error = EspNowError<R::Error>;

    /// See [`DELIVERY`].
    const DELIVERY: Delivery = DELIVERY;
    const LIMITS: Limits = LIMITS;

    /// As the transport's `recv`, selecting with this view's filter only.
    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, Self::Error> {
        let sel = Selection::Only(self.filter.as_filter());
        self.transport.recv_selected(sel, buf).await
    }

    /// As the transport's `try_recv`, selecting with this view's filter only.
    fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, Self::Error> {
        let sel = Selection::Only(self.filter.as_filter());
        self.transport.try_recv_selected(sel, buf)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use core::future::{Future, poll_fn};
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};
    use std::borrow::ToOwned;
    use std::collections::VecDeque;
    use std::vec::Vec;

    const A: Mac = [2, 0, 0, 0, 0, 0xA];
    const B: Mac = [2, 0, 0, 0, 0, 0xB];
    const SYNC: Key<'static> = Key::from_static("conagotchi/sync");
    const HELLO: Key<'static> = Key::from_static("app/hello");
    const OTHER: Key<'static> = Key::from_static("other/thing");
    const APP: KeyFilter<'static> = KeyFilter::Prefix(Key::from_static("app"));
    const SYNC_ONLY: KeyFilter<'static> = KeyFilter::Exact(SYNC);

    type T<'a, const K: usize = 4> = EspNowTransport<'a, FakeRadio, K, 4>;

    /// Polls `f` with a no-op waker until ready (bounded).
    fn block_on<F: Future>(f: F) -> F::Output {
        let mut f = pin!(f);
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..1000 {
            if let Poll::Ready(out) = f.as_mut().poll(&mut cx) {
                return out;
            }
        }
        panic!("future did not complete");
    }

    /// Polls `f` exactly once and drops it, returning whether it was ready.
    fn poll_once_then_drop<F: Future>(f: F) -> bool {
        let mut f = pin!(f);
        let mut cx = Context::from_waker(Waker::noop());
        f.as_mut().poll(&mut cx).is_ready()
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum FakeError {
        /// Nothing queued to receive (when `wait` is off).
        Empty,
        /// The frame does not fit the receive buffer.
        TooLarge,
    }

    /// In-memory radio: `inbox` feeds `receive`, `sent` collects broadcasts.
    /// With `wait`, an empty `receive` stays pending (cancel-safely) instead
    /// of failing; with `stall`, `broadcast` records the frame and never
    /// completes.
    #[derive(Default)]
    struct FakeRadio {
        inbox: VecDeque<(Vec<u8>, Mac)>,
        sent: Vec<Vec<u8>>,
        wait: bool,
        stall: bool,
    }

    impl FakeRadio {
        fn push(&mut self, frame: &[u8], src: Mac) {
            self.inbox.push_back((frame.to_vec(), src));
        }

        fn pop_into<'b>(
            &mut self,
            buf: &'b mut [u8],
        ) -> Result<Option<(&'b [u8], Mac)>, FakeError> {
            let Some((frame, src)) = self.inbox.pop_front() else {
                return Ok(None);
            };
            let slot = buf.get_mut(..frame.len()).ok_or(FakeError::TooLarge)?;
            slot.copy_from_slice(&frame);
            Ok(Some((slot, src)))
        }
    }

    impl EspNowRadio for FakeRadio {
        type Error = FakeError;

        async fn broadcast(&mut self, frame: &[u8]) -> Result<(), FakeError> {
            self.sent.push(frame.to_vec());
            if self.stall {
                poll_fn(|_| Poll::<()>::Pending).await;
            }
            Ok(())
        }

        async fn receive<'b>(&mut self, buf: &'b mut [u8]) -> Result<(&'b [u8], Mac), FakeError> {
            if self.wait {
                // Nothing is consumed until a frame is there: cancel-safe.
                poll_fn(|_| {
                    if self.inbox.is_empty() {
                        Poll::Pending
                    } else {
                        Poll::Ready(())
                    }
                })
                .await;
            }
            self.pop_into(buf)?.ok_or(FakeError::Empty)
        }

        fn try_receive<'b>(
            &mut self,
            buf: &'b mut [u8],
        ) -> Result<Option<(&'b [u8], Mac)>, FakeError> {
            self.pop_into(buf)
        }
    }

    fn frame(seq: u8, key: Key<'_>, payload: &[u8]) -> Vec<u8> {
        let mut out = [0u8; MAX_FRAME];
        let n = Frame { seq, key, payload }.encode(&mut out).unwrap();
        out[..n].to_vec()
    }

    fn sub<const K: usize>(radio: FakeRadio, filters: &[KeyFilter<'static>]) -> T<'static, K> {
        EspNowTransport::new(radio).with_filters(filters).unwrap()
    }

    #[test]
    fn n1_frame_round_trip_and_layout() {
        let bytes = frame(7, SYNC, b"OZL1abcdefgh");
        assert_eq!(&bytes[..3], b"MW\x01");
        assert_eq!(u16::from_le_bytes([bytes[3], bytes[4]]), short_id(SYNC));
        assert_eq!(bytes[5], 7);
        assert_eq!(usize::from(bytes[6]), SYNC.as_str().len());
        assert_eq!(bytes.len(), HEADER_LEN + SYNC.as_str().len() + 12);
        let f = Frame::decode(&bytes).unwrap();
        assert_eq!((f.seq, f.key, f.payload), (7, SYNC, &b"OZL1abcdefgh"[..]));
    }

    #[test]
    fn n1_largest_frame_fits_espnow_v1() {
        let key_text = [b'k'; MAX_KEY];
        let key = Key::new(core::str::from_utf8(&key_text).unwrap()).unwrap();
        let bytes = frame(0, key, &[0xAB; MAX_PAYLOAD]);
        assert_eq!(bytes.len(), MAX_FRAME);
        assert_eq!(Frame::decode(&bytes).unwrap().payload.len(), MAX_PAYLOAD);
    }

    #[test]
    fn n1_short_id_is_fnv1a64_folded() {
        // Reference: fold of fnv1a64("conagotchi/sync"), the BLE short id.
        let h = crate::dedup::fnv1a64(b"conagotchi/sync");
        assert_eq!(
            short_id(SYNC),
            (h ^ (h >> 16) ^ (h >> 32) ^ (h >> 48)) as u16
        );
        #[cfg(feature = "ble")]
        assert_eq!(short_id(SYNC), crate::ble::uuid::short_id(SYNC));
    }

    #[test]
    fn n2_oversized_publications_are_refused() {
        let mut out = [0u8; MAX_FRAME];
        let long = Frame {
            seq: 0,
            key: HELLO,
            payload: &[0; MAX_PAYLOAD + 1],
        };
        assert_eq!(long.encode(&mut out), Err(FrameError::PayloadTooLarge));
        let key_text = [b'k'; MAX_KEY + 1];
        let key = Key::new(core::str::from_utf8(&key_text).unwrap()).unwrap();
        let long_key = Frame {
            seq: 0,
            key,
            payload: b"",
        };
        assert_eq!(long_key.encode(&mut out), Err(FrameError::KeyTooLong));

        let mut t: T<'_> = EspNowTransport::new(FakeRadio::default());
        let r = block_on(t.publish(HELLO, &[0; MAX_PAYLOAD + 1]));
        assert_eq!(r, Err(EspNowError::Frame(FrameError::PayloadTooLarge)));
        assert!(t.radio_mut().sent.is_empty());
        assert_eq!(t.sequence().get(), 0);
    }

    #[test]
    fn n2_limits_are_advertised() {
        assert_eq!(<T<'_> as Publisher>::LIMITS, LIMITS);
        assert_eq!(
            LIMITS,
            Limits {
                max_key: 64,
                max_payload: 179,
                max_message: 243,
                shared: true
            }
        );
        assert_eq!(HEADER_LEN + LIMITS.max_key + LIMITS.max_payload, MAX_FRAME);
    }

    #[test]
    fn n3_malformed_frames_are_rejected() {
        let good = frame(1, SYNC, b"x");
        assert_eq!(Frame::decode(b""), Err(FrameError::TooShort));
        assert_eq!(Frame::decode(b"OZL1abcdefgh"), Err(FrameError::BadMagic));
        assert_eq!(Frame::decode(&good[..5]), Err(FrameError::TooShort));
        assert_eq!(Frame::decode(&good[..10]), Err(FrameError::TooShort));
        let mut v = good.clone();
        v[2] = 2;
        assert_eq!(Frame::decode(&v), Err(FrameError::BadVersion(2)));
        let mut v = good.clone();
        v[6] = 0;
        assert_eq!(Frame::decode(&v), Err(FrameError::BadKeyLength));
        let mut v = good.clone();
        v[6] = (MAX_KEY + 1) as u8;
        assert_eq!(Frame::decode(&v), Err(FrameError::BadKeyLength));
        let mut v = good.clone();
        v[HEADER_LEN] = b' ';
        assert_eq!(Frame::decode(&v), Err(FrameError::BadKey));
        let mut v = good.clone();
        v[3] ^= 1;
        assert_eq!(Frame::decode(&v), Err(FrameError::IdMismatch));
        assert_eq!(
            Frame::decode(&[b'M'; MAX_FRAME + 1]),
            Err(FrameError::TooLong)
        );
    }

    #[test]
    fn n3_malformed_mw_frame_is_an_error_foreign_frames_are_skipped() {
        let mut radio = FakeRadio::default();
        radio.push(b"OZL1abcdefgh", A); // foreign: skipped
        let mut bad = frame(1, SYNC, b"x");
        bad[3] ^= 1;
        radio.push(&bad, A);
        radio.push(&frame(2, SYNC, b"y"), A);
        let mut t = sub::<4>(radio, &[SYNC_ONLY]);
        let mut buf = [0u8; 64];
        assert_eq!(
            block_on(t.recv(&mut buf)),
            Err(EspNowError::Frame(FrameError::IdMismatch))
        );
        assert_eq!(block_on(t.recv(&mut buf)).unwrap().payload, b"y");
    }

    #[test]
    fn n4_exact_and_prefix_filters_select_keys() {
        let frames = [
            frame(0, SYNC, b"s"),
            frame(1, HELLO, b"h"),
            frame(2, Key::from_static("apple"), b"no"),
            frame(3, Key::from_static("app/x/y"), b"deep"),
            frame(4, OTHER, b"o"),
        ];
        let (mut ra, mut rb) = (FakeRadio::default(), FakeRadio::default());
        for f in &frames {
            ra.push(f, A);
            rb.push(f, A);
        }
        let mut exact = sub::<8>(ra, &[SYNC_ONLY]);
        let mut prefix = sub::<8>(rb, &[APP]);
        let mut buf = [0u8; 64];
        let mut got = Vec::new();
        while let Ok(Some(s)) = exact.try_recv(&mut buf) {
            got.push(s.key.as_str().to_owned());
        }
        assert_eq!(got, ["conagotchi/sync"]);
        let mut got = Vec::new();
        while let Ok(Some(s)) = prefix.try_recv(&mut buf) {
            got.push(s.key.as_str().to_owned());
        }
        assert_eq!(
            got,
            ["app/hello", "app/x/y"],
            "prefix is segment-wise: `apple` is not under `app`"
        );
        assert!(prefix.subscribed(Key::from_static("app")));
        assert!(!prefix.subscribed(SYNC));
    }

    #[test]
    fn n4_filters_change_at_runtime_and_are_bounded() {
        let mut t: EspNowTransport<'_, FakeRadio, 4, 2> =
            EspNowTransport::new(FakeRadio::default());
        assert!(!t.subscribed(SYNC), "no filter, nothing received");
        t.add_filter(SYNC_ONLY).unwrap();
        t.add_filter(SYNC_ONLY).unwrap(); // duplicate: no new slot
        t.add_filter(APP).unwrap();
        assert_eq!(t.add_filter(KeyFilter::Exact(OTHER)), Err(FiltersFull));
        assert!(t.subscribed(SYNC) && t.subscribed(HELLO) && !t.subscribed(OTHER));

        assert!(t.remove_filter(SYNC_ONLY));
        assert!(!t.remove_filter(SYNC_ONLY));
        t.add_filter(KeyFilter::Exact(OTHER)).unwrap();
        assert!(!t.subscribed(SYNC) && t.subscribed(OTHER));

        t.radio_mut().push(&frame(0, SYNC, b"gone"), A);
        t.radio_mut().push(&frame(0, OTHER, b"new"), A);
        let mut buf = [0u8; 64];
        assert_eq!(t.try_recv(&mut buf).unwrap().unwrap().payload, b"new");

        let full = EspNowTransport::<'_, FakeRadio, 4, 1>::new(FakeRadio::default())
            .with_filters(&[SYNC_ONLY, APP]);
        assert!(matches!(full, Err(FiltersFull)));
    }

    #[test]
    fn n5_duplicates_are_dropped_per_sender_and_key() {
        let mut radio = FakeRadio::default();
        let f = frame(5, SYNC, b"1");
        radio.push(&f, A);
        radio.push(&f, A); // repeat: dropped
        radio.push(&f, B); // same seq, other sender: fresh
        radio.push(&frame(5, HELLO, b"2"), A); // same seq, other key: fresh
        radio.push(&frame(6, SYNC, b"3"), A); // next seq: fresh
        let mut t = sub::<4>(radio, &[SYNC_ONLY, APP]);
        let mut buf = [0u8; 64];
        let mut got = Vec::new();
        while let Ok(s) = block_on(t.recv(&mut buf)) {
            got.push((s.meta.sender, s.payload.to_vec()));
        }
        let (a, b) = (Some(SenderId::from_mac(A)), Some(SenderId::from_mac(B)));
        assert_eq!(
            got,
            [
                (a, b"1".to_vec()),
                (b, b"1".to_vec()),
                (a, b"2".to_vec()),
                (a, b"3".to_vec())
            ]
        );
    }

    #[test]
    fn n6_samples_carry_sender_and_sequence() {
        let mut radio = FakeRadio::default();
        radio.push(&frame(9, SYNC, b"beacon"), A);
        let mut t = sub::<4>(radio, &[SYNC_ONLY]);
        let mut buf = [0u8; 64];
        let s = block_on(t.recv(&mut buf)).unwrap();
        assert_eq!((s.key, s.payload), (SYNC, &b"beacon"[..]));
        assert_eq!(
            s.meta,
            SampleMeta::from_sender(SenderId([2, 0, 0, 0, 0, 0xA, 0, 0])).with_seq(9)
        );
        assert_eq!(
            block_on(t.recv(&mut buf)),
            Err(EspNowError::Radio(FakeError::Empty))
        );
    }

    #[test]
    fn n7_own_frames_are_skipped() {
        let mut radio = FakeRadio::default();
        radio.push(&frame(0, SYNC, b"me"), A);
        radio.push(&frame(0, SYNC, b"peer"), B);
        let mut t = sub::<4>(radio, &[SYNC_ONLY]).with_own_address(A);
        let mut buf = [0u8; 64];
        assert_eq!(block_on(t.recv(&mut buf)).unwrap().payload, b"peer");
    }

    #[test]
    fn n7_ignored_limit_bounds_recv_and_try_recv() {
        let noise = frame(0, OTHER, b"");
        let mut radio = FakeRadio::default();
        for _ in 0..MAX_IGNORED_FRAMES * 2 + 1 {
            radio.push(&noise, B);
        }
        radio.push(&frame(0, SYNC, b"late"), B);
        let mut t = sub::<4>(radio, &[SYNC_ONLY]);
        let mut buf = [0u8; 64];
        assert_eq!(block_on(t.recv(&mut buf)), Err(EspNowError::IgnoredLimit));
        assert_eq!(t.radio_mut().inbox.len(), MAX_IGNORED_FRAMES + 2);
        assert_eq!(t.try_recv(&mut buf), Err(EspNowError::IgnoredLimit));
        assert_eq!(t.radio_mut().inbox.len(), 2);
        assert_eq!(t.try_recv(&mut buf).unwrap().unwrap().payload, b"late");
    }

    #[test]
    fn n8_buffer_too_small_keeps_the_sample() {
        let mut radio = FakeRadio::default();
        radio.push(&frame(3, SYNC, b"0123456789"), A);
        radio.push(&frame(4, SYNC, b"next"), A);
        let mut t = sub::<4>(radio, &[SYNC_ONLY]);
        let mut small = [0u8; 20]; // key (15) fits, payload (10) does not
        assert_eq!(
            block_on(t.recv(&mut small)),
            Err(EspNowError::BufferTooSmall)
        );
        assert_eq!(t.try_recv(&mut small), Err(EspNowError::BufferTooSmall));
        let mut buf = [0u8; 64];
        let s = block_on(t.recv(&mut buf)).unwrap();
        assert_eq!((s.payload, s.meta.seq), (&b"0123456789"[..], Some(3)));
        assert_eq!(t.try_recv(&mut buf).unwrap().unwrap().payload, b"next");
    }

    #[test]
    fn n9_try_recv_returns_queued_samples_without_waiting() {
        let mut radio = FakeRadio {
            wait: true,
            ..FakeRadio::default()
        };
        radio.push(&frame(0, OTHER, b"skip"), A);
        radio.push(&frame(0, SYNC, b"one"), A);
        radio.push(&frame(0, SYNC, b"one"), A); // repeat
        let mut t = sub::<4>(radio, &[SYNC_ONLY]);
        let mut buf = [0u8; 64];
        let s = t.try_recv(&mut buf).unwrap().unwrap();
        assert_eq!(
            (s.payload, s.meta.sender),
            (&b"one"[..], Some(SenderId::from_mac(A)))
        );
        assert_eq!(t.try_recv(&mut buf), Ok(None), "only a repeat was left");
        assert_eq!(t.try_recv(&mut buf), Ok(None), "empty radio");
    }

    #[test]
    fn n10_dropped_recv_loses_no_sample() {
        let mut radio = FakeRadio {
            wait: true,
            ..FakeRadio::default()
        };
        radio.push(&frame(0, OTHER, b"skip"), A);
        let mut t = sub::<4>(radio, &[SYNC_ONLY]);
        let mut buf = [0u8; 64];
        // The first poll skips the foreign key, then waits; drop it there.
        assert!(!poll_once_then_drop(t.recv(&mut buf)));
        assert!(t.radio_mut().inbox.is_empty());
        t.radio_mut().push(&frame(1, SYNC, b"after"), A);
        assert_eq!(t.try_recv(&mut buf).unwrap().unwrap().payload, b"after");

        // Dropped while waiting on an empty radio, then a frame arrives:
        // the next recv gets it, and the dedup state is intact.
        assert!(!poll_once_then_drop(t.recv(&mut buf)));
        t.radio_mut().push(&frame(2, SYNC, b"x"), B);
        t.radio_mut().push(&frame(1, SYNC, b"after"), A); // repeat of A/1
        assert_eq!(block_on(t.recv(&mut buf)).unwrap().payload, b"x");
        assert_eq!(t.try_recv(&mut buf), Ok(None));
    }

    #[test]
    fn n10_dropped_publish_never_reuses_a_sequence() {
        let mut t: T<'_> = EspNowTransport::new(FakeRadio {
            stall: true,
            ..FakeRadio::default()
        });
        assert!(!poll_once_then_drop(t.publish(SYNC, b"maybe")));
        t.radio_mut().stall = false;
        block_on(t.publish(SYNC, b"sure")).unwrap();
        let sent = core::mem::take(&mut t.radio_mut().sent);
        let seqs: Vec<u8> = sent.iter().map(|f| Frame::decode(f).unwrap().seq).collect();
        assert_eq!(seqs, [0, 1]);
        assert_eq!(t.sequence().get(), 2);

        // A receiver that saw the dropped frame still takes the next one.
        let mut radio = FakeRadio::default();
        for f in &sent {
            radio.push(f, A);
        }
        let mut r = sub::<4>(radio, &[SYNC_ONLY]);
        let mut buf = [0u8; 64];
        assert_eq!(r.try_recv(&mut buf).unwrap().unwrap().payload, b"maybe");
        assert_eq!(r.try_recv(&mut buf).unwrap().unwrap().payload, b"sure");
    }

    #[test]
    fn n13_subscribe_borrows_the_transport_with_its_own_filter() {
        let mut radio = FakeRadio::default();
        radio.push(&frame(0, SYNC, b"slot only"), A);
        radio.push(&frame(0, Key::from_static("apple"), b"no"), A);
        radio.push(&frame(0, HELLO, b"mine"), A);
        radio.push(&frame(0, HELLO, b"mine"), A); // repeat
        radio.push(&frame(1, SYNC, b"later"), B);
        radio.push(&frame(0, OTHER, b"own"), B);
        let mut t = sub::<4>(radio, &[SYNC_ONLY]).with_own_address(B);
        let mut buf = [0u8; 64];
        {
            // The filter's key text is gone once the view is made.
            let text = std::string::String::from("app");
            let mut view =
                block_on(t.subscribe(KeyFilter::Prefix(Key::new(&text).unwrap()))).unwrap();
            drop(text);
            assert_eq!(view.filter(), APP);
            let s = block_on(view.recv(&mut buf)).unwrap();
            assert_eq!((s.key, s.payload), (HELLO, &b"mine"[..]));
            assert_eq!(s.meta.sender, Some(SenderId::from_mac(A)));
            // The repeat is dropped; SYNC is not this view's key and the
            // last frame is from our own address.
            assert_eq!(view.try_recv(&mut buf), Ok(None));
        }
        // The view consumed the frames it skipped; the slots apply again.
        assert!(t.try_recv(&mut buf).unwrap().is_none());
        t.radio_mut().push(&frame(2, SYNC, b"slot"), A);
        assert_eq!(t.try_recv(&mut buf).unwrap().unwrap().payload, b"slot");
        // A filter on a key no frame can carry is refused.
        let key_text = [b'k'; MAX_KEY + 1];
        let long = Key::new(core::str::from_utf8(&key_text).unwrap()).unwrap();
        assert!(matches!(
            block_on(t.subscribe(KeyFilter::Exact(long))),
            Err(EspNowError::Frame(FrameError::KeyTooLong))
        ));
    }

    /// Generic code reaches the transport through the trait alone.
    async fn first_payload_len<S: Subscribe>(
        transport: &mut S,
        filter: KeyFilter<'_>,
        buf: &mut [u8],
    ) -> Option<usize> {
        let mut sub = transport.subscribe(filter).await.ok()?;
        sub.recv(buf).await.ok().map(|s| s.payload.len())
    }

    #[test]
    fn n13_transport_is_usable_through_generic_subscribe() {
        let mut radio = FakeRadio::default();
        radio.push(&frame(0, OTHER, b"x"), A);
        radio.push(&frame(0, SYNC, b"abcd"), A);
        // No filter slots at all: the view's filter is what selects.
        let mut t: T<'_> = EspNowTransport::new(radio);
        let mut buf = [0u8; 64];
        assert_eq!(
            block_on(first_payload_len(&mut t, SYNC_ONLY, &mut buf)),
            Some(4)
        );
    }

    #[test]
    fn n13_kept_sample_waits_for_a_receive_that_selects_it() {
        let mut radio = FakeRadio::default();
        radio.push(&frame(0, SYNC, b"0123456789"), A);
        radio.push(&frame(0, HELLO, b"hi"), A);
        let mut t = sub::<4>(radio, &[SYNC_ONLY]);
        let mut small = [0u8; 20];
        assert_eq!(t.try_recv(&mut small), Err(EspNowError::BufferTooSmall));
        let mut buf = [0u8; 64];
        {
            let mut view = block_on(t.subscribe(APP)).unwrap();
            // The kept SYNC sample is not this view's: it stays kept.
            assert_eq!(view.try_recv(&mut buf).unwrap().unwrap().payload, b"hi");
        }
        assert_eq!(
            t.try_recv(&mut buf).unwrap().unwrap().payload,
            b"0123456789"
        );
    }

    #[test]
    fn n14_keeping_is_optional_and_saves_a_frame_of_ram() {
        type Keep = EspNowTransport<'static, FakeRadio, 4, 4>;
        type NoKeep = EspNowTransport<'static, FakeRadio, 4, 4, 0>;
        let keep = core::mem::size_of::<Keep>();
        let no_keep = core::mem::size_of::<NoKeep>();
        assert_eq!(
            core::mem::size_of::<Option<Pending<MAX_FRAME>>>(),
            MAX_FRAME + 8,
            "an enabled kept slot is the frame plus sender and length"
        );
        // Field reordering may move a few bytes of padding into the gap.
        assert!(keep - no_keep >= MAX_FRAME - 8, "{keep} vs {no_keep}");
        assert_eq!(
            core::mem::size_of::<Option<Pending<0>>>(),
            8,
            "a disabled kept slot is the sender and length only"
        );

        let mut radio = FakeRadio::default();
        radio.push(&frame(3, SYNC, b"0123456789"), A);
        radio.push(&frame(4, SYNC, b"next"), A);
        let mut t: NoKeep = EspNowTransport::new(radio).with_filter(SYNC_ONLY).unwrap();
        let mut small = [0u8; 20];
        assert_eq!(t.try_recv(&mut small), Err(EspNowError::BufferTooSmall));
        let mut buf = [0u8; MAX_FRAME];
        let s = t.try_recv(&mut buf).unwrap().unwrap();
        assert_eq!(
            (s.payload, s.meta.seq),
            (&b"next"[..], Some(4)),
            "with P = 0 the sample that did not fit is dropped"
        );
    }

    #[test]
    fn n11_end_to_end_publish_to_subscribe() {
        let mut publisher: T<'_> = EspNowTransport::new(FakeRadio::default());
        block_on(publisher.publish(SYNC, b"OZL1\x01\0\0\0\x02\0\0\0")).unwrap();
        block_on(publisher.publish(HELLO, b"hi")).unwrap();
        block_on(publisher.publish(OTHER, b"no")).unwrap();
        assert_eq!(publisher.sequence().get(), 3);
        let sent = core::mem::take(&mut publisher.radio_mut().sent);
        assert_eq!(sent.len(), 3);

        let mut radio = FakeRadio::default();
        for f in &sent {
            radio.push(f, A);
        }
        let mut s = sub::<8>(radio, &[SYNC_ONLY, APP]);
        let mut buf = [0u8; 128];
        let r = block_on(s.recv(&mut buf)).unwrap();
        assert_eq!((r.key, r.meta.seq), (SYNC, Some(0)));
        assert_eq!(r.payload, b"OZL1\x01\0\0\0\x02\0\0\0");
        let r = block_on(s.recv(&mut buf)).unwrap();
        assert_eq!((r.key, r.payload, r.meta.seq), (HELLO, &b"hi"[..], Some(1)));
        assert!(block_on(s.recv(&mut buf)).is_err());
    }
}
