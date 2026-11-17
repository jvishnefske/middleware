//! The update record the bootloader reads (RT6, RT7): the
//! `middleware::fw_ota` record plus the two boot facts the RT1050 needs
//! because its boot ROM has no A/B support.
//!
//! [`BootRecord`] is `middleware::fw_ota`'s [`FwRecord`] (anti-rollback
//! floor, the pending update and whether its reboot was requested) with
//! the **confirmed slot** and the pending trial's **attempts**. The RP2350
//! keeps these in its boot ROM (the TBYB flag, the buy); here:
//!
//! | RP2350 boot ROM                         | RT1050 record                                   |
//! |-----------------------------------------|-------------------------------------------------|
//! | flash-update reboot into a TBYB image   | `pending.rebooted` set, `attempts` 0 ([`BootRecord::trial`]) |
//! | the trial boot (buy pending)            | the bootloader persists `attempts` 1, then jumps |
//! | trial watchdog / reset without a buy    | `attempts` 1 at the next boot: roll back         |
//! | `explicit_buy`                          | `confirmed` = the trial slot ([`BootRecord::confirmed_as`]) |
//! | normal boot: the bought image           | `confirmed`                                      |
//!
//! # Format and placement
//!
//! [`RECORD_LEN`] bytes, little-endian, at the start of one of the two
//! update-record blocks:
//!
//! | offset | field |
//! |--------|-------|
//! | 0      | the `FwRecord` encoding with the sequence number (60 bytes, its own magic `MWFW` and CRC) |
//! | 60     | confirmed slot (0 A, 1 B) |
//! | 61     | trial attempts |
//! | 62     | zero (2 bytes) |
//! | 64     | CRC-32 of bytes 0..64 |
//!
//! [`save`] writes the next record (sequence number + 1) into the block
//! that does *not* hold the current one: erase that block, program the
//! record. [`load`] decodes both blocks and takes the valid one with the
//! higher sequence number. A power cut during a save leaves a torn record
//! that fails its CRCs (or an erased block), so the previous record is
//! read: every save is atomic. With no valid record (a factory-flashed
//! board) the record is [`BootRecord::FACTORY`]: nothing pending, slot A
//! confirmed.
//!
//! A save erases a whole 256 KiB block; an update saves the record five
//! or six times (commit, reboot, the bootloader's attempt, confirm,
//! settle), far inside the S26KS512S's endurance.

use super::io::{IoError, read_at, write_at};
use crate::layout::{BLOCK, Region};
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use middleware::crc::Crc32;
use middleware::fw_ota::Slot;
use middleware::fw_ota::record::{FwRecord, RECORD_LEN as FW_RECORD_LEN};

/// Encoded record length.
pub const RECORD_LEN: usize = 68;
/// Boots a pending update gets on trial before it rolls back.
pub const MAX_TRIAL_ATTEMPTS: u8 = 1;

/// The persisted boot state. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootRecord {
    /// The updater's record.
    pub fw: FwRecord,
    /// The slot that boots when no trial is due.
    pub confirmed: Slot,
    /// Boots of the pending update on trial so far.
    pub attempts: u8,
}

impl BootRecord {
    /// A board with no record: nothing pending, slot A confirmed.
    pub const FACTORY: BootRecord = BootRecord {
        fw: FwRecord::EMPTY,
        confirmed: Slot::A,
        attempts: 0,
    };

    /// The slot of a requested trial: a pending update whose reboot was
    /// requested, in a slot other than the confirmed one (once confirmed
    /// it is no longer on trial, even before the record is settled).
    pub fn trial(&self) -> Option<Slot> {
        self.fw
            .pending
            .filter(|p| p.rebooted && p.slot != self.confirmed)
            .map(|p| p.slot)
    }

    /// Whether the requested trial still has a boot left.
    pub fn trial_left(&self) -> bool {
        self.trial().is_some() && self.attempts < MAX_TRIAL_ATTEMPTS
    }

    /// This record with the updater's record `fw` (`FwPlatform::persist`):
    /// the attempts carry over only while the pending update is the same,
    /// so a new pending update or a requested reboot starts at 0.
    pub fn with_fw(self, fw: FwRecord) -> BootRecord {
        let attempts = if fw.pending.is_some() && fw.pending == self.fw.pending {
            self.attempts
        } else {
            0
        };
        BootRecord {
            fw,
            attempts,
            ..self
        }
    }

    /// One more trial boot (what the bootloader persists before the
    /// jump).
    pub fn with_attempt(self) -> BootRecord {
        BootRecord {
            attempts: self.attempts.saturating_add(1),
            ..self
        }
    }

    /// `slot` confirmed (the RT1050's explicit buy).
    pub fn confirmed_as(self, slot: Slot) -> BootRecord {
        BootRecord {
            confirmed: slot,
            attempts: 0,
            ..self
        }
    }

    /// Encodes with sequence number `seq`.
    pub fn encode(&self, seq: u32) -> [u8; RECORD_LEN] {
        let mut out = [0u8; RECORD_LEN];
        out[..FW_RECORD_LEN].copy_from_slice(&self.fw.encode(seq));
        out[60] = self.confirmed as u8;
        out[61] = self.attempts;
        let crc = Crc32::checksum(&out[..64]);
        out[64..].copy_from_slice(&crc.to_le_bytes());
        out
    }

    /// Decodes a record and its sequence number; `None` unless both CRCs,
    /// the `FwRecord` fields and the slot are valid.
    pub fn decode(b: &[u8]) -> Option<(u32, BootRecord)> {
        let b = b.get(..RECORD_LEN)?;
        let crc = u32::from_le_bytes([b[64], b[65], b[66], b[67]]);
        if Crc32::checksum(&b[..64]) != crc || b[62] != 0 || b[63] != 0 {
            return None;
        }
        let (seq, fw) = FwRecord::decode(&b[..FW_RECORD_LEN])?;
        let confirmed = Slot::from_byte(b[60])?;
        Some((
            seq,
            BootRecord {
                fw,
                confirmed,
                attempts: b[61],
            },
        ))
    }
}

/// The record as found on flash, and where.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stored {
    /// The record ([`BootRecord::FACTORY`] when none is valid).
    pub record: BootRecord,
    /// The block (0 or 1) and sequence number of the valid record, if any.
    pub at: Option<(u32, u32)>,
}

/// Reads both blocks of `region` (the update record) and returns the
/// newest valid record.
pub fn load<F: ReadNorFlash>(flash: &mut F, region: Region) -> Result<Stored, IoError<F::Error>> {
    let mut best: Option<(u32, u32, BootRecord)> = None;
    for block in 0..2u32 {
        let mut b = [0u8; RECORD_LEN];
        read_at(flash, region.offset + block * BLOCK, &mut b)?;
        if let Some((seq, r)) = BootRecord::decode(&b)
            && best.is_none_or(|(_, s, _)| seq > s)
        {
            best = Some((block, seq, r));
        }
    }
    Ok(match best {
        Some((block, seq, record)) => Stored {
            record,
            at: Some((block, seq)),
        },
        None => Stored {
            record: BootRecord::FACTORY,
            at: None,
        },
    })
}

/// Saves `next` after `current` (what [`load`] or the previous save
/// returned): erases the other block and programs the record there with
/// the next sequence number. On error the flash still holds `current` as
/// the newest valid record.
pub fn save<F: NorFlash>(
    flash: &mut F,
    region: Region,
    current: &Stored,
    next: BootRecord,
) -> Result<Stored, IoError<F::Error>> {
    let (block, seq) = match current.at {
        Some((b, s)) => (1 - b.min(1), s.wrapping_add(1)),
        None => (0, 1),
    };
    let at = region.offset + block * BLOCK;
    flash.erase(at, at + BLOCK).map_err(IoError::Flash)?;
    write_at(flash, at, &next.encode(seq))?;
    Ok(Stored {
        record: next,
        at: Some((block, seq)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ab::test_flash::BlockFlash;
    use crate::layout::RT1050_EVKB;
    use middleware::fw_ota::record::Pending;
    use middleware::loader::trust::Version;

    fn pending(slot: Slot, rebooted: bool) -> FwRecord {
        FwRecord {
            min_version: Version(0x0001_0001),
            pending: Some(Pending {
                slot,
                version: Version(0x0001_0001),
                len: 4096,
                sha256: [9; 32],
                rebooted,
            }),
        }
    }

    #[test]
    fn rt6_records_round_trip_and_reject_damage() {
        let r = BootRecord {
            fw: pending(Slot::B, true),
            confirmed: Slot::A,
            attempts: 1,
        };
        for (seq, rec) in [(1, BootRecord::FACTORY), (u32::MAX, r)] {
            let bytes = rec.encode(seq);
            assert_eq!(BootRecord::decode(&bytes), Some((seq, rec)));
            for bit in 0..RECORD_LEN * 8 {
                let mut torn = bytes;
                torn[bit / 8] ^= 1 << (bit % 8);
                assert_eq!(BootRecord::decode(&torn), None, "bit {bit}");
            }
        }
        assert_eq!(BootRecord::decode(&[0xFF; RECORD_LEN]), None);
        assert_eq!(BootRecord::decode(&r.encode(1)[..RECORD_LEN - 1]), None);
    }

    #[test]
    fn rt6_the_record_maps_the_trial_and_the_buy() {
        let committed = BootRecord::FACTORY.with_fw(pending(Slot::B, false));
        assert_eq!(committed.trial(), None, "not rebooted yet");
        let requested = committed.with_fw(pending(Slot::B, true));
        assert_eq!((requested.trial(), requested.attempts), (Some(Slot::B), 0));
        assert!(requested.trial_left());
        let tried = requested.with_attempt();
        assert_eq!(
            (tried.trial(), tried.attempts, tried.trial_left()),
            (Some(Slot::B), 1, false)
        );
        // Re-persisting the same pending update keeps the attempts.
        assert_eq!(tried.with_fw(pending(Slot::B, true)).attempts, 1);
        // Bought: B confirmed, no longer a trial even before settling.
        let bought = tried.confirmed_as(Slot::B);
        assert_eq!((bought.trial(), bought.confirmed), (None, Slot::B));
        let settled = bought.with_fw(bought.fw.settled());
        assert_eq!(
            settled,
            BootRecord {
                fw: FwRecord {
                    min_version: Version(0x0001_0001),
                    pending: None
                },
                confirmed: Slot::B,
                attempts: 0
            }
        );
    }

    #[test]
    fn rt6_saves_alternate_blocks_and_load_takes_the_newest() {
        let region = RT1050_EVKB.record;
        let mut flash = BlockFlash::erased(region.end() as usize);
        let s0 = load(&mut flash, region).unwrap();
        assert_eq!(
            s0,
            Stored {
                record: BootRecord::FACTORY,
                at: None
            }
        );
        let a = BootRecord::FACTORY.with_fw(pending(Slot::B, false));
        let s1 = save(&mut flash, region, &s0, a).unwrap();
        assert_eq!(s1.at, Some((0, 1)));
        let b = a.with_fw(pending(Slot::B, true));
        let s2 = save(&mut flash, region, &s1, b).unwrap();
        assert_eq!(s2.at, Some((1, 2)));
        assert_eq!(load(&mut flash, region).unwrap(), s2);
        let c = b.with_attempt();
        let s3 = save(&mut flash, region, &s2, c).unwrap();
        assert_eq!(s3.at, Some((0, 3)));
        assert_eq!(load(&mut flash, region).unwrap(), s3);
    }

    #[test]
    fn rt8_a_cut_save_leaves_the_old_record_or_the_new_one() {
        let region = RT1050_EVKB.record;
        let mut base = BlockFlash::erased(region.end() as usize);
        let first = BootRecord::FACTORY.with_fw(pending(Slot::B, false));
        let s0 = load(&mut base, region).unwrap();
        let s1 = save(&mut base, region, &s0, first).unwrap();
        let next = first.with_fw(pending(Slot::B, true));
        for cut in 0..3 {
            let mut flash = base.clone();
            flash.cut_at(cut);
            let done = save(&mut flash, region, &s1, next).is_ok();
            flash.restore();
            let got = load(&mut flash, region).unwrap().record;
            assert_eq!(got, if done { next } else { first }, "cut {cut}");
            assert_eq!(done, cut >= 2, "a save is an erase and one program");
        }
    }
}
