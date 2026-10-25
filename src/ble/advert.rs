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
//! * An advert carries only the 16-bit short id of its key, never the key
//!   text. [`ShortIdTable`] holds up to `K` learned key texts and resolves a
//!   short id back to its key; learning a key whose short id is already taken
//!   by a different key fails with [`ShortIdError::Collision`], so ambiguous
//!   ids are noticed instead of silently delivering the wrong key.
//! * [`AdvertisingSubscriber`] implements [`Subscriber`] for a set of
//!   [`KeyFilter`]s that can grow at run time
//!   ([`AdvertisingSubscriber::subscribe_key`],
//!   [`AdvertisingSubscriber::learn`]). A report becomes a sample when its
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
//! [`Scanner::next_report`] is: all subscriber state changes happen
//! synchronously after a report arrives.

use super::uuid::{SERVICE_UUID, short_id};
use crate::dedup::SeenTable;
pub use crate::dedup::Sequence;
use crate::key::{Key, KeyFilter, MAX_KEY_LEN};
use crate::transport::{Limits, Publisher, Sample, SampleMeta, SenderId, Subscriber, sample_into};

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
    /// there and pops one here.
    fn try_next_report<'b>(
        &mut self,
        buf: &'b mut [u8],
    ) -> Result<Option<ScanReport<'b>>, Self::Error>;
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

/// A valid key's text held by value in an `L`-byte slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KeyText<const L: usize> {
    buf: [u8; L],
    len: usize,
}

impl<const L: usize> KeyText<L> {
    fn new(key: Key<'_>) -> Result<Self, ShortIdError> {
        let bytes = key.as_bytes();
        let mut buf = [0u8; L];
        buf.get_mut(..bytes.len())
            .ok_or(ShortIdError::TooLong)?
            .copy_from_slice(bytes);
        Ok(KeyText {
            buf,
            len: bytes.len(),
        })
    }

    fn as_bytes(&self) -> &[u8] {
        self.buf.get(..self.len).unwrap_or(&[])
    }

    /// The key. Always `Some`: only valid keys are stored.
    fn key(&self) -> Option<Key<'_>> {
        let text = core::str::from_utf8(self.as_bytes()).ok()?;
        Key::new(text).ok()
    }

    fn is(&self, key: Key<'_>) -> bool {
        self.as_bytes() == key.as_bytes()
    }
}

/// Why a key or filter was not added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortIdError {
    /// The table (or the subscriber's filter list) already holds `K` entries.
    Full,
    /// The key is longer than the `L`-byte text slots.
    TooLong,
    /// A different learned key already has this short id, so adverts could
    /// not tell the two apart.
    Collision(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Learned<const L: usize> {
    id: u16,
    text: KeyText<L>,
}

/// Learned key texts, resolving advert short ids back to keys.
///
/// Holds up to `K` keys of at most `L` bytes each (default
/// [`MAX_KEY_LEN`], so any key fits). No two learned keys share a short id.
/// Transitions consume the table and return the next one.
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
            Some(e) if e.text.is(key) => return Ok(self),
            Some(_) => return Err(ShortIdError::Collision(id)),
            None => {}
        }
        let text = KeyText::new(key)?;
        let mut entries = self.entries;
        let slot = entries
            .iter_mut()
            .find(|e| e.is_none())
            .ok_or(ShortIdError::Full)?;
        *slot = Some(Learned { id, text });
        Ok(ShortIdTable { entries })
    }

    /// Removes `key` if it is held.
    pub fn forget(self, key: Key<'_>) -> Self {
        let mut entries = self.entries;
        for e in entries.iter_mut() {
            if matches!(e, Some(l) if l.text.is(key)) {
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
            .and_then(|e| e.text.key())
    }

    /// True when `key` was learned.
    pub fn contains(&self, key: Key<'_>) -> bool {
        self.entries.iter().flatten().any(|e| e.text.is(key))
    }

    /// The learned keys, in slot order.
    pub fn keys(&self) -> impl Iterator<Item = Key<'_>> {
        self.entries.iter().flatten().filter_map(|e| e.text.key())
    }

    /// Number of learned keys.
    pub fn len(&self) -> usize {
        self.entries.iter().flatten().count()
    }

    /// True when no key was learned.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn resolve_text(&self, id: u16) -> Option<KeyText<L>> {
        self.entries
            .iter()
            .flatten()
            .find(|e| e.id == id)
            .map(|e| e.text)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterKind {
    Exact,
    Prefix,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StoredFilter<const L: usize> {
    kind: FilterKind,
    text: KeyText<L>,
}

impl<const L: usize> StoredFilter<L> {
    fn new(filter: KeyFilter<'_>) -> Result<Self, ShortIdError> {
        let kind = match filter {
            KeyFilter::Exact(_) => FilterKind::Exact,
            KeyFilter::Prefix(_) => FilterKind::Prefix,
        };
        Ok(StoredFilter {
            kind,
            text: KeyText::new(filter.key())?,
        })
    }

    fn matches(&self, key: Key<'_>) -> bool {
        self.text.key().is_some_and(|k| match self.kind {
            FilterKind::Exact => KeyFilter::Exact(k).matches(key),
            FilterKind::Prefix => KeyFilter::Prefix(k).matches(key),
        })
    }
}

/// Default number of (sender, key) pairs an [`AdvertisingSubscriber`]
/// remembers for duplicate suppression.
pub const DEFAULT_SENDERS: usize = 16;

/// An accepted report, before it is copied into the caller's buffer.
struct Hit<'r, const L: usize> {
    key: KeyText<L>,
    meta: SampleMeta,
    payload: &'r [u8],
}

impl<const L: usize> Hit<'_, L> {
    fn deliver<'b, E>(self, buf: &'b mut [u8]) -> Result<Sample<'b>, AdvertisingError<E>> {
        let key = self.key.key().ok_or(AdvertisingError::BufferTooSmall)?;
        sample_into(buf, key.as_str(), self.payload)
            .map(|s| s.with_meta(self.meta))
            .ok_or(AdvertisingError::BufferTooSmall)
    }
}

/// Receives publications by scanning (see the module docs).
///
/// * `K` bounds both the learned keys and the filters;
/// * `L` is the longest key text held (default [`MAX_KEY_LEN`]);
/// * `D` is the number of (sender, key) pairs remembered for duplicate
///   suppression (default [`DEFAULT_SENDERS`]).
pub struct AdvertisingSubscriber<
    S,
    const K: usize,
    const L: usize = MAX_KEY_LEN,
    const D: usize = DEFAULT_SENDERS,
> {
    scanner: S,
    keys: ShortIdTable<K, L>,
    filters: [Option<StoredFilter<L>>; K],
    company_id: u16,
    seen: SeenTable<D>,
}

impl<S: Scanner, const K: usize, const L: usize, const D: usize> AdvertisingSubscriber<S, K, L, D> {
    /// A subscriber with no filters (it delivers nothing until one is added),
    /// accepting manufacturer data from [`TEST_COMPANY_ID`].
    pub const fn new(scanner: S) -> Self {
        AdvertisingSubscriber {
            scanner,
            keys: ShortIdTable::new(),
            filters: [None; K],
            company_id: TEST_COMPANY_ID,
            seen: SeenTable::new(),
        }
    }

    /// Accepts manufacturer data from `company_id` instead.
    pub fn with_company_id(self, company_id: u16) -> Self {
        AdvertisingSubscriber { company_id, ..self }
    }

    /// Builder form of [`AdvertisingSubscriber::subscribe_key`].
    ///
    /// # Errors
    ///
    /// As [`AdvertisingSubscriber::subscribe_key`].
    pub fn with_filter(mut self, filter: KeyFilter<'_>) -> Result<Self, ShortIdError> {
        self.subscribe_key(filter)?;
        Ok(self)
    }

    /// Starts delivering keys matched by `filter`. An exact filter also
    /// learns its key. A prefix filter matches only learned keys
    /// ([`AdvertisingSubscriber::learn`]). Adding a filter that is already
    /// present changes nothing. On error the subscriber is unchanged.
    ///
    /// # Errors
    ///
    /// [`ShortIdError::Full`] when `K` filters (or, for an exact filter, `K`
    /// keys) are held, [`ShortIdError::TooLong`] for a key longer than `L`,
    /// [`ShortIdError::Collision`] when an exact key's short id is taken.
    pub fn subscribe_key(&mut self, filter: KeyFilter<'_>) -> Result<(), ShortIdError> {
        let stored = StoredFilter::new(filter)?;
        let keys = match filter {
            KeyFilter::Exact(key) => self.keys.clone().learn(key)?,
            KeyFilter::Prefix(_) => self.keys.clone(),
        };
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
    pub fn unsubscribe_key(&mut self, filter: KeyFilter<'_>) {
        let Ok(stored) = StoredFilter::<L>::new(filter) else {
            return;
        };
        for f in self.filters.iter_mut() {
            if *f == Some(stored) {
                *f = None;
            }
        }
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

    /// Borrows the radio.
    pub fn scanner_mut(&mut self) -> &mut S {
        &mut self.scanner
    }

    /// Decides whether `report` is a fresh sample on a subscribed key,
    /// recording its sequence number if it is.
    fn admit<'r>(&mut self, report: &ScanReport<'r>) -> Option<Hit<'r, L>> {
        let Ok(Some((company, [lo, hi, seq, payload @ ..]))) = manufacturer_data(report.data)
        else {
            return None;
        };
        if company != self.company_id {
            return None;
        }
        let text = self.keys.resolve_text(u16::from_le_bytes([*lo, *hi]))?;
        let key = text.key()?;
        if !self.filters.iter().flatten().any(|f| f.matches(key)) {
            return None;
        }
        let sender = report.address.sender();
        let (seen, fresh) = self.seen.observe(sender, key, *seq);
        self.seen = seen;
        fresh.then_some(Hit {
            key: text,
            meta: SampleMeta {
                sender: Some(sender),
                seq: Some(*seq),
            },
            payload,
        })
    }
}

impl<S: Scanner, const K: usize, const L: usize, const D: usize> Subscriber
    for AdvertisingSubscriber<S, K, L, D>
{
    type Error = AdvertisingError<S::Error>;

    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, Self::Error> {
        let mut adv = [0u8; MAX_ADV_DATA];
        for _ in 0..MAX_IGNORED_REPORTS {
            let report = self
                .scanner
                .next_report(&mut adv)
                .await
                .map_err(AdvertisingError::Radio)?;
            if let Some(hit) = self.admit(&report) {
                return hit.deliver(buf);
            }
        }
        Err(AdvertisingError::ReportLimit)
    }

    /// Drains already-received reports until one is a fresh sample. Returns
    /// `Ok(None)` when the scanner has no report, or after
    /// [`MAX_IGNORED_REPORTS`] unrelated ones (the rest are examined on the
    /// next call).
    fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, Self::Error> {
        let mut adv = [0u8; MAX_ADV_DATA];
        for _ in 0..MAX_IGNORED_REPORTS {
            let Some(report) = self
                .scanner
                .try_next_report(&mut adv)
                .map_err(AdvertisingError::Radio)?
            else {
                return Ok(None);
            };
            if let Some(hit) = self.admit(&report) {
                return hit.deliver(buf).map(Some);
            }
        }
        Ok(None)
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
}
