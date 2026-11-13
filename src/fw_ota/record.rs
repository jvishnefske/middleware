//! The persisted firmware-update record: the anti-rollback floor and the
//! update in flight.
//!
//! [`FwRecord`] is what [`super::FwPlatform::persist`] stores and
//! [`super::FwPlatform::load_record`] returns. The platform must make a
//! save atomic: after a power cut either the previous record or the new one
//! is read back, never a mix. [`FwRecord::encode`] and
//! [`FwRecord::decode`] give it a self-checking [`RECORD_LEN`]-byte format
//! for the usual two-sector scheme (as `firmware/src/trust_store.rs` does
//! for the trust record): write the new record with the next sequence
//! number into the sector that does *not* hold the current one, fields
//! first and magic last, and at load take the valid record with the higher
//! sequence number ([`newest`]).
//!
//! | offset | field                                                    |
//! |--------|----------------------------------------------------------|
//! | 0      | magic `b"MWFW"`                                          |
//! | 4      | format (1)                                               |
//! | 5      | flags: bit 0 pending, bit 1 rebooted                     |
//! | 6      | pending slot (0 A, 1 B; 0 when none)                     |
//! | 7      | zero                                                     |
//! | 8      | sequence number u32 LE                                   |
//! | 12     | `min_version` u32 LE                                     |
//! | 16     | pending version u32 LE                                   |
//! | 20     | pending image length u32 LE                              |
//! | 24     | pending image SHA-256 (32 bytes)                         |
//! | 56     | CRC-32 of bytes 0..56, u32 LE                            |
//!
//! `min_version` only ever grows ([`FwRecord::lowers`] lets the shell
//! refuse a save that would lower it).

use super::Slot;
use crate::crc::Crc32;
use crate::loader::trust::Version;

/// Encoded record length.
pub const RECORD_LEN: usize = 60;
/// Record magic.
pub const RECORD_MAGIC: &[u8; 4] = b"MWFW";
/// Record format.
pub const RECORD_FORMAT: u8 = 1;

const FLAG_PENDING: u8 = 1;
const FLAG_REBOOTED: u8 = 2;

/// A committed update that is not yet confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pending {
    /// The slot holding the new image.
    pub slot: Slot,
    /// Its version.
    pub version: Version,
    /// Its length in bytes.
    pub len: u32,
    /// Its SHA-256 as committed (before the bootrom's explicit buy rewrites
    /// the TBYB flag).
    pub sha256: [u8; 32],
    /// Whether the flash-update reboot into it was requested.
    pub rebooted: bool,
}

/// The persisted state of firmware updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FwRecord {
    /// Every new image's version must exceed this (the last committed
    /// version; 0 on a new device).
    pub min_version: Version,
    /// The update in flight, if any.
    pub pending: Option<Pending>,
}

impl FwRecord {
    /// No update ever committed.
    pub const EMPTY: FwRecord = FwRecord {
        min_version: Version(0),
        pending: None,
    };

    /// Whether saving `next` over `self` would lower `min_version`.
    pub fn lowers(&self, next: &FwRecord) -> bool {
        next.min_version < self.min_version
    }

    /// This record with no update in flight.
    pub const fn settled(self) -> FwRecord {
        FwRecord {
            min_version: self.min_version,
            pending: None,
        }
    }

    /// Encodes with sequence number `seq`.
    pub fn encode(&self, seq: u32) -> [u8; RECORD_LEN] {
        let mut out = [0u8; RECORD_LEN];
        out[..4].copy_from_slice(RECORD_MAGIC);
        out[4] = RECORD_FORMAT;
        if let Some(p) = self.pending {
            out[5] = FLAG_PENDING | if p.rebooted { FLAG_REBOOTED } else { 0 };
            out[6] = p.slot as u8;
            out[16..20].copy_from_slice(&p.version.0.to_le_bytes());
            out[20..24].copy_from_slice(&p.len.to_le_bytes());
            out[24..56].copy_from_slice(&p.sha256);
        }
        out[8..12].copy_from_slice(&seq.to_le_bytes());
        out[12..16].copy_from_slice(&self.min_version.0.to_le_bytes());
        let crc = Crc32::checksum(&out[..56]);
        out[56..60].copy_from_slice(&crc.to_le_bytes());
        out
    }

    /// Decodes a record, returning its sequence number; `None` unless the
    /// magic, format, CRC and fields are valid (an erased or torn record is
    /// `None`).
    pub fn decode(bytes: &[u8]) -> Option<(u32, FwRecord)> {
        let b = bytes.get(..RECORD_LEN)?;
        let u32_at = |at: usize| -> Option<u32> {
            let w = b.get(at..at + 4)?;
            Some(u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
        };
        if b.get(..4)? != RECORD_MAGIC || b[4] != RECORD_FORMAT || b[7] != 0 {
            return None;
        }
        if Crc32::checksum(b.get(..56)?) != u32_at(56)? {
            return None;
        }
        let flags = b[5];
        if flags & !(FLAG_PENDING | FLAG_REBOOTED) != 0 {
            return None;
        }
        let pending = if flags & FLAG_PENDING != 0 {
            Some(Pending {
                slot: Slot::from_byte(b[6])?,
                version: Version(u32_at(16)?),
                len: u32_at(20)?,
                sha256: <[u8; 32]>::try_from(b.get(24..56)?).ok()?,
                rebooted: flags & FLAG_REBOOTED != 0,
            })
        } else if flags != 0 || b[6] != 0 {
            return None;
        } else {
            None
        };
        Some((
            u32_at(8)?,
            FwRecord {
                min_version: Version(u32_at(12)?),
                pending,
            },
        ))
    }
}

/// The record to use of the two candidates read from the two sectors: the
/// valid one with the higher sequence number.
pub fn newest(a: Option<(u32, FwRecord)>, b: Option<(u32, FwRecord)>) -> Option<(u32, FwRecord)> {
    match (a, b) {
        (Some(x), Some(y)) => Some(if y.0 > x.0 { y } else { x }),
        (x, None) => x,
        (None, y) => y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(rebooted: bool) -> FwRecord {
        FwRecord {
            min_version: Version(0x0001_0002),
            pending: Some(Pending {
                slot: Slot::B,
                version: Version(0x0001_0002),
                len: 123_456,
                sha256: [0xA5; 32],
                rebooted,
            }),
        }
    }

    #[test]
    fn y10_records_round_trip_and_reject_damage() {
        for (seq, r) in [
            (0, FwRecord::EMPTY),
            (7, pending(false)),
            (u32::MAX, pending(true)),
        ] {
            let bytes = r.encode(seq);
            assert_eq!(FwRecord::decode(&bytes), Some((seq, r)));
            // Any single flipped bit is caught (CRC or field check).
            for bit in 0..RECORD_LEN * 8 {
                let mut torn = bytes;
                torn[bit / 8] ^= 1 << (bit % 8);
                assert_eq!(FwRecord::decode(&torn), None, "bit {bit}");
            }
        }
        assert_eq!(FwRecord::decode(&[0xFF; RECORD_LEN]), None, "erased");
        assert_eq!(
            FwRecord::decode(&pending(true).encode(1)[..RECORD_LEN - 1]),
            None
        );
        assert_eq!(
            pending(true).settled(),
            FwRecord {
                min_version: Version(0x0001_0002),
                pending: None
            }
        );
    }

    #[test]
    fn y10_newest_takes_the_higher_sequence_and_floors_never_drop() {
        let a = Some((3, FwRecord::EMPTY));
        let b = Some((4, pending(false)));
        assert_eq!(newest(a, b), b);
        assert_eq!(newest(b, a), b);
        assert_eq!(newest(None, a), a);
        assert_eq!(newest(a, None), a);
        assert_eq!(newest(None, None), None);
        assert!(pending(false).lowers(&FwRecord::EMPTY));
        assert!(!FwRecord::EMPTY.lowers(&pending(false)));
    }
}
