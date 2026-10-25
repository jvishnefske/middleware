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
//! * [`Advertiser`] and [`Scanner`] are the radio traits.
//! * [`AdvertisingPublisher`] implements [`Publisher`]: each publish sets
//!   advertising data whose manufacturer data is
//!   `[short_id(key) u16 LE][sequence u8][payload]`. The sequence starts at 0
//!   and increments (wrapping) after every successful publish. A payload that
//!   does not fit beside the template's other fields is an error.
//! * [`AdvertisingSubscriber`] implements [`Subscriber`] for a fixed array of
//!   `K` keys: reports whose manufacturer data has the configured company id
//!   and a matching short id become samples, and a sequence number equal to
//!   the last one seen for that key is dropped as a repeat ([`DedupTable`]).
//! * [`discover`] recognises a scan report advertising [`SERVICE_UUID`] as a
//!   middleware [`Node`] and extracts its local name.
//!
//! Scan loops drop unrelated reports and give up after
//! [`MAX_IGNORED_REPORTS`] with [`AdvertisingError::ReportLimit`], so every
//! call is bounded.

use super::uuid::{SERVICE_UUID, short_id};
use crate::key::Key;
use crate::transport::{Publisher, Sample, Subscriber, sample_into};

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
            if s.data.len() % 16 != 0 {
                return Err(AdvertError::Malformed);
            }
            if s.data.chunks_exact(16).any(|u| u == wanted) {
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
pub trait Advertiser {
    /// Binding-specific failure.
    type Error: core::fmt::Debug;

    /// Replaces the advertising data (starting advertising if needed).
    async fn set_advertising_data(&mut self, data: &[u8]) -> Result<(), Self::Error>;
}

/// Radio side of scanning.
pub trait Scanner {
    /// Binding-specific failure.
    type Error: core::fmt::Debug;

    /// Waits for the next advertisement and copies its data into `buf`
    /// ([`MAX_ADV_DATA`] bytes are offered).
    async fn next_report<'b>(&mut self, buf: &'b mut [u8]) -> Result<ScanReport<'b>, Self::Error>;
}

/// Failure of an advertising publisher or subscriber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvertisingError<E> {
    /// The radio failed.
    Radio(E),
    /// The advertising data could not be built (payload too large).
    Advert(AdvertError),
    /// The caller's buffer cannot hold the sample.
    BufferTooSmall,
    /// [`MAX_IGNORED_REPORTS`] reports passed without a matching one.
    ReportLimit,
}

/// Publish sequence number, advanced by wrapping increment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sequence(u8);

impl Sequence {
    /// The raw value.
    pub const fn get(self) -> u8 {
        self.0
    }

    /// The following sequence number.
    pub const fn next(self) -> Self {
        Sequence(self.0.wrapping_add(1))
    }
}

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
            sequence: Sequence(0),
        }
    }

    /// The sequence number the next publish will carry.
    pub const fn sequence(&self) -> Sequence {
        self.sequence
    }

    /// Borrows the radio.
    pub fn advertiser_mut(&mut self) -> &mut A {
        &mut self.advertiser
    }
}

impl<A: Advertiser> Publisher for AdvertisingPublisher<'_, A> {
    type Error = AdvertisingError<A::Error>;

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

/// Last sequence number seen per subscribed key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DedupTable<const K: usize> {
    last: [Option<u8>; K],
}

impl<const K: usize> Default for DedupTable<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const K: usize> DedupTable<K> {
    /// A table that has seen nothing.
    pub const fn new() -> Self {
        DedupTable { last: [None; K] }
    }

    /// Records `sequence` for key slot `slot`. Returns the new table and
    /// whether the sample is fresh (differs from the last one for that slot).
    /// An out-of-range slot is never fresh.
    pub fn observe(self, slot: usize, sequence: u8) -> (Self, bool) {
        let mut last = self.last;
        match last.get_mut(slot) {
            Some(entry) if *entry == Some(sequence) => (self, false),
            Some(entry) => {
                *entry = Some(sequence);
                (DedupTable { last }, true)
            }
            None => (self, false),
        }
    }
}

/// Receives publications on a fixed set of `K` keys by scanning.
pub struct AdvertisingSubscriber<'a, S, const K: usize> {
    scanner: S,
    keys: [Key<'a>; K],
    company_id: u16,
    seen: DedupTable<K>,
}

impl<'a, S: Scanner, const K: usize> AdvertisingSubscriber<'a, S, K> {
    /// Subscribes to `keys`, accepting manufacturer data from [`TEST_COMPANY_ID`].
    pub const fn new(scanner: S, keys: [Key<'a>; K]) -> Self {
        AdvertisingSubscriber {
            scanner,
            keys,
            company_id: TEST_COMPANY_ID,
            seen: DedupTable::new(),
        }
    }

    /// Accepts manufacturer data from `company_id` instead.
    pub fn with_company_id(self, company_id: u16) -> Self {
        AdvertisingSubscriber { company_id, ..self }
    }

    /// Borrows the radio.
    pub fn scanner_mut(&mut self) -> &mut S {
        &mut self.scanner
    }
}

impl<S: Scanner, const K: usize> Subscriber for AdvertisingSubscriber<'_, S, K> {
    type Error = AdvertisingError<S::Error>;

    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, Self::Error> {
        let mut adv = [0u8; MAX_ADV_DATA];
        for _ in 0..MAX_IGNORED_REPORTS {
            let report = self
                .scanner
                .next_report(&mut adv)
                .await
                .map_err(AdvertisingError::Radio)?;
            let Ok(Some((company, data))) = manufacturer_data(report.data) else {
                continue;
            };
            let [lo, hi, sequence, payload @ ..] = data else {
                continue;
            };
            if company != self.company_id {
                continue;
            }
            let id = u16::from_le_bytes([*lo, *hi]);
            let Some(slot) = self.keys.iter().position(|k| short_id(*k) == id) else {
                continue;
            };
            let (seen, fresh) = self.seen.observe(slot, *sequence);
            self.seen = seen;
            if !fresh {
                continue;
            }
            let key = self.keys.get(slot).ok_or(AdvertisingError::ReportLimit)?;
            return sample_into(buf, key.as_str(), payload).ok_or(AdvertisingError::BufferTooSmall);
        }
        Err(AdvertisingError::ReportLimit)
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
            FakeScanner {
                reports: data.into_iter().map(|d| (ADDR, -40, d)).collect(),
            }
        }
    }

    impl Scanner for FakeScanner {
        type Error = ();
        async fn next_report<'b>(&mut self, buf: &'b mut [u8]) -> Result<ScanReport<'b>, ()> {
            let (address, rssi, data) = self.reports.pop_front().ok_or(())?;
            let out = buf.get_mut(..data.len()).ok_or(())?;
            out.copy_from_slice(&data);
            Ok(ScanReport {
                address,
                rssi,
                data: out,
            })
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
        let mut s = AdvertisingSubscriber::new(FakeScanner::new(reports), [TEMP, HUM]);
        let mut buf = [0u8; 64];
        let got: Vec<(Key<'static>, Vec<u8>)> = (0..3)
            .map(|_| {
                let x = block_on(s.recv(&mut buf)).unwrap();
                (if x.key == TEMP { TEMP } else { HUM }, x.payload.to_vec())
            })
            .collect();
        assert_eq!(
            got,
            std::vec![
                (TEMP, b"t0".to_vec()),
                (HUM, b"h0".to_vec()),
                (TEMP, b"t1".to_vec())
            ]
        );
        assert_eq!(block_on(s.recv(&mut buf)), Err(AdvertisingError::Radio(())));
    }

    #[test]
    fn scanning_subscriber_errors() {
        let reports = std::vec![mfr(0x0059, TEMP, 3, b"payload")];
        let mut s =
            AdvertisingSubscriber::new(FakeScanner::new(reports), [TEMP]).with_company_id(0x0059);
        let mut small = [0u8; 4];
        assert_eq!(
            block_on(s.recv(&mut small)),
            Err(AdvertisingError::BufferTooSmall)
        );

        let junk = (0..MAX_IGNORED_REPORTS)
            .map(|_| mfr(TEST_COMPANY_ID, HUM, 0, b""))
            .collect();
        let mut s = AdvertisingSubscriber::new(FakeScanner::new(junk), [TEMP]);
        let mut buf = [0u8; 64];
        assert_eq!(
            block_on(s.recv(&mut buf)),
            Err(AdvertisingError::ReportLimit)
        );
    }

    #[test]
    fn dedup_table_tracks_each_slot() {
        let t = DedupTable::<2>::new();
        let (t, fresh) = t.observe(0, 5);
        assert!(fresh);
        let (t, fresh) = t.observe(0, 5);
        assert!(!fresh);
        let (t, fresh) = t.observe(1, 5);
        assert!(fresh);
        let (_, fresh) = t.observe(2, 0);
        assert!(!fresh);
    }

    #[test]
    fn end_to_end_publish_to_scanning_subscriber() {
        let mut p =
            AdvertisingPublisher::new(FakeAdvertiser::default(), AdvertisementBuilder::new());
        block_on(p.publish(HUM, b"55%")).unwrap();
        let reports = p.advertiser_mut().set.clone();
        let mut s = AdvertisingSubscriber::new(FakeScanner::new(reports), [TEMP, HUM]);
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
