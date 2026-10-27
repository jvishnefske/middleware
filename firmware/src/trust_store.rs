//! The persisted root of trust: pinned key and `min_version` on flash.
//!
//! The `trust` partition (`partitions.csv`, 0x350000, two 4 KiB sectors)
//! holds one [`Record`] at the start of each sector. A record is
//! [`RECORD_LEN`] bytes:
//!
//! | offset | field                                      |
//! |--------|--------------------------------------------|
//! | 0      | magic `b"MWTR"`                            |
//! | 4      | format version (1)                         |
//! | 5      | state: 0 Unclaimed, 1 Claimed              |
//! | 6      | zero (2 bytes)                             |
//! | 8      | sequence number u32 LE                     |
//! | 12     | `min_version` u32 LE                       |
//! | 16     | public key (32 bytes, zero when Unclaimed) |
//! | 48     | CRC-32 of bytes 0..48, u32 LE              |
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
//! # Ordering with activation
//!
//! A signed commit raises `min_version`. The shell saves the new record
//! inside [`crate::store::PayloadStore::activate_with`]: after the new
//! image's CRC check and before its slot header is written. A power cut
//! between the two leaves the old image active with the raised
//! `min_version` — the commit's version is used up and must be re-signed
//! with a higher one, but no older image can be replayed. The opposite
//! order would let the new image boot with the old `min_version`.

use crate::image::{SECTOR, WORD};
use crate::store::StoreError;
use embedded_storage::nor_flash::NorFlash;
use middleware::crc::Crc32;
use middleware::loader::trust::{KEY_LEN, PublicKey, TrustState, Version};

/// Bytes of one record on flash.
pub const RECORD_LEN: usize = 52;
/// Smallest trust partition: two sectors.
pub const PARTITION_LEN: u32 = 2 * SECTOR;
const MAGIC: [u8; 4] = *b"MWTR";
const FORMAT: u8 = 1;
const CRC_AT: usize = 48;

/// A record: the trust state and its sequence number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    /// Save counter; the newest valid record wins.
    pub seq: u32,
    /// The persisted state.
    pub state: TrustState,
}

impl Record {
    /// The record's bytes.
    pub fn encode(&self) -> [u8; RECORD_LEN] {
        let mut b = [0u8; RECORD_LEN];
        let (flag, min, key) = match self.state {
            TrustState::Unclaimed => (0u8, 0u32, [0u8; KEY_LEN]),
            TrustState::Claimed { key, min_version } => (1, min_version.0, key.0),
        };
        b[..4].copy_from_slice(&MAGIC);
        b[4] = FORMAT;
        b[5] = flag;
        b[8..12].copy_from_slice(&self.seq.to_le_bytes());
        b[12..16].copy_from_slice(&min.to_le_bytes());
        b[16..48].copy_from_slice(&key);
        let crc = Crc32::checksum(&b[..CRC_AT]);
        b[CRC_AT..].copy_from_slice(&crc.to_le_bytes());
        b
    }

    /// Decodes a record; `None` unless magic, format, CRC and fields are
    /// all valid.
    pub fn decode(b: &[u8; RECORD_LEN]) -> Option<Self> {
        let word = |at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
        if b[..4] != MAGIC || b[4] != FORMAT || b[6] != 0 || b[7] != 0 {
            return None;
        }
        if Crc32::checksum(&b[..CRC_AT]) != word(CRC_AT) {
            return None;
        }
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(&b[16..48]);
        let state = match b[5] {
            0 if word(12) == 0 && key == [0; KEY_LEN] => TrustState::Unclaimed,
            1 => TrustState::Claimed { key: PublicKey(key), min_version: Version(word(12)) },
            _ => return None,
        };
        Some(Record { seq: word(8), state })
    }
}

/// The trust partition: which sector holds the current record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustStore {
    base: u32,
    /// Sector index (0 or 1) and sequence of the current record.
    current: Option<(u32, u32)>,
}

impl TrustStore {
    /// Reads the partition at absolute offset `base` (sector aligned, at
    /// least [`PARTITION_LEN`] bytes) and returns the store and the newest
    /// valid state (Unclaimed when there is none). Unreadable sectors count
    /// as invalid.
    pub fn open<F: NorFlash>(flash: &mut F, base: u32, size: u32) -> Result<(Self, TrustState), StoreError> {
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
        let store = TrustStore { base, current: newest.map(|(s, r)| (s, r.seq)) };
        Ok((store, newest.map_or(TrustState::Unclaimed, |(_, r)| r.state)))
    }

    /// Persists `state` in the sector that does not hold the current
    /// record (see the module docs). On error the previous record is still
    /// the one [`TrustStore::open`] finds.
    pub fn save<F: NorFlash>(&mut self, flash: &mut F, state: &TrustState) -> Result<(), StoreError> {
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u32 = SECTOR;
    type Flash = crate::ram_flash::RamFlash<{ (SECTOR + PARTITION_LEN) as usize }>;

    fn claimed(k: u8, min: u32) -> TrustState {
        TrustState::Claimed { key: PublicKey([k; 32]), min_version: Version(min) }
    }

    fn open(flash: &mut Flash) -> (TrustStore, TrustState) {
        TrustStore::open(flash, BASE, PARTITION_LEN).unwrap()
    }

    #[test]
    fn records_round_trip_and_reject_damage() {
        for state in [TrustState::Unclaimed, claimed(3, 1_761_500_000)] {
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
        assert_eq!(open(&mut flash).1, TrustState::Unclaimed);
        flash.mem.fill(0xFF);
        assert_eq!(open(&mut flash).1, TrustState::Unclaimed);
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
            let new = claimed(0xEE, 4242);
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
                    None => assert_eq!(got, TrustState::Unclaimed),
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
}
