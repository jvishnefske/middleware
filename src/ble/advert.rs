//! Connectionless BLE: legacy advertising data, scanning and discovery.
//!
//! * [`AdvertisementBuilder`] assembles legacy advertising data (at most
//!   [`MAX_ADV_DATA`] = 31 bytes) into a fixed buffer, in this order: Flags
//!   (`0x01`), the 128-bit Complete Service UUID list (`0x07`) holding
//!   [`SERVICE_UUID`] little-endian, Manufacturer Specific Data (`0xFF`) with
//!   a configurable company id (default [`TEST_COMPANY_ID`] `0xFFFF`), and
//!   the local name last. Under the default [`NamePolicy::Shorten`] a name
//!   that does not fit is cut at a UTF-8 boundary and sent as Shortened
//!   Local Name (`0x08`); if not even one character fits, or under
//!   [`NamePolicy::Exact`], building fails with [`AdvertError::Overflow`].
//!   Anything else that does not fit is also [`AdvertError::Overflow`].
//! * [`parse`] iterates AD structures without copying. A length byte that
//!   overruns the data yields [`AdvertError::Malformed`] and ends iteration;
//!   a zero length byte marks the start of zero padding and ends it cleanly.
//! * [`Advertiser`] and [`Scanner`] are the radio traits. An advertiser may
//!   share the advertising set with other traffic, so a successful publish
//!   means the data was accepted (possibly queued), not that it is on air.
//!   A scanner offers a blocking [`Scanner::next_report`] and a non-blocking
//!   [`Scanner::try_next_report`], so a binding fed by a synchronous radio
//!   callback can queue reports and hand them out without waiting.
//! * [`AdvertisingPublisher`] implements [`Publisher`]: each publish hands
//!   the advertiser advertising data whose manufacturer data is
//!   `[short_id(key) u16 LE][sequence u8][payload]`. The [`Sequence`] starts
//!   at 0 and increments (wrapping) after every accepted publish. A payload
//!   that does not fit beside the template's other fields is an error;
//!   [`Publisher::LIMITS`] states [`MAX_ADVERT_PAYLOAD`] (21 bytes, the room
//!   left by the default template) and [`AdvertisingPublisher::max_payload`]
//!   the exact room left by the configured template.
//! * [`ScanQueue`] is a bounded, no-heap queue of scan reports between a
//!   radio callback ([`ScanQueue::push_report`], `&mut self`, newest wins
//!   when full) and a [`Scanner`] consumer that awaits a report with a
//!   registered [`Waker`]. Producer and consumer can be one owner or split
//!   by the caller's own lock ([`ScanQueue::poll_report`]); a dropped await
//!   loses no report.
//! * An advert carries only the 16-bit short id of its key, never the key
//!   text. [`ShortIdTable`] holds up to `K` learned key texts and resolves a
//!   short id back to its key; learning a key whose short id is already taken
//!   by a different key fails with [`ShortIdError::Collision`], so ambiguous
//!   ids are noticed instead of silently delivering the wrong key.
//! * [`AdvertisingSubscriber`] implements [`Subscriber`] for up to `F`
//!   filters, copied in as owned [`KeyFilterBuf`]s, that can grow at run
//!   time ([`AdvertisingSubscriber::subscribe_key`],
//!   [`AdvertisingSubscriber::learn`]); `K` separately bounds its learned
//!   keys. It also implements [`Subscribe`]: each subscription is an
//!   [`AdvertisingSubscription`] view borrowing the subscriber and
//!   delivering only its own filter's keys. A report becomes a sample when its
//!   manufacturer data has the configured company id, its short id resolves
//!   to a learned key, and that key matches a filter. An exact filter learns
//!   its key; a prefix filter matches **only keys that were learned**, because
//!   the advert lacks the text needed to match a prefix. Repeats are dropped
//!   per (sender address, key) with a [`SeenTable`], so two advertisers that
//!   both start their sequence at 0 never suppress each other, and each
//!   sample carries [`SampleMeta`] with the sender ([`Address::sender`]) and
//!   sequence number.
//! * [`discover`] recognises a scan report advertising [`SERVICE_UUID`] as a
//!   middleware [`Node`] and extracts its local name. A publish with a
//!   payload leaves no room for the 128-bit service UUID, so publishing nodes
//!   are discovered through a separate advert.
//!
//! Scan loops drop unrelated reports. [`Subscriber::recv`] gives up after
//! [`MAX_IGNORED_REPORTS`] with [`AdvertisingError::ReportLimit`] and
//! [`Subscriber::try_recv`] returns `Ok(None)` after as many, so every call
//! is bounded. Receiving is cancel-safe when the scanner's
//! [`Scanner::next_report`] is (a [`ScanQueue`]'s is): all subscriber state
//! changes happen synchronously after a report arrives.
//!
//! The publisher, subscriber, [`ShortIdTable`] and [`ScanQueue`] have
//! `const fn new` constructors and [`Default`] impls (the publisher and
//! subscriber when their radio type is `Default`), so they can live in
//! `static` initialisers.

use core::task::{Context, Poll, Waker};

use super::uuid::{SERVICE_UUID, short_id};
use crate::dedup::SeenTable;
pub use crate::dedup::Sequence;
use crate::key::{AsKey, Filter, Key, KeyBuf, KeyFilter, KeyFilterBuf, MAX_KEY_LEN};
use crate::transport::{
    Limits, Publisher, Sample, SampleMeta, SenderId, Subscribe, Subscriber, sample_into,
};

/// Largest legacy advertising data payload.
pub const MAX_ADV_DATA: usize = 31;
/// AD type: Flags.
pub const AD_FLAGS: u8 = 0x01;
/// AD type: Incomplete List of 128-bit Service UUIDs.
pub const AD_INCOMPLETE_UUID128: u8 = 0x06;
/// AD type: Complete List of 128-bit Service UUIDs.
pub const AD_COMPLETE_UUID128: u8 = 0x07;
/// AD type: Shortened Local Name.
pub const AD_SHORTENED_LOCAL_NAME: u8 = 0x08;
/// AD type: Complete Local Name.
pub const AD_COMPLETE_LOCAL_NAME: u8 = 0x09;
/// AD type: Manufacturer Specific Data.
pub const AD_MANUFACTURER_DATA: u8 = 0xFF;
/// Flags value: LE General Discoverable, BR/EDR not supported.
pub const FLAGS_GENERAL_DISCOVERABLE: u8 = 0x06;
/// Bluetooth SIG company id reserved for testing.
pub const TEST_COMPANY_ID: u16 = 0xFFFF;
/// Bytes of `[short id][sequence]` before a published payload.
pub const PUBLISH_HEADER_LEN: usize = 3;
/// Reports a scanning call drops before failing.
pub const MAX_IGNORED_REPORTS: usize = 256;

/// Advertising data error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvertError {
    /// The fields do not fit in [`MAX_ADV_DATA`] bytes.
    Overflow,
    /// An AD structure length overruns the data, or a field has an invalid
    /// layout (e.g. manufacturer data shorter than a company id).
    Malformed,
}

/// What to do with a local name that does not fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NamePolicy {
    /// Truncate to fit and send as Shortened Local Name.
    #[default]
    Shorten,
    /// Fail with [`AdvertError::Overflow`].
    Exact,
}

/// Built advertising data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdvertisingData {
    buf: [u8; MAX_ADV_DATA],
    len: usize,
}

impl AdvertisingData {
    const EMPTY: Self = AdvertisingData {
        buf: [0; MAX_ADV_DATA],
        len: 0,
    };

    /// The encoded bytes.
    pub fn as_bytes(&self) -> &[u8] {
        self.buf.get(..self.len).unwrap_or(&[])
    }

    /// Appends one AD structure built from `parts`, returning the new value.
    fn push(self, ad_type: u8, parts: &[&[u8]]) -> Result<Self, AdvertError> {
        let body: usize = parts.iter().map(|p| p.len()).sum();
        let ad_len = u8::try_from(body + 1).map_err(|_| AdvertError::Overflow)?;
        let end = self.len + 2 + body;
        let mut buf = self.buf;
        let slot = buf.get_mut(self.len..end).ok_or(AdvertError::Overflow)?;
        let (head, mut rest) = slot.split_at_mut_checked(2).ok_or(AdvertError::Overflow)?;
        head.copy_from_slice(&[ad_len, ad_type]);
        for p in parts {
            let (dst, tail) = core::mem::take(&mut rest)
                .split_at_mut_checked(p.len())
                .ok_or(AdvertError::Overflow)?;
            dst.copy_from_slice(p);
            rest = tail;
        }
        Ok(AdvertisingData { buf, len: end })
    }

    const fn remaining(&self) -> usize {
        MAX_ADV_DATA - self.len
    }
}

/// Builds legacy advertising data. Every setter consumes and returns the
/// builder; [`AdvertisementBuilder::build`] does not consume it, so one
/// builder serves as a template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdvertisementBuilder<'a> {
    flags: Option<u8>,
    service_uuid: bool,
    name: Option<&'a str>,
    name_policy: NamePolicy,
    company_id: u16,
    manufacturer: Option<&'a [u8]>,
}

impl Default for AdvertisementBuilder<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> AdvertisementBuilder<'a> {
    /// Flags [`FLAGS_GENERAL_DISCOVERABLE`], nothing else, company id
    /// [`TEST_COMPANY_ID`], name policy [`NamePolicy::Shorten`].
    pub const fn new() -> Self {
        AdvertisementBuilder {
            flags: Some(FLAGS_GENERAL_DISCOVERABLE),
            service_uuid: false,
            name: None,
            name_policy: NamePolicy::Shorten,
            company_id: TEST_COMPANY_ID,
            manufacturer: None,
        }
    }

    /// Sets the Flags value, or omits the field with `None`.
    pub const fn flags(self, flags: Option<u8>) -> Self {
        AdvertisementBuilder { flags, ..self }
    }

    /// Includes [`SERVICE_UUID`] as the complete 128-bit service list.
    pub const fn service_uuid(self, include: bool) -> Self {
        AdvertisementBuilder {
            service_uuid: include,
            ..self
        }
    }

    /// Sets the local name; an empty name is omitted.
    pub const fn name(self, name: &'a str) -> Self {
        AdvertisementBuilder {
            name: Some(name),
            ..self
        }
    }

    /// Chooses what happens to a name that does not fit.
    pub const fn name_policy(self, name_policy: NamePolicy) -> Self {
        AdvertisementBuilder {
            name_policy,
            ..self
        }
    }

    /// Sets the manufacturer data company id.
    pub const fn company_id(self, company_id: u16) -> Self {
        AdvertisementBuilder { company_id, ..self }
    }

    /// The configured company id.
    pub const fn get_company_id(&self) -> u16 {
        self.company_id
    }

    /// Sets the manufacturer specific data (after the company id).
    pub const fn manufacturer_data(self, data: &'a [u8]) -> Self {
        AdvertisementBuilder {
            manufacturer: Some(data),
            ..self
        }
    }

    /// Encodes the fields.
    pub fn build(&self) -> Result<AdvertisingData, AdvertError> {
        let mut ad = AdvertisingData::EMPTY;
        if let Some(flags) = self.flags {
            ad = ad.push(AD_FLAGS, &[&[flags]])?;
        }
        if self.service_uuid {
            ad = ad.push(AD_COMPLETE_UUID128, &[&SERVICE_UUID.to_le_bytes()])?;
        }
        if let Some(data) = self.manufacturer {
            ad = ad.push(
                AD_MANUFACTURER_DATA,
                &[&self.company_id.to_le_bytes(), data],
            )?;
        }
        match self.name {
            Some(name) if !name.is_empty() => {
                let room = ad.remaining().saturating_sub(2);
                if name.len() <= room {
                    ad.push(AD_COMPLETE_LOCAL_NAME, &[name.as_bytes()])
                } else if self.name_policy == NamePolicy::Shorten {
                    let cut = (1..=room).rev().find(|i| name.is_char_boundary(*i));
                    let short = cut
                        .and_then(|i| name.get(..i))
                        .ok_or(AdvertError::Overflow)?;
                    ad.push(AD_SHORTENED_LOCAL_NAME, &[short.as_bytes()])
                } else {
                    Err(AdvertError::Overflow)
                }
            }
            _ => Ok(ad),
        }
    }
}

/// One AD structure, borrowing the advertising data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdStructure<'a> {
    /// The AD type.
    pub ad_type: u8,
    /// The AD data (after the type byte).
    pub data: &'a [u8],
}

/// Iterator over AD structures; see [`parse`].
#[derive(Debug, Clone)]
pub struct AdStructures<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for AdStructures<'a> {
    type Item = Result<AdStructure<'a>, AdvertError>;

    fn next(&mut self) -> Option<Self::Item> {
        let (&len, tail) = self.rest.split_first()?;
        if len == 0 {
            self.rest = &[];
            return None;
        }
        match tail.split_at_checked(usize::from(len)) {
            Some(([ad_type, data @ ..], rest)) => {
                self.rest = rest;
                Some(Ok(AdStructure {
                    ad_type: *ad_type,
                    data,
                }))
            }
            _ => {
                self.rest = &[];
                Some(Err(AdvertError::Malformed))
            }
        }
    }
}

/// Iterates the AD structures in `data` without copying.
pub fn parse(data: &[u8]) -> AdStructures<'_> {
    AdStructures { rest: data }
}

/// Checks that every AD structure length is consistent.
pub fn validate(data: &[u8]) -> Result<(), AdvertError> {
    parse(data).try_for_each(|s| s.map(|_| ()))
}

/// The first manufacturer data field as `(company id, data)`.
pub fn manufacturer_data(data: &[u8]) -> Result<Option<(u16, &[u8])>, AdvertError> {
    for s in parse(data) {
        let s = s?;
        if s.ad_type == AD_MANUFACTURER_DATA {
            return match s.data {
                [lo, hi, rest @ ..] => Ok(Some((u16::from_le_bytes([*lo, *hi]), rest))),
                _ => Err(AdvertError::Malformed),
            };
        }
    }
    Ok(None)
}

/// True when a complete or incomplete 128-bit UUID list holds [`SERVICE_UUID`].
pub fn advertises_service(data: &[u8]) -> Result<bool, AdvertError> {
    let wanted = SERVICE_UUID.to_le_bytes();
    for s in parse(data) {
        let s = s?;
        if s.ad_type == AD_COMPLETE_UUID128 || s.ad_type == AD_INCOMPLETE_UUID128 {
            let (uuids, rest) = s.data.as_chunks::<16>();
            if !rest.is_empty() {
                return Err(AdvertError::Malformed);
            }
            if uuids.contains(&wanted) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// A local name found in advertising data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalName<'a> {
    /// The name text.
    pub name: &'a str,
    /// False when the advertiser sent a Shortened Local Name.
    pub complete: bool,
}

/// The first complete or shortened local name. Non-UTF-8 names are malformed.
pub fn local_name(data: &[u8]) -> Result<Option<LocalName<'_>>, AdvertError> {
    for s in parse(data) {
        let s = s?;
        let complete = match s.ad_type {
            AD_COMPLETE_LOCAL_NAME => true,
            AD_SHORTENED_LOCAL_NAME => false,
            _ => continue,
        };
        let name = core::str::from_utf8(s.data).map_err(|_| AdvertError::Malformed)?;
        return Ok(Some(LocalName { name, complete }));
    }
    Ok(None)
}

/// A 48-bit device address, in the order the radio reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Address(pub [u8; 6]);

impl Address {
    /// The transport-neutral sender id of this address
    /// ([`SenderId::from_mac`]), as put in [`SampleMeta::sender`].
    pub const fn sender(self) -> SenderId {
        SenderId::from_mac(self.0)
    }
}

impl From<Address> for SenderId {
    fn from(address: Address) -> Self {
        address.sender()
    }
}

/// One received advertisement; `data` borrows the caller's buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanReport<'b> {
    /// The advertiser's address.
    pub address: Address,
    /// Received signal strength in dBm.
    pub rssi: i8,
    /// The advertising data.
    pub data: &'b [u8],
}

/// Radio side of advertising.
///
/// An implementation need not own the advertising set. A binding that shares
/// one set with other traffic (a beacon, another protocol) may queue the data
/// and put it on air later, for a slot of its choosing, or coalesce it with
/// queued data on the same key. `Ok` therefore means the data was accepted,
/// not that it is on air; a refusal (such as a full queue) is an `Err`, and
/// [`AdvertisingPublisher`] then does not advance its sequence number.
pub trait Advertiser {
    /// Binding-specific failure.
    type Error: core::fmt::Debug;

    /// Hands complete legacy advertising data to the radio: it replaces what
    /// is on air, or is queued to go on air (see the trait docs).
    async fn set_advertising_data(&mut self, data: &[u8]) -> Result<(), Self::Error>;
}

/// Radio side of scanning.
///
/// Both methods copy the report's advertising data into `buf`
/// ([`MAX_ADV_DATA`] bytes are offered).
pub trait Scanner {
    /// Binding-specific failure.
    type Error: core::fmt::Debug;

    /// Waits for the next advertisement. Must be cancel-safe: dropping the
    /// future before it is ready loses no report.
    async fn next_report<'b>(&mut self, buf: &'b mut [u8]) -> Result<ScanReport<'b>, Self::Error>;

    /// Returns the next advertisement if one was already received, without
    /// waiting. A binding fed by a synchronous radio callback queues reports
    /// there and pops one here; [`ScanQueue`] is such a queue.
    fn try_next_report<'b>(
        &mut self,
        buf: &'b mut [u8],
    ) -> Result<Option<ScanReport<'b>>, Self::Error>;
}

/// Default capacity of a [`ScanQueue`].
pub const DEFAULT_SCAN_QUEUE: usize = 8;

/// Why a [`ScanQueue`] refused or lost a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanQueueError {
    /// The pushed advertising data is longer than [`MAX_ADV_DATA`]; the
    /// report is dropped (and counted).
    TooLong,
    /// The caller's buffer is shorter than the popped report; the report is
    /// dropped (and counted).
    BufferTooSmall,
}

/// What [`ScanQueue::push_report`] did with a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pushed {
    /// Queued with room to spare.
    Queued,
    /// Queued after dropping (and counting) the oldest report, because the
    /// queue was full: the newest reports win.
    DisplacedOldest,
}

/// One scan report held by value: [`ScanReport`] with its data copied in.
///
/// What [`ScanQueue::pop_report`] and [`ScanQueue::poll_report`] return, so
/// a caller that shares the queue behind its own lock can take a report out
/// under the lock and use it after releasing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueuedReport {
    address: Address,
    rssi: i8,
    len: u8,
    data: [u8; MAX_ADV_DATA],
}

impl QueuedReport {
    const EMPTY: Self = QueuedReport {
        address: Address([0; 6]),
        rssi: 0,
        len: 0,
        data: [0; MAX_ADV_DATA],
    };

    /// Copies `report`.
    ///
    /// # Errors
    ///
    /// [`ScanQueueError::TooLong`] when its data exceeds [`MAX_ADV_DATA`].
    pub fn new(report: &ScanReport<'_>) -> Result<Self, ScanQueueError> {
        let mut data = [0u8; MAX_ADV_DATA];
        data.get_mut(..report.data.len())
            .ok_or(ScanQueueError::TooLong)?
            .copy_from_slice(report.data);
        Ok(QueuedReport {
            address: report.address,
            rssi: report.rssi,
            // Lossless: at most MAX_ADV_DATA (31) bytes were copied.
            len: report.data.len() as u8,
            data,
        })
    }

    /// The report, borrowing its data from `self`.
    pub fn as_report(&self) -> ScanReport<'_> {
        ScanReport {
            address: self.address,
            rssi: self.rssi,
            data: self.data.get(..usize::from(self.len)).unwrap_or(&[]),
        }
    }

    /// The report with its data copied into `buf`.
    ///
    /// # Errors
    ///
    /// [`ScanQueueError::BufferTooSmall`] when `buf` is shorter than the data.
    pub fn copy_into<'b>(&self, buf: &'b mut [u8]) -> Result<ScanReport<'b>, ScanQueueError> {
        let report = self.as_report();
        let out = buf
            .get_mut(..report.data.len())
            .ok_or(ScanQueueError::BufferTooSmall)?;
        out.copy_from_slice(report.data);
        Ok(ScanReport {
            address: report.address,
            rssi: report.rssi,
            data: out,
        })
    }
}

/// A bounded, waker-based queue of scan reports between a radio callback and
/// a [`Scanner`] consumer. No heap, no locks: it holds up to `R` reports
/// inline (default [`DEFAULT_SCAN_QUEUE`]; `R` must be at least 1) and one
/// [`Waker`].
///
/// * **Producer.** The radio's scan callback calls
///   [`ScanQueue::push_report`] (`&mut self`, synchronous, never blocks). A
///   full queue drops its oldest report to make room ([`Pushed::DisplacedOldest`]);
///   data longer than [`MAX_ADV_DATA`] is refused ([`ScanQueueError::TooLong`]).
///   Both are counted in [`ScanQueue::dropped`]. A push wakes the registered
///   waker, if any, before it returns; waking only schedules the waiting
///   task, it never runs it inline.
/// * **Consumer.** The [`Scanner`] implementation awaits a report:
///   [`Scanner::next_report`] pops one if queued, otherwise registers the
///   task's waker and waits for a push. [`Scanner::try_next_report`] pops
///   without waiting. The queue keeps one waker, so it serves one waiting
///   consumer; a second one replaces the first's registration.
/// * **Ownership.** Producer and consumer may be one owner, for example a
///   task that holds an [`AdvertisingSubscriber`] over the queue, pushes
///   through [`AdvertisingSubscriber::scanner_mut`] and then drains with
///   `try_recv`. Or the caller splits them with its own synchronisation (a
///   critical section, a `RefCell` on one executor): put the queue behind
///   that lock and give the consumer a small [`Scanner`] that, on each poll,
///   takes the lock and calls [`ScanQueue::poll_report`], which hands back an
///   owned [`QueuedReport`] so nothing borrowed outlives the lock.
/// * **Cancel safety.** A report is removed only in the same poll that
///   returns it as ready, so dropping a pending `next_report` (or a pending
///   `recv` over the queue) loses no report; a later call finds it. A
///   popped report that does not fit the caller's buffer is dropped with
///   [`ScanQueueError::BufferTooSmall`] (and counted); the subscribers in
///   this module always offer [`MAX_ADV_DATA`] bytes, so they never see it.
#[derive(Debug, Clone)]
pub struct ScanQueue<const R: usize = DEFAULT_SCAN_QUEUE> {
    slots: [QueuedReport; R],
    head: usize,
    len: usize,
    dropped: u32,
    waker: Option<Waker>,
}

impl<const R: usize> Default for ScanQueue<R> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const R: usize> ScanQueue<R> {
    /// Compile-time check that the queue can hold a report.
    const CAPACITY_OK: () = assert!(R > 0, "ScanQueue capacity must be at least 1");

    /// An empty queue with no waker registered.
    pub const fn new() -> Self {
        let () = Self::CAPACITY_OK;
        ScanQueue {
            slots: [QueuedReport::EMPTY; R],
            head: 0,
            len: 0,
            dropped: 0,
            waker: None,
        }
    }

    /// Queues `report` (data copied) and wakes the waiting consumer. A full
    /// queue drops its oldest report first. Does not block and does not
    /// consume the queue, so a radio callback calls it in place.
    ///
    /// # Errors
    ///
    /// [`ScanQueueError::TooLong`] when the data exceeds [`MAX_ADV_DATA`];
    /// the report is dropped and counted, and the queue is otherwise
    /// unchanged.
    pub fn push_report(&mut self, report: &ScanReport<'_>) -> Result<Pushed, ScanQueueError> {
        let queued = match QueuedReport::new(report) {
            Ok(q) => q,
            Err(e) => {
                self.dropped = self.dropped.wrapping_add(1);
                return Err(e);
            }
        };
        let pushed = if self.len >= R {
            self.head = (self.head + 1) % R;
            self.len -= 1;
            self.dropped = self.dropped.wrapping_add(1);
            Pushed::DisplacedOldest
        } else {
            Pushed::Queued
        };
        if let Some(slot) = self.slots.get_mut((self.head + self.len) % R) {
            *slot = queued;
            self.len += 1;
        }
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
        Ok(pushed)
    }

    /// Removes and returns the oldest report, without waiting.
    pub fn pop_report(&mut self) -> Option<QueuedReport> {
        if self.len == 0 {
            return None;
        }
        let report = self.slots.get(self.head).copied()?;
        self.head = (self.head + 1) % R;
        self.len -= 1;
        Some(report)
    }

    /// Pops the oldest report, or registers `cx`'s waker to be woken by the
    /// next [`ScanQueue::push_report`] and returns `Pending`. A report is
    /// removed only when `Ready` is returned (see the cancel-safety notes).
    pub fn poll_report(&mut self, cx: &mut Context<'_>) -> Poll<QueuedReport> {
        if let Some(report) = self.pop_report() {
            return Poll::Ready(report);
        }
        match &self.waker {
            Some(w) if w.will_wake(cx.waker()) => {}
            _ => self.waker = Some(cx.waker().clone()),
        }
        Poll::Pending
    }

    /// Reports waiting.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// True if no report is waiting.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The number of reports the queue holds, `R`.
    pub const fn capacity(&self) -> usize {
        R
    }

    /// Reports dropped so far (displaced by a full queue, too long, or not
    /// fitting the consumer's buffer); wraps at `u32::MAX`.
    pub const fn dropped(&self) -> u32 {
        self.dropped
    }
}

/// Waits through [`ScanQueue::poll_report`]; see [`ScanQueue`] for the
/// waking and cancel-safety rules.
impl<const R: usize> Scanner for ScanQueue<R> {
    type Error = ScanQueueError;

    async fn next_report<'b>(
        &mut self,
        buf: &'b mut [u8],
    ) -> Result<ScanReport<'b>, ScanQueueError> {
        let report = core::future::poll_fn(|cx| self.poll_report(cx)).await;
        self.deliver(&report, buf)
    }

    fn try_next_report<'b>(
        &mut self,
        buf: &'b mut [u8],
    ) -> Result<Option<ScanReport<'b>>, ScanQueueError> {
        match self.pop_report() {
            Some(report) => self.deliver(&report, buf).map(Some),
            None => Ok(None),
        }
    }
}

impl<const R: usize> ScanQueue<R> {
    /// Copies a popped report into `buf`, counting it as dropped if it does
    /// not fit.
    fn deliver<'b>(
        &mut self,
        report: &QueuedReport,
        buf: &'b mut [u8],
    ) -> Result<ScanReport<'b>, ScanQueueError> {
        report.copy_into(buf).inspect_err(|_| {
            self.dropped = self.dropped.wrapping_add(1);
        })
    }
}

/// Failure of an advertising publisher or subscriber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvertisingError<E> {
    /// The radio failed or refused the data.
    Radio(E),
    /// The advertising data could not be built (payload too large).
    Advert(AdvertError),
    /// The caller's buffer cannot hold the sample (the sample is dropped).
    BufferTooSmall,
    /// [`MAX_IGNORED_REPORTS`] reports passed without a matching one.
    ReportLimit,
}

/// Largest payload a publish carries with the default template (Flags plus
/// manufacturer data): 31 - 3 (Flags) - 4 (manufacturer AD header and company
/// id) - [`PUBLISH_HEADER_LEN`] = 21 bytes. Other templates leave a
/// different amount ([`AdvertisingPublisher::max_payload`]).
pub const MAX_ADVERT_PAYLOAD: usize = MAX_ADV_DATA - 3 - 4 - PUBLISH_HEADER_LEN;

/// Broadcasts publications as manufacturer data.
pub struct AdvertisingPublisher<'a, A> {
    advertiser: A,
    template: AdvertisementBuilder<'a>,
    sequence: Sequence,
}

/// A default advertiser with the default template
/// ([`AdvertisementBuilder::new`]), sequence 0.
impl<A: Advertiser + Default> Default for AdvertisingPublisher<'_, A> {
    fn default() -> Self {
        Self::new(A::default(), AdvertisementBuilder::new())
    }
}

impl<'a, A: Advertiser> AdvertisingPublisher<'a, A> {
    /// Publishes through `advertiser`; `template` supplies flags, name,
    /// service UUID and company id, and its manufacturer data is replaced.
    pub const fn new(advertiser: A, template: AdvertisementBuilder<'a>) -> Self {
        AdvertisingPublisher {
            advertiser,
            template,
            sequence: Sequence::new(0),
        }
    }

    /// Replaces the template (builder form, usable in `const` context).
    pub const fn with_template(mut self, template: AdvertisementBuilder<'a>) -> Self {
        self.template = template;
        self
    }

    /// The configured template.
    pub const fn template(&self) -> &AdvertisementBuilder<'a> {
        &self.template
    }

    /// The sequence number the next publish will carry.
    pub const fn sequence(&self) -> Sequence {
        self.sequence
    }

    /// The largest payload that fits beside the template's other fields, or
    /// `None` if not even an empty publish fits.
    pub fn max_payload(&self) -> Option<usize> {
        let zeros = [0u8; MAX_ADV_DATA];
        (0..=MAX_ADV_DATA - PUBLISH_HEADER_LEN).rev().find(|n| {
            zeros
                .get(..PUBLISH_HEADER_LEN + n)
                .is_some_and(|body| self.template.manufacturer_data(body).build().is_ok())
        })
    }

    /// Borrows the radio, e.g. to inspect a binding's queue.
    pub const fn advertiser(&self) -> &A {
        &self.advertiser
    }

    /// Borrows the radio mutably, e.g. to drive a binding's schedule.
    pub fn advertiser_mut(&mut self) -> &mut A {
        &mut self.advertiser
    }
}

impl<A: Advertiser> Publisher for AdvertisingPublisher<'_, A> {
    type Error = AdvertisingError<A::Error>;

    /// Keys are hashed to a short id, so any valid key fits; the payload
    /// limit is [`MAX_ADVERT_PAYLOAD`] (default template).
    const LIMITS: Limits = Limits::new(MAX_KEY_LEN, MAX_ADVERT_PAYLOAD);

    async fn publish(&mut self, key: Key<'_>, payload: &[u8]) -> Result<(), Self::Error> {
        let mut mfr = [0u8; MAX_ADV_DATA];
        let n = PUBLISH_HEADER_LEN + payload.len();
        let slot = mfr
            .get_mut(..n)
            .ok_or(AdvertisingError::Advert(AdvertError::Overflow))?;
        let (head, body) = slot.split_at_mut(PUBLISH_HEADER_LEN);
        let id = short_id(key).to_le_bytes();
        head.copy_from_slice(&[id[0], id[1], self.sequence.get()]);
        body.copy_from_slice(payload);
        let ad = self
            .template
            .manufacturer_data(slot)
            .build()
            .map_err(AdvertisingError::Advert)?;
        self.advertiser
            .set_advertising_data(ad.as_bytes())
            .await
            .map_err(AdvertisingError::Radio)?;
        self.sequence = self.sequence.next();
        Ok(())
    }
}

/// Why a key or filter was not added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortIdError {
    /// The table already holds `K` keys, or the subscriber's filter list
    /// already holds `F` filters.
    Full,
    /// The key is longer than the `L`-byte key slots.
    TooLong,
    /// A different learned key already has this short id, so adverts could
    /// not tell the two apart.
    Collision(u16),
}

/// An owned key of at most `L` bytes, or [`ShortIdError::TooLong`].
fn key_buf<const L: usize>(key: Key<'_>) -> Result<KeyBuf<L>, ShortIdError> {
    KeyBuf::from_key(key).map_err(|_| ShortIdError::TooLong)
}

/// An owned filter whose key fits `L` bytes, or [`ShortIdError::TooLong`].
fn filter_buf<const L: usize, Q: AsKey>(
    filter: &Filter<Q>,
) -> Result<KeyFilterBuf<L>, ShortIdError> {
    filter.to_buf().map_err(|_| ShortIdError::TooLong)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Learned<const L: usize> {
    id: u16,
    key: KeyBuf<L>,
}

/// Learned key texts, resolving advert short ids back to keys.
///
/// Holds up to `K` keys of at most `L` bytes each (default
/// [`MAX_KEY_LEN`], so any key fits; `L` may not exceed it), stored inline
/// as [`KeyBuf`]s. No two learned keys share a short id. Transitions
/// consume the table and return the next one. [`ShortIdTable::new`] is a
/// `const fn` and [`Default`] is the same empty table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortIdTable<const K: usize, const L: usize = MAX_KEY_LEN> {
    entries: [Option<Learned<L>>; K],
}

impl<const K: usize, const L: usize> Default for ShortIdTable<K, L> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const K: usize, const L: usize> ShortIdTable<K, L> {
    /// A table that knows no key.
    pub const fn new() -> Self {
        ShortIdTable { entries: [None; K] }
    }

    /// Adds `key`. Learning a key that is already present returns the table
    /// unchanged.
    ///
    /// # Errors
    ///
    /// [`ShortIdError::Collision`] when a different learned key has the same
    /// short id, [`ShortIdError::TooLong`] when the key exceeds `L` bytes,
    /// [`ShortIdError::Full`] when `K` keys are held.
    pub fn learn(self, key: Key<'_>) -> Result<Self, ShortIdError> {
        let id = short_id(key);
        match self.entries.iter().flatten().find(|e| e.id == id) {
            Some(e) if e.key == key => return Ok(self),
            Some(_) => return Err(ShortIdError::Collision(id)),
            None => {}
        }
        let key = key_buf(key)?;
        let mut entries = self.entries;
        let slot = entries
            .iter_mut()
            .find(|e| e.is_none())
            .ok_or(ShortIdError::Full)?;
        *slot = Some(Learned { id, key });
        Ok(ShortIdTable { entries })
    }

    /// Removes `key` if it is held.
    pub fn forget(self, key: Key<'_>) -> Self {
        let mut entries = self.entries;
        for e in entries.iter_mut() {
            if matches!(e, Some(l) if l.key == key) {
                *e = None;
            }
        }
        ShortIdTable { entries }
    }

    /// The learned key with short id `id`.
    pub fn resolve(&self, id: u16) -> Option<Key<'_>> {
        self.entries
            .iter()
            .flatten()
            .find(|e| e.id == id)
            .map(|e| e.key.as_key())
    }

    /// True when `key` was learned.
    pub fn contains(&self, key: Key<'_>) -> bool {
        self.entries.iter().flatten().any(|e| e.key == key)
    }

    /// The learned keys, in slot order.
    pub fn keys(&self) -> impl Iterator<Item = Key<'_>> {
        self.entries.iter().flatten().map(|e| e.key.as_key())
    }

    /// Number of learned keys.
    pub fn len(&self) -> usize {
        self.entries.iter().flatten().count()
    }

    /// True when no key was learned.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The number of keys the table holds, `K`.
    pub const fn capacity(&self) -> usize {
        K
    }

    fn resolve_buf(&self, id: u16) -> Option<KeyBuf<L>> {
        self.entries
            .iter()
            .flatten()
            .find(|e| e.id == id)
            .map(|e| e.key)
    }
}

/// Default number of (sender, key) pairs an [`AdvertisingSubscriber`]
/// remembers for duplicate suppression.
pub const DEFAULT_SENDERS: usize = 16;

/// An accepted report, before it is copied into the caller's buffer.
struct Hit<'r, const L: usize> {
    key: KeyBuf<L>,
    meta: SampleMeta,
    payload: &'r [u8],
}

impl<const L: usize> Hit<'_, L> {
    fn deliver<'b, E>(self, buf: &'b mut [u8]) -> Result<Sample<'b>, AdvertisingError<E>> {
        sample_into(buf, self.key.as_str(), self.payload)
            .map(|s| s.with_meta(self.meta))
            .ok_or(AdvertisingError::BufferTooSmall)
    }
}

/// Receives publications by scanning (see the module docs).
///
/// * `K` bounds the learned keys (its [`ShortIdTable`]);
/// * `L` is the longest key text held (default [`MAX_KEY_LEN`], which it
///   may not exceed);
/// * `D` is the number of (sender, key) pairs remembered for duplicate
///   suppression (default [`DEFAULT_SENDERS`]);
/// * `F` bounds the filters, stored as owned [`KeyFilterBuf`]s (default `K`,
///   the 0.2 behaviour where one bound covered both).
///
/// [`AdvertisingSubscriber::new`] and [`AdvertisingSubscriber::with_company_id`]
/// are `const fn`, so a subscriber can be built in a `static` initialiser;
/// [`Default`] wraps a default scanner.
pub struct AdvertisingSubscriber<
    S,
    const K: usize,
    const L: usize = MAX_KEY_LEN,
    const D: usize = DEFAULT_SENDERS,
    const F: usize = K,
> {
    scanner: S,
    keys: ShortIdTable<K, L>,
    filters: [Option<KeyFilterBuf<L>>; F],
    company_id: u16,
    seen: SeenTable<D>,
}

impl<S: Scanner + Default, const K: usize, const L: usize, const D: usize, const F: usize> Default
    for AdvertisingSubscriber<S, K, L, D, F>
{
    fn default() -> Self {
        Self::new(S::default())
    }
}

impl<S: Scanner, const K: usize, const L: usize, const D: usize, const F: usize>
    AdvertisingSubscriber<S, K, L, D, F>
{
    /// A subscriber with no filters (it delivers nothing until one is added),
    /// accepting manufacturer data from [`TEST_COMPANY_ID`].
    pub const fn new(scanner: S) -> Self {
        AdvertisingSubscriber {
            scanner,
            keys: ShortIdTable::new(),
            filters: [None; F],
            company_id: TEST_COMPANY_ID,
            seen: SeenTable::new(),
        }
    }

    /// Accepts manufacturer data from `company_id` instead.
    pub const fn with_company_id(mut self, company_id: u16) -> Self {
        self.company_id = company_id;
        self
    }

    /// The accepted company id.
    pub const fn company_id(&self) -> u16 {
        self.company_id
    }

    /// Builder form of [`AdvertisingSubscriber::subscribe_key`].
    ///
    /// # Errors
    ///
    /// As [`AdvertisingSubscriber::subscribe_key`].
    pub fn with_filter<Q: AsKey>(mut self, filter: Filter<Q>) -> Result<Self, ShortIdError> {
        self.subscribe_key(filter)?;
        Ok(self)
    }

    /// Starts delivering keys matched by `filter`, a borrowed [`KeyFilter`]
    /// or an owned [`KeyFilterBuf`]; the filter is copied into the
    /// subscriber, so the caller's key need not outlive the call. An exact
    /// filter also learns its key. A prefix filter matches only learned keys
    /// ([`AdvertisingSubscriber::learn`]). Adding a filter that is already
    /// present changes nothing. On error the subscriber is unchanged.
    ///
    /// # Errors
    ///
    /// [`ShortIdError::Full`] when `F` filters (or, for an exact filter, `K`
    /// keys) are held, [`ShortIdError::TooLong`] for a key longer than `L`,
    /// [`ShortIdError::Collision`] when an exact key's short id is taken.
    pub fn subscribe_key<Q: AsKey>(&mut self, filter: Filter<Q>) -> Result<(), ShortIdError> {
        let stored = filter_buf::<L, Q>(&filter)?;
        let keys = self.learned_for(&stored)?;
        let mut filters = self.filters;
        if !filters.contains(&Some(stored)) {
            let slot = filters
                .iter_mut()
                .find(|f| f.is_none())
                .ok_or(ShortIdError::Full)?;
            *slot = Some(stored);
        }
        self.keys = keys;
        self.filters = filters;
        Ok(())
    }

    /// Stops delivering keys matched by `filter` (an identical filter must
    /// have been added). Learned keys are kept.
    pub fn unsubscribe_key<Q: AsKey>(&mut self, filter: Filter<Q>) {
        let Ok(stored) = filter_buf::<L, Q>(&filter) else {
            return;
        };
        for f in self.filters.iter_mut() {
            if *f == Some(stored) {
                *f = None;
            }
        }
    }

    /// The filters held, borrowing their keys from the subscriber.
    pub fn filters(&self) -> impl Iterator<Item = KeyFilter<'_>> {
        self.filters.iter().flatten().map(|f| f.as_filter())
    }

    /// Learns `key` so its adverts can be resolved and matched by prefix
    /// filters. On error the subscriber is unchanged.
    ///
    /// # Errors
    ///
    /// As [`ShortIdTable::learn`].
    pub fn learn(&mut self, key: Key<'_>) -> Result<(), ShortIdError> {
        self.keys = self.keys.clone().learn(key)?;
        Ok(())
    }

    /// The learned keys.
    pub const fn keys(&self) -> &ShortIdTable<K, L> {
        &self.keys
    }

    /// Borrows the radio, e.g. to inspect a [`ScanQueue`].
    pub const fn scanner(&self) -> &S {
        &self.scanner
    }

    /// Borrows the radio mutably, e.g. to
    /// [`push_report`](ScanQueue::push_report) into a [`ScanQueue`] in place.
    pub fn scanner_mut(&mut self) -> &mut S {
        &mut self.scanner
    }

    /// A filtered view delivering only keys matched by `filter`, borrowing
    /// the subscriber (what [`Subscribe::subscribe`] returns). The filter is
    /// copied into the view and is not added to the subscriber's own
    /// filters; an exact filter's key is learned, and stays learned. While
    /// the view receives, reports on other keys are consumed and dropped, as
    /// for any single-stream subscriber. On error the subscriber is
    /// unchanged.
    ///
    /// # Errors
    ///
    /// [`ShortIdError::TooLong`] for a key longer than `L`; for an exact
    /// filter, [`ShortIdError::Full`] when `K` keys are held and
    /// [`ShortIdError::Collision`] when its short id is taken.
    pub fn subscription<Q: AsKey>(
        &mut self,
        filter: Filter<Q>,
    ) -> Result<AdvertisingSubscription<'_, S, K, L, D, F>, ShortIdError> {
        let filter = filter_buf::<L, Q>(&filter)?;
        self.keys = self.learned_for(&filter)?;
        Ok(AdvertisingSubscription {
            subscriber: self,
            filter,
        })
    }

    /// The key table after adding `filter`'s key if it is exact.
    fn learned_for(&self, filter: &KeyFilterBuf<L>) -> Result<ShortIdTable<K, L>, ShortIdError> {
        match filter {
            Filter::Exact(key) => self.keys.clone().learn(key.as_key()),
            Filter::Prefix(_) => Ok(self.keys.clone()),
        }
    }

    /// Decides whether `report` is a fresh sample on a wanted key (matched by
    /// `only`, or by any held filter when `None`), recording its sequence
    /// number if it is.
    fn admit<'r>(
        &mut self,
        report: &ScanReport<'r>,
        only: Option<&KeyFilterBuf<L>>,
    ) -> Option<Hit<'r, L>> {
        let Ok(Some((company, [lo, hi, seq, payload @ ..]))) = manufacturer_data(report.data)
        else {
            return None;
        };
        if company != self.company_id {
            return None;
        }
        let owned = self.keys.resolve_buf(u16::from_le_bytes([*lo, *hi]))?;
        let key = owned.as_key();
        let wanted = match only {
            Some(f) => f.matches(key),
            None => self.filters.iter().flatten().any(|f| f.matches(key)),
        };
        if !wanted {
            return None;
        }
        let sender = report.address.sender();
        let (seen, fresh) = self.seen.observe(sender, key, *seq);
        self.seen = seen;
        fresh.then_some(Hit {
            key: owned,
            meta: SampleMeta {
                sender: Some(sender),
                seq: Some(*seq),
            },
            payload,
        })
    }

    /// [`Subscriber::recv`], restricted to `only` when given.
    async fn recv_where<'b>(
        &mut self,
        buf: &'b mut [u8],
        only: Option<&KeyFilterBuf<L>>,
    ) -> Result<Sample<'b>, AdvertisingError<S::Error>> {
        let mut adv = [0u8; MAX_ADV_DATA];
        for _ in 0..MAX_IGNORED_REPORTS {
            let report = self
                .scanner
                .next_report(&mut adv)
                .await
                .map_err(AdvertisingError::Radio)?;
            if let Some(hit) = self.admit(&report, only) {
                return hit.deliver(buf);
            }
        }
        Err(AdvertisingError::ReportLimit)
    }

    /// [`Subscriber::try_recv`], restricted to `only` when given.
    fn try_recv_where<'b>(
        &mut self,
        buf: &'b mut [u8],
        only: Option<&KeyFilterBuf<L>>,
    ) -> Result<Option<Sample<'b>>, AdvertisingError<S::Error>> {
        let mut adv = [0u8; MAX_ADV_DATA];
        for _ in 0..MAX_IGNORED_REPORTS {
            let Some(report) = self
                .scanner
                .try_next_report(&mut adv)
                .map_err(AdvertisingError::Radio)?
            else {
                return Ok(None);
            };
            if let Some(hit) = self.admit(&report, only) {
                return hit.deliver(buf).map(Some);
            }
        }
        Ok(None)
    }
}

impl<S: Scanner, const K: usize, const L: usize, const D: usize, const F: usize> Subscriber
    for AdvertisingSubscriber<S, K, L, D, F>
{
    type Error = AdvertisingError<S::Error>;

    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, Self::Error> {
        self.recv_where(buf, None).await
    }

    /// Drains already-received reports until one is a fresh sample. Returns
    /// `Ok(None)` when the scanner has no report, or after
    /// [`MAX_IGNORED_REPORTS`] unrelated ones (the rest are examined on the
    /// next call).
    fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, Self::Error> {
        self.try_recv_where(buf, None)
    }
}

/// Runtime subscriptions on one scanner: each is a filtered view borrowing
/// the subscriber ([`AdvertisingSubscriber::subscription`]).
impl<S: Scanner, const K: usize, const L: usize, const D: usize, const F: usize> Subscribe
    for AdvertisingSubscriber<S, K, L, D, F>
{
    type Subscriber<'a>
        = AdvertisingSubscription<'a, S, K, L, D, F>
    where
        Self: 'a;
    type Error = ShortIdError;

    async fn subscribe<'a>(
        &'a mut self,
        filter: KeyFilter<'_>,
    ) -> Result<AdvertisingSubscription<'a, S, K, L, D, F>, ShortIdError> {
        self.subscription(filter)
    }
}

/// A filtered view of an [`AdvertisingSubscriber`], delivering only keys its
/// own owned filter matches; see [`AdvertisingSubscriber::subscription`].
/// Bounds, dedup and cancel safety are the subscriber's.
pub struct AdvertisingSubscription<
    'a,
    S,
    const K: usize,
    const L: usize = MAX_KEY_LEN,
    const D: usize = DEFAULT_SENDERS,
    const F: usize = K,
> {
    subscriber: &'a mut AdvertisingSubscriber<S, K, L, D, F>,
    filter: KeyFilterBuf<L>,
}

impl<S: Scanner, const K: usize, const L: usize, const D: usize, const F: usize>
    AdvertisingSubscription<'_, S, K, L, D, F>
{
    /// The view's filter.
    pub fn filter(&self) -> KeyFilter<'_> {
        self.filter.as_filter()
    }
}

impl<S: Scanner, const K: usize, const L: usize, const D: usize, const F: usize> Subscriber
    for AdvertisingSubscription<'_, S, K, L, D, F>
{
    type Error = AdvertisingError<S::Error>;

    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, Self::Error> {
        self.subscriber.recv_where(buf, Some(&self.filter)).await
    }

    fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, Self::Error> {
        self.subscriber.try_recv_where(buf, Some(&self.filter))
    }
}

/// A discovered middleware node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Node<'b> {
    /// The node's address.
    pub address: Address,
    /// Signal strength of the report.
    pub rssi: i8,
    /// The advertised local name, if any.
    pub name: Option<LocalName<'b>>,
}

/// Returns the node behind `report` when it advertises [`SERVICE_UUID`];
/// malformed advertising data is not a node.
pub fn discover<'b>(report: &ScanReport<'b>) -> Option<Node<'b>> {
    if !advertises_service(report.data).ok()? {
        return None;
    }
    Some(Node {
        address: report.address,
        rssi: report.rssi,
        name: local_name(report.data).ok()?,
    })
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use core::future::Future;
    use futures::executor::block_on;
    use std::collections::VecDeque;
    use std::vec::Vec;

    const TEMP: Key<'static> = Key::from_static("sensor/temperature");
    const HUM: Key<'static> = Key::from_static("sensor/humidity");
    const ADDR: Address = Address([1, 2, 3, 4, 5, 6]);

    #[derive(Default)]
    struct FakeAdvertiser {
        set: Vec<Vec<u8>>,
    }

    impl Advertiser for FakeAdvertiser {
        type Error = ();
        async fn set_advertising_data(&mut self, data: &[u8]) -> Result<(), ()> {
            assert!(data.len() <= MAX_ADV_DATA);
            self.set.push(data.to_vec());
            Ok(())
        }
    }

    struct FakeScanner {
        reports: VecDeque<(Address, i8, Vec<u8>)>,
    }

    impl FakeScanner {
        fn new(data: Vec<Vec<u8>>) -> Self {
            Self::from(data.into_iter().map(|d| (ADDR, d)).collect())
        }

        fn from(data: Vec<(Address, Vec<u8>)>) -> Self {
            FakeScanner {
                reports: data.into_iter().map(|(a, d)| (a, -40, d)).collect(),
            }
        }

        fn pop<'b>(&mut self, buf: &'b mut [u8]) -> Option<ScanReport<'b>> {
            let (address, rssi, data) = self.reports.pop_front()?;
            let out = buf.get_mut(..data.len())?;
            out.copy_from_slice(&data);
            Some(ScanReport {
                address,
                rssi,
                data: out,
            })
        }
    }

    /// Scripted: an empty queue is an error for `next_report` (so a test
    /// cannot hang) and `None` for `try_next_report`.
    impl Scanner for FakeScanner {
        type Error = ();
        async fn next_report<'b>(&mut self, buf: &'b mut [u8]) -> Result<ScanReport<'b>, ()> {
            self.pop(buf).ok_or(())
        }
        fn try_next_report<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<ScanReport<'b>>, ()> {
            Ok(self.pop(buf))
        }
    }

    fn structures(data: &[u8]) -> Vec<(u8, Vec<u8>)> {
        parse(data)
            .map(|s| s.unwrap())
            .map(|s| (s.ad_type, s.data.to_vec()))
            .collect()
    }

    #[test]
    fn builder_emits_all_fields() {
        let ad = AdvertisementBuilder::new()
            .service_uuid(true)
            .manufacturer_data(&[0xAB])
            .name("mw")
            .build()
            .unwrap();
        let s = structures(ad.as_bytes());
        assert_eq!(s[0], (AD_FLAGS, std::vec![FLAGS_GENERAL_DISCOVERABLE]));
        assert_eq!(
            s[1],
            (AD_COMPLETE_UUID128, SERVICE_UUID.to_le_bytes().to_vec())
        );
        assert_eq!(s[2], (AD_MANUFACTURER_DATA, std::vec![0xFF, 0xFF, 0xAB]));
        assert_eq!(s[3], (AD_COMPLETE_LOCAL_NAME, b"mw".to_vec()));
        assert_eq!(ad.as_bytes().len(), 3 + 18 + 5 + 4);
    }

    #[test]
    fn builder_uses_configured_company_id() {
        let ad = AdvertisementBuilder::new()
            .flags(None)
            .company_id(0x0059)
            .manufacturer_data(b"hi")
            .build()
            .unwrap();
        assert_eq!(
            ad.as_bytes(),
            &[5, AD_MANUFACTURER_DATA, 0x59, 0x00, b'h', b'i']
        );
        assert_eq!(
            manufacturer_data(ad.as_bytes()),
            Ok(Some((0x0059, &b"hi"[..])))
        );
    }

    #[test]
    fn long_name_is_shortened_or_rejected() {
        let b = AdvertisementBuilder::new()
            .service_uuid(true)
            .name("middleware-node-01");
        // 31 - 3 (flags) - 18 (uuid) - 2 (name header) = 8 name bytes.
        let ad = b.build().unwrap();
        assert_eq!(ad.as_bytes().len(), MAX_ADV_DATA);
        assert_eq!(
            local_name(ad.as_bytes()),
            Ok(Some(LocalName {
                name: "middlewa",
                complete: false
            }))
        );
        assert_eq!(
            b.name_policy(NamePolicy::Exact).build(),
            Err(AdvertError::Overflow)
        );
        // Shortening respects UTF-8 boundaries: "é" is two bytes.
        let ad = AdvertisementBuilder::new()
            .service_uuid(true)
            .name("abcdefgé")
            .build()
            .unwrap();
        assert_eq!(local_name(ad.as_bytes()).unwrap().unwrap().name, "abcdefg");
        // No room for even one character.
        let full = AdvertisementBuilder::new()
            .service_uuid(true)
            .manufacturer_data(&[0; 4])
            .name("x");
        assert_eq!(full.build(), Err(AdvertError::Overflow));
    }

    #[test]
    fn oversized_fields_are_an_error() {
        let b = AdvertisementBuilder::new()
            .service_uuid(true)
            .manufacturer_data(&[0; 7]);
        assert_eq!(b.build(), Err(AdvertError::Overflow));
        assert!(
            AdvertisementBuilder::new()
                .service_uuid(true)
                .manufacturer_data(&[0; 6])
                .build()
                .is_ok()
        );
        let huge = [0u8; 300];
        assert_eq!(
            AdvertisementBuilder::new().manufacturer_data(&huge).build(),
            Err(AdvertError::Overflow)
        );
    }

    #[test]
    fn parser_rejects_malformed_lengths() {
        // Second structure claims 5 bytes but only 2 remain.
        let data = [2, AD_FLAGS, 6, 5, AD_MANUFACTURER_DATA, 1];
        let mut it = parse(&data);
        assert_eq!(
            it.next(),
            Some(Ok(AdStructure {
                ad_type: AD_FLAGS,
                data: &[6]
            }))
        );
        assert_eq!(it.next(), Some(Err(AdvertError::Malformed)));
        assert_eq!(it.next(), None);
        assert_eq!(validate(&data), Err(AdvertError::Malformed));
        assert_eq!(validate(&[1]), Err(AdvertError::Malformed));
        assert_eq!(manufacturer_data(&data), Err(AdvertError::Malformed));
        assert_eq!(advertises_service(&data), Err(AdvertError::Malformed));
        assert_eq!(
            local_name(&[3, AD_COMPLETE_LOCAL_NAME, 0xFF, 0xFE]),
            Err(AdvertError::Malformed)
        );
        assert_eq!(
            manufacturer_data(&[2, AD_MANUFACTURER_DATA, 1]),
            Err(AdvertError::Malformed)
        );
        assert_eq!(
            advertises_service(&[3, AD_COMPLETE_UUID128, 1, 2]),
            Err(AdvertError::Malformed)
        );
    }

    #[test]
    fn parser_stops_at_zero_padding() {
        let data = [2, AD_FLAGS, 6, 0, 0, 0];
        assert_eq!(structures(&data), std::vec![(AD_FLAGS, std::vec![6])]);
        assert_eq!(validate(&data), Ok(()));
        assert_eq!(validate(&[]), Ok(()));
    }

    #[test]
    fn publish_broadcasts_short_id_sequence_and_payload() {
        let mut p =
            AdvertisingPublisher::new(FakeAdvertiser::default(), AdvertisementBuilder::new());
        block_on(p.publish(TEMP, b"21.5")).unwrap();
        block_on(p.publish(TEMP, b"21.6")).unwrap();
        assert_eq!(p.sequence().get(), 2);
        let set = &p.advertiser_mut().set;
        let id = short_id(TEMP).to_le_bytes();
        let (company, data) = manufacturer_data(&set[0]).unwrap().unwrap();
        assert_eq!(company, TEST_COMPANY_ID);
        assert_eq!(data, &[id[0], id[1], 0, b'2', b'1', b'.', b'5']);
        let (_, data) = manufacturer_data(&set[1]).unwrap().unwrap();
        assert_eq!(data, &[id[0], id[1], 1, b'2', b'1', b'.', b'6']);
    }

    #[test]
    fn publish_rejects_payload_that_does_not_fit() {
        let template = AdvertisementBuilder::new();
        let mut p = AdvertisingPublisher::new(FakeAdvertiser::default(), template);
        // 31 - 3 (flags) - 4 (mfr header + company) - 3 (id + seq) = 21.
        assert!(block_on(p.publish(TEMP, &[0; 21])).is_ok());
        assert_eq!(
            block_on(p.publish(TEMP, &[0; 22])),
            Err(AdvertisingError::Advert(AdvertError::Overflow))
        );
        assert_eq!(
            block_on(p.publish(TEMP, &[0; 64])),
            Err(AdvertisingError::Advert(AdvertError::Overflow))
        );
        assert_eq!(
            p.sequence().get(),
            1,
            "failed publishes do not advance the sequence"
        );
    }

    /// An advertiser that queues instead of going on air and refuses when
    /// its one-slot queue is full.
    #[derive(Default)]
    struct QueueingAdvertiser {
        queued: Option<Vec<u8>>,
    }

    impl Advertiser for QueueingAdvertiser {
        type Error = &'static str;
        async fn set_advertising_data(&mut self, data: &[u8]) -> Result<(), &'static str> {
            if self.queued.is_some() {
                return Err("queue full");
            }
            self.queued = Some(data.to_vec());
            Ok(())
        }
    }

    #[test]
    fn queueing_advertiser_accepts_or_refuses() {
        let mut p =
            AdvertisingPublisher::new(QueueingAdvertiser::default(), AdvertisementBuilder::new());
        block_on(p.publish(TEMP, b"a")).unwrap();
        assert!(p.advertiser().queued.is_some(), "accepted means queued");
        assert_eq!(
            block_on(p.publish(TEMP, b"b")),
            Err(AdvertisingError::Radio("queue full"))
        );
        assert_eq!(
            p.sequence().get(),
            1,
            "a refusal does not advance the sequence"
        );
        // The binding drains its queue (goes on air) and accepts again.
        p.advertiser_mut().queued = None;
        block_on(p.publish(TEMP, b"b")).unwrap();
        assert_eq!(p.sequence().get(), 2);
    }

    #[test]
    fn publisher_limits_match_the_template() {
        type P = AdvertisingPublisher<'static, FakeAdvertiser>;
        assert_eq!(<P as Publisher>::LIMITS.max_payload, 21);
        assert_eq!(<P as Publisher>::LIMITS.max_key, MAX_KEY_LEN);
        let p = P::new(FakeAdvertiser::default(), AdvertisementBuilder::new());
        assert_eq!(p.max_payload(), Some(MAX_ADVERT_PAYLOAD));
        let bare = P::new(
            FakeAdvertiser::default(),
            AdvertisementBuilder::new().flags(None),
        );
        assert_eq!(bare.max_payload(), Some(24));
        // Service UUID (18) + Flags (3) + header (7) leave 3 bytes; a name
        // under the Shorten policy needs at least 3 more, so 0 remain.
        let full = AdvertisementBuilder::new().service_uuid(true);
        assert_eq!(
            P::new(FakeAdvertiser::default(), full).max_payload(),
            Some(3)
        );
        let named = full.name("n");
        assert_eq!(
            P::new(FakeAdvertiser::default(), named).max_payload(),
            Some(0)
        );
        let over = named.name_policy(NamePolicy::Exact).name("long-name");
        assert_eq!(P::new(FakeAdvertiser::default(), over).max_payload(), None);
        assert!(p.advertiser().set.is_empty());
    }

    fn mfr(company: u16, key: Key<'_>, seq: u8, payload: &[u8]) -> Vec<u8> {
        let mut body = short_id(key).to_le_bytes().to_vec();
        body.push(seq);
        body.extend_from_slice(payload);
        AdvertisementBuilder::new()
            .company_id(company)
            .manufacturer_data(&body)
            .build()
            .unwrap()
            .as_bytes()
            .to_vec()
    }

    type Sub<const K: usize> = AdvertisingSubscriber<FakeScanner, K>;

    fn exact<const K: usize>(scanner: FakeScanner, keys: &[Key<'_>]) -> Sub<K> {
        keys.iter()
            .try_fold(Sub::<K>::new(scanner), |s, k| {
                s.with_filter(KeyFilter::Exact(*k))
            })
            .unwrap()
    }

    #[test]
    fn scanning_subscriber_matches_keys_and_drops_repeats() {
        let reports = std::vec![
            mfr(TEST_COMPANY_ID, Key::from_static("other"), 0, b"no"),
            mfr(0x0059, TEMP, 0, b"wrong company"),
            std::vec![9, 1], // malformed
            AdvertisementBuilder::new()
                .build()
                .unwrap()
                .as_bytes()
                .to_vec(), // no mfr data
            mfr(TEST_COMPANY_ID, TEMP, 0, b"t0"),
            mfr(TEST_COMPANY_ID, TEMP, 0, b"t0"), // repeat → dropped
            mfr(TEST_COMPANY_ID, HUM, 0, b"h0"),  // same seq, other key → kept
            mfr(TEST_COMPANY_ID, TEMP, 1, b"t1"),
        ];
        let mut s = exact::<2>(FakeScanner::new(reports), &[TEMP, HUM]);
        let mut buf = [0u8; 64];
        let got: Vec<(Key<'static>, Vec<u8>, SampleMeta)> = (0..3)
            .map(|_| {
                let x = block_on(s.recv(&mut buf)).unwrap();
                (
                    if x.key == TEMP { TEMP } else { HUM },
                    x.payload.to_vec(),
                    x.meta,
                )
            })
            .collect();
        let meta = |seq| SampleMeta {
            sender: Some(SenderId::from_mac(ADDR.0)),
            seq: Some(seq),
        };
        assert_eq!(
            got,
            std::vec![
                (TEMP, b"t0".to_vec(), meta(0)),
                (HUM, b"h0".to_vec(), meta(0)),
                (TEMP, b"t1".to_vec(), meta(1))
            ]
        );
        assert_eq!(block_on(s.recv(&mut buf)), Err(AdvertisingError::Radio(())));
    }

    #[test]
    fn scanning_subscriber_errors() {
        let reports = std::vec![mfr(0x0059, TEMP, 3, b"payload")];
        let mut s = exact::<1>(FakeScanner::new(reports), &[TEMP]).with_company_id(0x0059);
        let mut small = [0u8; 4];
        assert_eq!(
            block_on(s.recv(&mut small)),
            Err(AdvertisingError::BufferTooSmall)
        );

        let junk = (0..MAX_IGNORED_REPORTS)
            .map(|_| mfr(TEST_COMPANY_ID, HUM, 0, b""))
            .collect();
        let mut s = exact::<1>(FakeScanner::new(junk), &[TEMP]);
        let mut buf = [0u8; 64];
        assert_eq!(
            block_on(s.recv(&mut buf)),
            Err(AdvertisingError::ReportLimit)
        );
    }

    #[test]
    fn dedup_is_per_sender() {
        let a = Address([0xA; 6]);
        let b = Address([0xB; 6]);
        let ad = mfr(TEST_COMPANY_ID, TEMP, 0, b"x");
        let reports = std::vec![
            (a, ad.clone()),
            (b, ad.clone()), // other sender, same key and seq → delivered
            (a, ad.clone()), // repeat from a → dropped
            (b, ad),         // repeat from b → dropped
        ];
        let mut s = exact::<1>(FakeScanner::from(reports), &[TEMP]);
        let mut buf = [0u8; 64];
        let first = block_on(s.recv(&mut buf)).unwrap().meta;
        assert_eq!(first.sender, Some(a.sender()));
        let second = block_on(s.recv(&mut buf)).unwrap().meta;
        assert_eq!(second.sender, Some(SenderId::from(b)));
        assert_eq!((first.seq, second.seq), (Some(0), Some(0)));
        assert_eq!(block_on(s.recv(&mut buf)), Err(AdvertisingError::Radio(())));
    }

    /// Two keys with the same short id, found by search.
    fn colliding_keys() -> (std::string::String, std::string::String) {
        let mut seen = std::collections::HashMap::new();
        for i in 0..100_000u32 {
            let k = std::format!("k/{i}");
            let id = short_id(Key::new(&k).unwrap());
            if let Some(prev) = seen.insert(id, k.clone()) {
                return (prev, k);
            }
        }
        panic!("no collision found");
    }

    #[test]
    fn short_id_table_learns_resolves_and_detects_collisions() {
        let t = ShortIdTable::<2>::new().learn(TEMP).unwrap();
        // Learning again is a no-op.
        let t = t.learn(TEMP).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t.resolve(short_id(TEMP)), Some(TEMP));
        assert_eq!(t.resolve(short_id(HUM)), None);
        let t = t.learn(HUM).unwrap();
        assert!(t.contains(HUM));
        assert_eq!(t.keys().collect::<Vec<_>>(), std::vec![TEMP, HUM]);
        assert_eq!(
            t.clone().learn(Key::from_static("x")),
            Err(ShortIdError::Full)
        );
        let t = t.forget(TEMP);
        assert_eq!(t.resolve(short_id(TEMP)), None);
        assert_eq!(t.len(), 1);

        let (a, b) = colliding_keys();
        let (a, b) = (Key::new(&a).unwrap(), Key::new(&b).unwrap());
        assert_ne!(a, b);
        let t = ShortIdTable::<4>::new().learn(a).unwrap();
        assert_eq!(
            t.clone().learn(b),
            Err(ShortIdError::Collision(short_id(a)))
        );
        assert_eq!(t.resolve(short_id(b)), Some(a));

        // Key text longer than the slot.
        assert_eq!(
            ShortIdTable::<1, 4>::new().learn(TEMP),
            Err(ShortIdError::TooLong)
        );
        assert!(ShortIdTable::<1, 4>::new().is_empty());
    }

    #[test]
    fn subscribe_key_is_atomic_and_bounded() {
        let mut s = Sub::<1>::new(FakeScanner::new(Vec::new()));
        s.subscribe_key(KeyFilter::Exact(TEMP)).unwrap();
        // Same filter again: nothing changes.
        s.subscribe_key(KeyFilter::Exact(TEMP)).unwrap();
        assert_eq!(
            s.subscribe_key(KeyFilter::Prefix(Key::from_static("app"))),
            Err(ShortIdError::Full)
        );
        // A failed exact subscribe leaves the learned keys unchanged.
        let mut s = Sub::<2>::new(FakeScanner::new(Vec::new()));
        s.subscribe_key(KeyFilter::Prefix(Key::from_static("a")))
            .unwrap();
        s.subscribe_key(KeyFilter::Prefix(Key::from_static("b")))
            .unwrap();
        assert_eq!(
            s.subscribe_key(KeyFilter::Exact(TEMP)),
            Err(ShortIdError::Full)
        );
        assert!(s.keys().is_empty());
        s.unsubscribe_key(KeyFilter::Prefix(Key::from_static("b")));
        s.subscribe_key(KeyFilter::Exact(TEMP)).unwrap();
        assert!(s.keys().contains(TEMP));
    }

    #[test]
    fn prefix_subscription_end_to_end() {
        let mut p =
            AdvertisingPublisher::new(FakeAdvertiser::default(), AdvertisementBuilder::new());
        let (x, y, z) = (
            Key::from_static("app/x"),
            Key::from_static("app/y"),
            Key::from_static("other/z"),
        );
        block_on(p.publish(x, b"1")).unwrap();
        block_on(p.publish(y, b"2")).unwrap(); // never learned → not resolvable
        block_on(p.publish(z, b"3")).unwrap(); // learned but outside the prefix
        let reports = p.advertiser().set.clone();
        let mut s = Sub::<4>::new(FakeScanner::new(reports))
            .with_filter(KeyFilter::Prefix(Key::from_static("app")))
            .unwrap();
        s.learn(x).unwrap();
        s.learn(z).unwrap();
        let mut buf = [0u8; 64];
        let got = block_on(s.recv(&mut buf)).unwrap();
        assert_eq!((got.key, got.payload), (x, &b"1"[..]));
        assert_eq!(got.meta.seq, Some(0));
        assert_eq!(block_on(s.recv(&mut buf)), Err(AdvertisingError::Radio(())));
    }

    #[test]
    fn try_recv_drains_without_waiting() {
        let reports = std::vec![
            mfr(TEST_COMPANY_ID, HUM, 0, b"no"),
            mfr(TEST_COMPANY_ID, TEMP, 0, b"t0"),
            mfr(TEST_COMPANY_ID, TEMP, 0, b"t0"),
        ];
        let mut s = exact::<1>(FakeScanner::new(reports), &[TEMP]);
        let mut buf = [0u8; 64];
        let got = s.try_recv(&mut buf).unwrap().unwrap();
        assert_eq!((got.key, got.payload), (TEMP, &b"t0"[..]));
        assert_eq!(s.try_recv(&mut buf), Ok(None), "repeat dropped, then empty");
        assert_eq!(s.try_recv(&mut buf), Ok(None));

        // Bounded: after MAX_IGNORED_REPORTS unrelated reports it yields.
        let mut junk: Vec<Vec<u8>> = (0..MAX_IGNORED_REPORTS)
            .map(|_| mfr(TEST_COMPANY_ID, HUM, 0, b""))
            .collect();
        junk.push(mfr(TEST_COMPANY_ID, TEMP, 9, b"late"));
        let mut s = exact::<1>(FakeScanner::new(junk), &[TEMP]);
        assert_eq!(s.try_recv(&mut buf), Ok(None));
        assert_eq!(s.try_recv(&mut buf).unwrap().unwrap().payload, b"late");
    }

    #[test]
    fn try_next_report_is_non_blocking() {
        let mut scanner = FakeScanner::new(std::vec![std::vec![2, AD_FLAGS, 6]]);
        let mut buf = [0u8; MAX_ADV_DATA];
        let r = scanner.try_next_report(&mut buf).unwrap().unwrap();
        assert_eq!((r.address, r.data), (ADDR, &[2, AD_FLAGS, 6][..]));
        assert_eq!(scanner.try_next_report(&mut buf), Ok(None));
    }

    #[test]
    fn end_to_end_publish_to_scanning_subscriber() {
        let mut p =
            AdvertisingPublisher::new(FakeAdvertiser::default(), AdvertisementBuilder::new());
        block_on(p.publish(HUM, b"55%")).unwrap();
        let reports = p.advertiser_mut().set.clone();
        let mut s = exact::<2>(FakeScanner::new(reports), &[TEMP, HUM]);
        let mut buf = [0u8; 64];
        let sample = block_on(s.recv(&mut buf)).unwrap();
        assert_eq!(sample.key, HUM);
        assert_eq!(sample.payload, b"55%");
    }

    #[test]
    fn discovery_recognises_middleware_nodes() {
        let ad = AdvertisementBuilder::new()
            .service_uuid(true)
            .name("node-7")
            .build()
            .unwrap();
        let report = ScanReport {
            address: ADDR,
            rssi: -61,
            data: ad.as_bytes(),
        };
        let node = discover(&report).unwrap();
        assert_eq!(node.address, ADDR);
        assert_eq!(node.rssi, -61);
        assert_eq!(
            node.name,
            Some(LocalName {
                name: "node-7",
                complete: true
            })
        );

        // Without the service UUID it is not a middleware node.
        let other = AdvertisementBuilder::new().name("node-7").build().unwrap();
        assert_eq!(
            discover(&ScanReport {
                address: ADDR,
                rssi: 0,
                data: other.as_bytes()
            }),
            None
        );
        // Malformed data is not a node.
        assert_eq!(
            discover(&ScanReport {
                address: ADDR,
                rssi: 0,
                data: &[9, 7]
            }),
            None
        );
        // A node without a name is still recognised.
        let bare = AdvertisementBuilder::new()
            .service_uuid(true)
            .build()
            .unwrap();
        let node = discover(&ScanReport {
            address: ADDR,
            rssi: 0,
            data: bare.as_bytes(),
        })
        .unwrap();
        assert_eq!(node.name, None);
    }

    /// A waker that counts its wakes.
    struct CountingWaker(std::sync::atomic::AtomicUsize);

    impl std::task::Wake for CountingWaker {
        fn wake(self: std::sync::Arc<Self>) {
            self.wake_by_ref();
        }
        fn wake_by_ref(self: &std::sync::Arc<Self>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn counting_waker() -> (std::sync::Arc<CountingWaker>, Waker) {
        let count = std::sync::Arc::new(CountingWaker(0.into()));
        (count.clone(), Waker::from(count))
    }

    fn wakes(count: &CountingWaker) -> usize {
        count.0.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn report(data: &[u8]) -> ScanReport<'_> {
        ScanReport {
            address: ADDR,
            rssi: -50,
            data,
        }
    }

    #[test]
    fn scan_queue_wakes_waiting_consumer() {
        let (count, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        let mut q = ScanQueue::<2>::new();
        let mut buf = [0u8; MAX_ADV_DATA];
        {
            let fut = core::pin::pin!(q.next_report(&mut buf));
            assert!(fut.poll(&mut cx).is_pending(), "empty queue waits");
        }
        assert_eq!(wakes(&count), 0);
        assert_eq!(
            q.push_report(&report(&[2, AD_FLAGS, 6])),
            Ok(Pushed::Queued)
        );
        assert_eq!(wakes(&count), 1, "a push wakes the registered waker");
        assert_eq!(q.push_report(&report(&[1, 9])), Ok(Pushed::Queued));
        assert_eq!(wakes(&count), 1, "the waker is taken by the first push");
        {
            let fut = core::pin::pin!(q.next_report(&mut buf));
            match fut.poll(&mut cx) {
                Poll::Ready(Ok(r)) => {
                    assert_eq!(
                        (r.address, r.rssi, r.data),
                        (ADDR, -50, &[2, AD_FLAGS, 6][..])
                    )
                }
                other => panic!("{other:?}"),
            }
        }
        let r = q.try_next_report(&mut buf).unwrap().unwrap();
        assert_eq!(r.data, &[1, 9]);
        assert_eq!(q.try_next_report(&mut buf), Ok(None), "try does not wait");
        assert!(q.is_empty());
    }

    #[test]
    fn scan_queue_is_bounded_and_counts_drops() {
        let mut q = ScanQueue::<2>::default();
        assert_eq!(q.capacity(), 2);
        for i in 0..3u8 {
            q.push_report(&report(&[1, i])).unwrap();
        }
        assert_eq!(q.push_report(&report(&[1, 3])), Ok(Pushed::DisplacedOldest));
        assert_eq!((q.len(), q.dropped()), (2, 2), "newest win");
        assert_eq!(
            q.push_report(&report(&[0; MAX_ADV_DATA + 1])),
            Err(ScanQueueError::TooLong)
        );
        assert_eq!((q.len(), q.dropped()), (2, 3));
        let mut small = [0u8; 1];
        assert_eq!(
            q.try_next_report(&mut small),
            Err(ScanQueueError::BufferTooSmall)
        );
        assert_eq!((q.len(), q.dropped()), (1, 4));
        let mut buf = [0u8; MAX_ADV_DATA];
        assert_eq!(q.try_next_report(&mut buf).unwrap().unwrap().data, &[1, 3]);
        // Wraps around the ring many times without losing order.
        for i in 0..10u8 {
            q.push_report(&report(&[1, i])).unwrap();
            assert_eq!(q.pop_report().unwrap().as_report().data, &[1, i]);
        }
        assert_eq!(q.pop_report(), None);
    }

    #[test]
    fn scan_queue_dropped_await_loses_no_report() {
        let (count, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        let ad = mfr(TEST_COMPANY_ID, TEMP, 0, b"t0");
        let mut s = AdvertisingSubscriber::<ScanQueue<4>, 1>::new(ScanQueue::new())
            .with_filter(KeyFilter::Exact(TEMP))
            .unwrap();
        let mut buf = [0u8; 64];
        {
            let fut = core::pin::pin!(s.recv(&mut buf));
            assert!(fut.poll(&mut cx).is_pending());
        } // the pending recv is dropped
        s.scanner_mut().push_report(&report(&ad)).unwrap();
        assert_eq!(wakes(&count), 1);
        // Dropped again after the wake, before it was polled.
        drop(s.recv(&mut buf));
        assert_eq!(s.scanner().len(), 1, "the report is still queued");
        let got = block_on(s.recv(&mut buf)).unwrap();
        assert_eq!((got.key, got.payload), (TEMP, &b"t0"[..]));
    }

    #[test]
    fn scan_queue_single_owner_pushes_in_place() {
        // As the badge does: one owner pushes from its scan callback, then
        // drains with try_recv.
        let mut s = AdvertisingSubscriber::<ScanQueue, 2>::default()
            .with_filter(KeyFilter::Exact(TEMP))
            .unwrap();
        let mut buf = [0u8; 64];
        for seq in 0..2 {
            let ad = mfr(TEST_COMPANY_ID, TEMP, seq, b"x");
            s.scanner_mut().push_report(&report(&ad)).unwrap();
            s.scanner_mut().push_report(&report(&ad)).unwrap(); // repeat
        }
        let mut seqs = Vec::new();
        while let Some(sample) = s.try_recv(&mut buf).unwrap() {
            seqs.push(sample.meta.seq);
        }
        assert_eq!(seqs, std::vec![Some(0), Some(1)]);
        assert!(s.scanner().is_empty());
    }

    /// A consumer that shares the queue through the caller's own lock (a
    /// `RefCell` on one executor), locking only inside each poll.
    struct Shared<'q>(&'q core::cell::RefCell<ScanQueue<4>>);

    impl Scanner for Shared<'_> {
        type Error = ScanQueueError;
        async fn next_report<'b>(
            &mut self,
            buf: &'b mut [u8],
        ) -> Result<ScanReport<'b>, ScanQueueError> {
            let r = core::future::poll_fn(|cx| self.0.borrow_mut().poll_report(cx)).await;
            r.copy_into(buf)
        }
        fn try_next_report<'b>(
            &mut self,
            buf: &'b mut [u8],
        ) -> Result<Option<ScanReport<'b>>, ScanQueueError> {
            let r = self.0.borrow_mut().pop_report();
            r.map(|r| r.copy_into(buf)).transpose()
        }
    }

    #[test]
    fn scan_queue_splits_by_caller_lock() {
        let (count, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        let queue = core::cell::RefCell::new(ScanQueue::<4>::new());
        let mut s = AdvertisingSubscriber::<_, 1>::new(Shared(&queue))
            .with_filter(KeyFilter::Exact(HUM))
            .unwrap();
        let mut buf = [0u8; 64];
        let mut fut = core::pin::pin!(s.recv(&mut buf));
        assert!(fut.as_mut().poll(&mut cx).is_pending());
        // The producer side, e.g. a radio callback, while recv is pending.
        let ad = mfr(TEST_COMPANY_ID, HUM, 4, b"55%");
        queue.borrow_mut().push_report(&report(&ad)).unwrap();
        assert_eq!(wakes(&count), 1);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(Ok(got)) => {
                assert_eq!(
                    (got.key, got.payload, got.meta.seq),
                    (HUM, &b"55%"[..], Some(4))
                )
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn key_and_filter_bounds_are_separate() {
        // One learned key, three filters.
        type S = AdvertisingSubscriber<FakeScanner, 1, MAX_KEY_LEN, DEFAULT_SENDERS, 3>;
        let mut s = S::new(FakeScanner::new(Vec::new()));
        for p in ["a", "b"] {
            s.subscribe_key(KeyFilter::Prefix(Key::new(p).unwrap()))
                .unwrap();
        }
        s.subscribe_key(KeyFilter::Exact(TEMP)).unwrap();
        assert_eq!(s.filters().count(), 3);
        assert_eq!(s.keys().capacity(), 1);
        assert_eq!(
            s.subscribe_key(KeyFilter::Prefix(Key::from_static("c"))),
            Err(ShortIdError::Full),
            "F bounds the filters"
        );
        s.unsubscribe_key(KeyFilter::Prefix(Key::from_static("a")));
        assert_eq!(
            s.subscribe_key(KeyFilter::Exact(HUM)),
            Err(ShortIdError::Full),
            "K bounds the learned keys"
        );
        assert_eq!(s.filters().count(), 2, "a failed subscribe changes nothing");
        // Many keys, one filter.
        type T = AdvertisingSubscriber<FakeScanner, 4, MAX_KEY_LEN, DEFAULT_SENDERS, 1>;
        let mut t = T::new(FakeScanner::new(Vec::new()))
            .with_filter(KeyFilter::Prefix(Key::from_static("sensor")))
            .unwrap();
        t.learn(TEMP).unwrap();
        t.learn(HUM).unwrap();
        assert_eq!(t.keys().len(), 2);
        assert_eq!(
            t.subscribe_key(KeyFilter::Exact(TEMP)),
            Err(ShortIdError::Full)
        );
    }

    #[test]
    fn filters_are_owned() {
        let mut s = Sub::<2>::new(FakeScanner::new(std::vec![mfr(
            TEST_COMPANY_ID,
            TEMP,
            0,
            b"t"
        ),]));
        {
            // The caller's key text lives only in this scope.
            let text = std::string::String::from("sensor/temperature");
            s.subscribe_key(KeyFilter::Exact(Key::new(&text).unwrap()))
                .unwrap();
        }
        let owned: KeyFilterBuf<16> = KeyFilter::Prefix(Key::from_static("sensor"))
            .to_buf()
            .unwrap();
        s.subscribe_key(owned).unwrap();
        assert_eq!(
            s.filters().collect::<Vec<_>>(),
            std::vec![
                KeyFilter::Exact(TEMP),
                KeyFilter::Prefix(Key::from_static("sensor"))
            ]
        );
        let mut buf = [0u8; 64];
        assert_eq!(block_on(s.recv(&mut buf)).unwrap().key, TEMP);
        s.unsubscribe_key(owned);
        assert_eq!(s.filters().count(), 1);
        // A key longer than the L-byte slots is refused.
        let mut short =
            AdvertisingSubscriber::<FakeScanner, 1, 4>::new(FakeScanner::new(Vec::new()));
        assert_eq!(
            short.subscribe_key(KeyFilter::Prefix(TEMP)),
            Err(ShortIdError::TooLong)
        );
    }

    #[test]
    fn subscribe_hands_out_filtered_view() {
        let reports = std::vec![
            mfr(TEST_COMPANY_ID, TEMP, 0, b"t0"),
            mfr(TEST_COMPANY_ID, HUM, 0, b"h0"),
            mfr(TEST_COMPANY_ID, HUM, 1, b"h1"),
        ];
        let mut s = exact::<2>(FakeScanner::new(reports), &[TEMP]);
        let mut buf = [0u8; 64];
        {
            let mut view = block_on(s.subscribe(KeyFilter::Exact(HUM))).unwrap();
            assert_eq!(view.filter(), KeyFilter::Exact(HUM));
            let got = block_on(view.recv(&mut buf)).unwrap();
            // The TEMP report was consumed and dropped by the HUM view.
            assert_eq!((got.key, got.payload), (HUM, &b"h0"[..]));
        }
        assert!(s.keys().contains(HUM), "the view's exact key stays learned");
        assert_eq!(
            s.filters().collect::<Vec<_>>(),
            std::vec![KeyFilter::Exact(TEMP)],
            "the view's filter is not added to the subscriber"
        );
        // The subscriber's own filters do not select HUM.
        assert_eq!(s.try_recv(&mut buf), Ok(None));
        let mut s = Sub::<1>::new(FakeScanner::new(Vec::new()));
        s.learn(TEMP).unwrap();
        assert!(matches!(
            block_on(s.subscribe(KeyFilter::Exact(HUM))),
            Err(ShortIdError::Full)
        ));
        let mut view = s
            .subscription(KeyFilter::Prefix(Key::from_static("sensor")))
            .unwrap();
        assert_eq!(view.try_recv(&mut buf), Ok(None));
    }

    #[derive(Default)]
    struct DefaultScanner;

    impl Scanner for DefaultScanner {
        type Error = ();
        async fn next_report<'b>(&mut self, _: &'b mut [u8]) -> Result<ScanReport<'b>, ()> {
            Err(())
        }
        fn try_next_report<'b>(&mut self, _: &'b mut [u8]) -> Result<Option<ScanReport<'b>>, ()> {
            Ok(None)
        }
    }

    #[test]
    fn defaults_and_const_builders() {
        const TEMPLATE: AdvertisementBuilder<'static> =
            AdvertisementBuilder::new().company_id(0x0059);
        const PUB: AdvertisingPublisher<'static, FakeAdvertiser2> =
            AdvertisingPublisher::new(FakeAdvertiser2, AdvertisementBuilder::new())
                .with_template(TEMPLATE);
        const SUB: AdvertisingSubscriber<ScanQueue<4>, 2> =
            AdvertisingSubscriber::new(ScanQueue::new()).with_company_id(0x0059);
        const TABLE: ShortIdTable<2> = ShortIdTable::new();
        const QUEUE: ScanQueue<3> = ScanQueue::new();
        assert_eq!(PUB.template().get_company_id(), 0x0059);
        assert_eq!(PUB.sequence().get(), 0);
        assert_eq!(SUB.company_id(), 0x0059);
        assert!(SUB.keys().is_empty() && SUB.scanner().is_empty());
        assert_eq!(TABLE, ShortIdTable::default());
        assert_eq!((QUEUE.capacity(), QUEUE.len(), QUEUE.dropped()), (3, 0, 0));

        let p = AdvertisingPublisher::<FakeAdvertiser>::default();
        assert_eq!(*p.template(), AdvertisementBuilder::new());
        assert_eq!(p.max_payload(), Some(MAX_ADVERT_PAYLOAD));
        let s = AdvertisingSubscriber::<DefaultScanner, 2>::default();
        assert_eq!(s.company_id(), TEST_COMPANY_ID);
        assert_eq!(s.filters().count(), 0);
        assert!(ScanQueue::<1>::default().is_empty());
    }

    /// A unit advertiser, constructible in `const` context.
    struct FakeAdvertiser2;

    impl Advertiser for FakeAdvertiser2 {
        type Error = ();
        async fn set_advertising_data(&mut self, _: &[u8]) -> Result<(), ()> {
            Ok(())
        }
    }
}
