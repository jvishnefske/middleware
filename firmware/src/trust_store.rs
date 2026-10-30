//! The persisted root of trust: pinned key, `min_version` and the last
//! accepted control counter on flash.
//!
//! The `trust` partition (`partitions.csv`, 0x350000, two 4 KiB sectors)
//! holds one [`Record`] at the start of each sector. The current format
//! (2) is [`RECORD_LEN`] bytes:
//!
//! | offset | field                                      |
//! |--------|--------------------------------------------|
//! | 0      | magic `b"MWTR"`                            |
//! | 4      | format version (2)                         |
//! | 5      | state: 0 Unclaimed, 1 Claimed              |
//! | 6      | zero (2 bytes)                             |
//! | 8      | sequence number u32 LE                     |
//! | 12     | `min_version` u32 LE                       |
//! | 16     | public key (32 bytes, zero when Unclaimed) |
//! | 48     | last accepted control counter u64 LE ([`ControlCounter`], zero when Unclaimed) |
//! | 56     | CRC-32 of bytes 0..56, u32 LE              |
//!
//! # Migration from format 1
//!
//! Format 1 (firmware before signed control requests) is the same up to
//! offset 48, where its CRC-32 of bytes 0..48 ends a 52-byte record; it has
//! no control counter. [`Record::decode`] reads both: a format-1 record
//! loads with control counter 0. Nothing is rewritten at boot; the next
//! save (a claim, a signed commit or an accepted control request) writes a
//! format-2 record into the other sector with the next sequence number, so
//! the migration has the same power-loss safety as any save: until it
//! completes, the format-1 record is the newest valid one. A format-2 record
//! is never written back as format 1.
//!
//! # Power-loss safety
//!
//! [`TrustStore::save`] never touches the sector holding the current
//! record: it erases the *other* sector, writes the new record there with
//! the next sequence number (fields first, magic last, as the payload
//! header is written), and only then treats it as current. A cut during
//! the erase or either write leaves at most a half record, which fails its
//! magic or CRC, so [`TrustStore::open`] still finds the previous record
//! in the untouched sector. The newest valid record wins (higher sequence
//! number; sequence numbers only grow by one per save, so they do not wrap
//! within the flash's erase endurance). With no valid record the device is
//! Unclaimed.
//!
//! # Ordering with activation and control
//!
//! A signed commit raises `min_version`. The shell saves the new record
//! inside [`crate::store::PayloadStore::activate_with`]: after the new
//! image's CRC check and before its slot header is written. A power cut
//! between the two leaves the old image active with the raised
//! `min_version` — the commit's version is used up and must be re-signed
//! with a higher one, but no older image can be replayed. The opposite
//! order would let the new image boot with the old `min_version`. In the
//! same way an accepted signed control request's counter is saved before
//! the payload is started or stopped, so a captured request can never act
//! twice, even across a power cut.
//!
//! A save never lowers `min_version` or the control counter for the same
//! pinned key ([`lowers`]); a claim of a new key starts both over at 0.

use crate::image::{SECTOR, WORD};
use crate::store::StoreError;
use embedded_storage::nor_flash::NorFlash;
use middleware::crc::Crc32;
use middleware::loader::trust::{ControlCounter, KEY_LEN, PublicKey, TrustState, Version};

/// Bytes of one current-format record on flash.
pub const RECORD_LEN: usize = 60;
/// Bytes of a format-1 record.
pub const RECORD_V1_LEN: usize = 52;
/// Smallest trust partition: two sectors.
pub const PARTITION_LEN: u32 = 2 * SECTOR;
const MAGIC: [u8; 4] = *b"MWTR";
/// The format [`Record::encode`] writes.
pub const FORMAT: u8 = 2;
const FORMAT_V1: u8 = 1;
const CRC_AT: usize = 56;
const CRC_V1_AT: usize = 48;

/// Everything the trust partition persists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Persisted {
    /// The root of trust.
    pub trust: TrustState,
    /// The last signed control request's counter (0 when none).
    pub control: ControlCounter,
}

/// A record: the persisted state and its sequence number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    /// Save counter; the newest valid record wins.
    pub seq: u32,
    /// The persisted state.
    pub state: Persisted,
}

fn word(b: &[u8], at: usize) -> Option<u32> {
    let w = b.get(at..at + 4)?;
    Some(u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
}

impl Record {
    /// The record's bytes, in the current format.
    pub fn encode(&self) -> [u8; RECORD_LEN] {
        let mut b = [0u8; RECORD_LEN];
        let (flag, min, key) = match self.state.trust {
            TrustState::Unclaimed => (0u8, 0u32, [0u8; KEY_LEN]),
            TrustState::Claimed { key, min_version } => (1, min_version.0, key.0),
        };
        let control = if flag == 0 { 0 } else { self.state.control.0 };
        b[..4].copy_from_slice(&MAGIC);
        b[4] = FORMAT;
        b[5] = flag;
        b[8..12].copy_from_slice(&self.seq.to_le_bytes());
        b[12..16].copy_from_slice(&min.to_le_bytes());
        b[16..48].copy_from_slice(&key);
        b[48..56].copy_from_slice(&control.to_le_bytes());
        let crc = Crc32::checksum(&b[..CRC_AT]);
        b[CRC_AT..].copy_from_slice(&crc.to_le_bytes());
        b
    }

    /// Decodes a record of format 2 or 1 (`b` holds [`RECORD_LEN`] bytes
    /// read from the sector start; a format-1 record uses the first
    /// [`RECORD_V1_LEN`] and loads with control counter 0). `None` unless
    /// magic, format, CRC and fields are all valid.
    pub fn decode(b: &[u8; RECORD_LEN]) -> Option<Self> {
        if b[..4] != MAGIC || b[6] != 0 || b[7] != 0 {
            return None;
        }
        let (crc_at, control) = match b[4] {
            FORMAT => {
                let mut c = [0u8; 8];
                c.copy_from_slice(&b[48..56]);
                (CRC_AT, u64::from_le_bytes(c))
            }
            FORMAT_V1 => (CRC_V1_AT, 0),
            _ => return None,
        };
        if Crc32::checksum(b.get(..crc_at)?) != word(b, crc_at)? {
            return None;
        }
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(&b[16..48]);
        let min = word(b, 12)?;
        let trust = match b[5] {
            0 if min == 0 && key == [0; KEY_LEN] && control == 0 => TrustState::Unclaimed,
            1 => TrustState::Claimed { key: PublicKey(key), min_version: Version(min) },
            _ => return None,
        };
        Some(Record { seq: word(b, 8)?, state: Persisted { trust, control: ControlCounter(control) } })
    }
}

/// The trust partition: which sector holds the current record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustStore {
    base: u32,
    /// Sector index (0 or 1) and sequence of the current record.
    current: Option<(u32, u32)>,
    /// The state the current record holds (what a reboot would load).
    persisted: Persisted,
}

impl TrustStore {
    /// Reads the partition at absolute offset `base` (sector aligned, at
    /// least [`PARTITION_LEN`] bytes) and returns the store and the newest
    /// valid state (Unclaimed when there is none). Unreadable sectors count
    /// as invalid.
    pub fn open<F: NorFlash>(flash: &mut F, base: u32, size: u32) -> Result<(Self, Persisted), StoreError> {
        if !base.is_multiple_of(SECTOR) {
            return Err(StoreError::Flash);
        }
        if size < PARTITION_LEN {
            return Err(StoreError::TooLarge);
        }
        let mut newest: Option<(u32, Record)> = None;
        for sector in 0..2u32 {
            let mut buf = [0u8; RECORD_LEN];
            if flash.read(base + sector * SECTOR, &mut buf).is_err() {
                continue;
            }
            if let Some(r) = Record::decode(&buf)
                && newest.is_none_or(|(_, n)| r.seq > n.seq)
            {
                newest = Some((sector, r));
            }
        }
        let persisted = newest.map_or(Persisted::default(), |(_, r)| r.state);
        let store = TrustStore { base, current: newest.map(|(s, r)| (s, r.seq)), persisted };
        Ok((store, persisted))
    }

    /// Persists `state` in the sector that does not hold the current
    /// record (see the module docs), always in the current format. On
    /// error the previous record is still the one [`TrustStore::open`]
    /// finds. A state that would lower `min_version` or the control
    /// counter for the same key is refused with [`StoreError::Rollback`].
    pub fn save<F: NorFlash>(&mut self, flash: &mut F, state: &Persisted) -> Result<(), StoreError> {
        if lowers(&self.persisted, state) {
            return Err(StoreError::Rollback);
        }
        let (sector, seq) = match self.current {
            Some((s, seq)) => (1 - s, seq.wrapping_add(1)),
            None => (0, 1),
        };
        let at = self.base + sector * SECTOR;
        let bytes = Record { seq, state: *state }.encode();
        let (magic, fields) = bytes.split_at(WORD as usize);
        flash.erase(at, at + SECTOR).map_err(|_| StoreError::Flash)?;
        flash.write(at + WORD, fields).map_err(|_| StoreError::Flash)?;
        flash.write(at, magic).map_err(|_| StoreError::Flash)?;
        self.current = Some((sector, seq));
        self.persisted = *state;
        Ok(())
    }

    /// What is on flash: what the device would load after a reboot. The
    /// shell adopts it when a later step of an activation fails, so RAM
    /// never trusts an older `min_version` (or control counter) than flash
    /// does.
    pub const fn persisted(&self) -> Persisted {
        self.persisted
    }
}

/// True if saving `next` over `current` would lower `min_version` or the
/// control counter for the same pinned key, which would reopen versions or
/// control requests already used (rollback, replay), or forget the key. A
/// new key (re-key) starts over at 0 and is allowed.
pub fn lowers(current: &Persisted, next: &Persisted) -> bool {
    match (current.trust, next.trust) {
        (
            TrustState::Claimed { key: a, min_version: old },
            TrustState::Claimed { key: b, min_version: new },
        ) => a == b && (new.0 < old.0 || next.control < current.control),
        (TrustState::Claimed { .. }, TrustState::Unclaimed) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u32 = SECTOR;
    type Flash = crate::ram_flash::RamFlash<{ (SECTOR + PARTITION_LEN) as usize }>;

    fn claimed(k: u8, min: u32) -> Persisted {
        with_control(k, min, 0)
    }

    fn with_control(k: u8, min: u32, control: u64) -> Persisted {
        Persisted {
            trust: TrustState::Claimed { key: PublicKey([k; 32]), min_version: Version(min) },
            control: ControlCounter(control),
        }
    }

    const UNCLAIMED: Persisted = Persisted { trust: TrustState::Unclaimed, control: ControlCounter(0) };

    fn open(flash: &mut Flash) -> (TrustStore, Persisted) {
        TrustStore::open(flash, BASE, PARTITION_LEN).unwrap()
    }

    #[test]
    fn saves_never_lower_min_version_for_the_same_key() {
        let mut flash = Flash::new();
        let (mut ts, _) = open(&mut flash);
        ts.save(&mut flash, &claimed(1, 100)).unwrap();
        assert_eq!(ts.save(&mut flash, &claimed(1, 99)), Err(StoreError::Rollback));
        assert_eq!(ts.save(&mut flash, &UNCLAIMED), Err(StoreError::Rollback));
        assert_eq!(ts.persisted(), claimed(1, 100));
        // Same key upwards, and a re-key (new key at 0), are allowed.
        ts.save(&mut flash, &claimed(1, 101)).unwrap();
        ts.save(&mut flash, &claimed(2, 0)).unwrap();
        assert_eq!(ts.persisted(), claimed(2, 0));
        let (reopened, state) = open(&mut flash);
        assert_eq!((reopened.persisted(), state), (claimed(2, 0), claimed(2, 0)));
    }

    #[test]
    fn records_round_trip_and_reject_damage() {
        for state in [UNCLAIMED, claimed(3, 1_761_500_000), with_control(3, 7, 1_761_500_000_123)] {
            let r = Record { seq: 9, state };
            let b = r.encode();
            assert_eq!(Record::decode(&b), Some(r));
            for i in 0..RECORD_LEN {
                let mut bad = b;
                bad[i] ^= 0x01;
                assert_eq!(Record::decode(&bad), None, "byte {i}");
            }
        }
        assert_eq!(Record::decode(&[0xFF; RECORD_LEN]), None, "erased");
        assert_eq!(Record::decode(&[0; RECORD_LEN]), None, "zeroed");
    }

    #[test]
    fn blank_or_foreign_partition_is_unclaimed() {
        let mut flash = Flash::new();
        assert_eq!(open(&mut flash).1, UNCLAIMED);
        flash.mem.fill(0xFF);
        assert_eq!(open(&mut flash).1, UNCLAIMED);
        assert!(TrustStore::open(&mut flash, BASE + 4, PARTITION_LEN).is_err());
        assert_eq!(TrustStore::open(&mut flash, BASE, SECTOR), Err(StoreError::TooLarge));
    }

    #[test]
    fn saves_alternate_sectors_and_survive_reopen() {
        let mut flash = Flash::new();
        let (mut store, _) = open(&mut flash);
        for n in 1..=5u32 {
            let state = claimed(n as u8, n * 10);
            store.save(&mut flash, &state).unwrap();
            let (reopened, got) = open(&mut flash);
            assert_eq!(got, state, "save {n}");
            assert_eq!(reopened, store);
            assert_eq!(store.current, Some(((n - 1) % 2, n)));
        }
        assert!(flash.mem[..BASE as usize].iter().all(|&b| b == 0), "outside partition untouched");
    }

    /// Power lost at every flash operation of a save: afterwards the store
    /// reads either the previous state or the new one, never Unclaimed
    /// once something was claimed, and a later save still works.
    #[test]
    fn power_loss_at_any_operation_keeps_old_or_new_record() {
        for prior in 0..3u32 {
            let old = (prior > 0).then(|| claimed(prior as u8, prior * 100));
            let new = with_control(0xEE, 4242, 99);
            for cut in 0..3 {
                let mut flash = Flash::new();
                let (mut store, _) = open(&mut flash);
                for p in 1..=prior {
                    store.save(&mut flash, &claimed(p as u8, p * 100)).unwrap();
                }
                flash.cut_at(cut);
                assert!(store.save(&mut flash, &new).is_err(), "prior {prior} cut {cut}");
                flash.restore();
                let (mut store, got) = open(&mut flash);
                match old {
                    Some(o) => assert_eq!(got, o, "prior {prior} cut {cut}"),
                    None => assert_eq!(got, UNCLAIMED),
                }
                store.save(&mut flash, &new).unwrap();
                assert_eq!(open(&mut flash).1, new);
            }
            // Cut after the last write: nothing torn, the save completed.
            let mut flash = Flash::new();
            let (mut store, _) = open(&mut flash);
            flash.cut_at(3);
            store.save(&mut flash, &new).unwrap();
            assert_eq!(open(&mut flash).1, new);
        }
    }

    #[test]
    fn newest_valid_record_wins_and_a_corrupt_one_falls_back() {
        let mut flash = Flash::new();
        let (mut store, _) = open(&mut flash);
        store.save(&mut flash, &claimed(1, 10)).unwrap();
        store.save(&mut flash, &claimed(2, 20)).unwrap();
        assert_eq!(open(&mut flash).1, claimed(2, 20));
        // Damage the newest (sector 1): the older record is used.
        flash.mem[(BASE + SECTOR + 20) as usize] ^= 0xFF;
        let (mut store, got) = open(&mut flash);
        assert_eq!(got, claimed(1, 10));
        // The next save overwrites the damaged sector, not the good one.
        store.save(&mut flash, &claimed(3, 30)).unwrap();
        assert_eq!(open(&mut flash).1, claimed(3, 30));
        assert_eq!(store.current, Some((1, 2)));
    }

    #[test]
    fn saves_never_lower_the_control_counter_for_the_same_key() {
        let mut flash = Flash::new();
        let (mut ts, _) = open(&mut flash);
        ts.save(&mut flash, &with_control(1, 5, 100)).unwrap();
        assert_eq!(ts.save(&mut flash, &with_control(1, 5, 99)), Err(StoreError::Rollback));
        assert_eq!(ts.save(&mut flash, &with_control(1, 6, 0)), Err(StoreError::Rollback), "a commit keeps the counter");
        ts.save(&mut flash, &with_control(1, 5, 100)).unwrap();
        ts.save(&mut flash, &with_control(1, 6, 101)).unwrap();
        // A re-key starts the counter over.
        ts.save(&mut flash, &with_control(2, 0, 0)).unwrap();
        assert_eq!(open(&mut flash).1, with_control(2, 0, 0));
    }

    /// A format-1 record exactly as the previous firmware wrote it.
    fn v1_record(seq: u32, state: &TrustState) -> [u8; RECORD_V1_LEN] {
        let mut b = [0u8; RECORD_V1_LEN];
        let (flag, min, key) = match *state {
            TrustState::Unclaimed => (0u8, 0u32, [0u8; KEY_LEN]),
            TrustState::Claimed { key, min_version } => (1, min_version.0, key.0),
        };
        b[..4].copy_from_slice(b"MWTR");
        b[4] = 1;
        b[5] = flag;
        b[8..12].copy_from_slice(&seq.to_le_bytes());
        b[12..16].copy_from_slice(&min.to_le_bytes());
        b[16..48].copy_from_slice(&key);
        let crc = Crc32::checksum(&b[..48]);
        b[48..].copy_from_slice(&crc.to_le_bytes());
        b
    }

    /// Writes `bytes` at the start of `sector` as the old firmware did.
    fn put(flash: &mut Flash, sector: u32, bytes: &[u8]) {
        let at = (BASE + sector * SECTOR) as usize;
        flash.mem[at..at + SECTOR as usize].fill(0xFF);
        flash.mem[at..at + bytes.len()].copy_from_slice(bytes);
    }

    #[test]
    fn format_1_records_load_with_counter_zero_and_migrate_on_the_next_save() {
        let old = TrustState::Claimed { key: PublicKey([0xAB; 32]), min_version: Version(3000) };
        // The board's history: sector 0 seq 4, sector 1 seq 5 (newest).
        let mut flash = Flash::new();
        put(&mut flash, 0, &v1_record(4, &TrustState::Claimed { key: PublicKey([0xAB; 32]), min_version: Version(1000) }));
        put(&mut flash, 1, &v1_record(5, &old));
        let (mut ts, got) = open(&mut flash);
        assert_eq!(got, Persisted { trust: old, control: ControlCounter(0) });
        assert_eq!(ts.current, Some((1, 5)));
        // A damaged format-1 record is refused like any other.
        for i in 0..RECORD_V1_LEN {
            let mut bad = [0xFFu8; RECORD_LEN];
            bad[..RECORD_V1_LEN].copy_from_slice(&v1_record(5, &old));
            bad[i] ^= 0x01;
            assert_eq!(Record::decode(&bad), None, "byte {i}");
        }
        // The first save (a control counter) writes format 2 into sector 0
        // with seq 6; the format-1 record in sector 1 stays untouched.
        let next = Persisted { trust: old, control: ControlCounter(1_761_500_000_123) };
        ts.save(&mut flash, &next).unwrap();
        let s0 = (BASE) as usize;
        assert_eq!(flash.mem[s0 + 4], FORMAT);
        let s1 = (BASE + SECTOR) as usize;
        assert_eq!(&flash.mem[s1..s1 + RECORD_V1_LEN], &v1_record(5, &old));
        let (reopened, got) = open(&mut flash);
        assert_eq!((got, reopened.current), (next, Some((0, 6))));
        // A migration cut short at any flash operation keeps the format-1
        // state, and a later save still migrates.
        for cut in 0..3 {
            let mut flash = Flash::new();
            put(&mut flash, 1, &v1_record(5, &old));
            let (mut ts, _) = open(&mut flash);
            flash.cut_at(cut);
            assert!(ts.save(&mut flash, &next).is_err(), "cut {cut}");
            flash.restore();
            let (mut ts, got) = open(&mut flash);
            assert_eq!(got, Persisted { trust: old, control: ControlCounter(0) }, "cut {cut}");
            ts.save(&mut flash, &next).unwrap();
            assert_eq!(open(&mut flash).1, next);
        }
        // An unclaimed format-1 record loads as Unclaimed.
        let mut flash = Flash::new();
        put(&mut flash, 0, &v1_record(1, &TrustState::Unclaimed));
        assert_eq!(open(&mut flash).1, UNCLAIMED);
    }

    #[test]
    fn unknown_formats_and_an_unclaimed_record_with_a_counter_are_refused() {
        let mut b = Record { seq: 1, state: with_control(1, 1, 1) }.encode();
        b[4] = 3;
        let crc = Crc32::checksum(&b[..CRC_AT]);
        b[CRC_AT..].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(Record::decode(&b), None, "format 3");
        let mut u = Record { seq: 1, state: UNCLAIMED }.encode();
        u[48] = 1;
        let crc = Crc32::checksum(&u[..CRC_AT]);
        u[CRC_AT..].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(Record::decode(&u), None);
        // Encoding an unclaimed state never stores a counter.
        let e = Record { seq: 2, state: Persisted { trust: TrustState::Unclaimed, control: ControlCounter(5) } }.encode();
        assert_eq!(Record::decode(&e).map(|r| r.state), Some(UNCLAIMED));
    }
}
