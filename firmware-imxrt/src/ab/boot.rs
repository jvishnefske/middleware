//! The bootloader's boot decision (RT6), pure.
//!
//! [`decide`] takes the persisted [`BootRecord`] and a `check` function
//! (the shell's [`check_slot`]: copy the slot's image to SDRAM and verify
//! it, [`super::header::check_copy`]) and returns what to boot. It checks
//! slots one at a time, in the order the rules below give, and boots the
//! first that passes; so the slot it boots is always the **last one
//! checked**, and the SDRAM copy is that slot's verified image. A slot
//! that fails its check never boots.
//!
//! 1. A trial is due ([`BootRecord::trial`] with a boot left,
//!    [`BootRecord::trial_left`]): the trial slot boots on trial if it
//!    passes, and the decision carries the record with the attempt
//!    counted ([`BootRecord::with_attempt`]), which the shell **persists
//!    before the jump** (with the watchdog armed). If the trial slot fails,
//!    the other slot boots normally.
//! 2. A trial was requested but its boot is spent (it reset without
//!    `sys/fw/confirm`: the watchdog, a panic, a power cut): roll back,
//!    the other slot (the one that ran when the update was committed)
//!    boots normally. The spent trial slot is never booted again: if the
//!    rollback slot fails too, the decision is recovery, with the trial
//!    slot's reason [`Refusal::TrialSpent`].
//! 3. Otherwise the confirmed slot boots if it passes, else the other
//!    slot.
//! 4. Neither passes: [`Decision::Recovery`], with each slot's reason; the
//!    bootloader boots nothing and prints them on LPUART1.
//!
//! If the attempt cannot be persisted, the shell must not jump into the
//! trial: it decides again with the attempt counted in RAM
//! (`decide(&record.with_attempt(), ..)`), which rolls back.
//!
//! The firmware learns what was booted from a [`Handoff`] word the
//! bootloader leaves for it (where it is kept is the shell's choice, a
//! register that survives the jump).

use super::header::{ImageDest, Refusal, SlotHeader, check_copy};
use super::io::read_at;
use super::record::BootRecord;
use crate::layout::AbMap;
use embedded_storage::nor_flash::ReadNorFlash;
use middleware::fw_ota::Slot;

/// What the bootloader does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Jump into `slot`'s image, already copied to SDRAM and verified.
    Boot {
        /// The slot.
        slot: Slot,
        /// Booted on trial (the watchdog is armed; the firmware must
        /// confirm).
        trial: bool,
        /// The slot's verified header.
        header: SlotHeader,
        /// The record to persist **before** the jump (the trial attempt).
        persist: Option<BootRecord>,
    },
    /// Boot nothing: neither slot passed.
    Recovery {
        /// Why slot A failed.
        a: Refusal,
        /// Why slot B failed.
        b: Refusal,
    },
}

impl Decision {
    /// What the booted firmware is told, for a boot.
    pub const fn handoff(&self) -> Option<Handoff> {
        match *self {
            Decision::Boot { slot, trial, .. } => Some(Handoff { slot, trial }),
            Decision::Recovery { .. } => None,
        }
    }
}

/// How one candidate boots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Trial,
}

/// The boot decision. See the module docs for the rules.
pub fn decide<C>(record: &BootRecord, mut check: C) -> Decision
where
    C: FnMut(Slot) -> Result<SlotHeader, Refusal>,
{
    let mut refusals = [Refusal::Unreadable; 2];
    let candidates = match record.trial() {
        Some(t) if record.trial_left() => [Some((t, Mode::Trial)), Some((t.other(), Mode::Normal))],
        Some(t) => {
            refusals[t as usize] = Refusal::TrialSpent;
            [Some((t.other(), Mode::Normal)), None]
        }
        None => [
            Some((record.confirmed, Mode::Normal)),
            Some((record.confirmed.other(), Mode::Normal)),
        ],
    };
    // Bounded: at most two candidates.
    for (slot, mode) in candidates.into_iter().flatten() {
        match check(slot) {
            Ok(header) => {
                let trial = mode == Mode::Trial;
                return Decision::Boot {
                    slot,
                    trial,
                    header,
                    persist: trial.then(|| record.with_attempt()),
                };
            }
            Err(r) => refusals[slot as usize] = r,
        }
    }
    Decision::Recovery {
        a: refusals[0],
        b: refusals[1],
    }
}

/// The integrity check of `slot` on `flash`: [`check_copy`] over the
/// slot's region of `ab`, copying into `dest` (the SDRAM image region).
pub fn check_slot<F: ReadNorFlash, D: ImageDest + ?Sized>(
    flash: &mut F,
    ab: &AbMap,
    slot: Slot,
    dest: &mut D,
) -> Result<SlotHeader, Refusal> {
    let region = ab.slot(slot);
    let mut read = |at: u32, buf: &mut [u8]| {
        let end = at
            .checked_add(buf.len() as u32)
            .filter(|&e| e <= region.size);
        match end {
            Some(_) => read_at(flash, region.offset + at, buf),
            None => Err(super::io::IoError::Granule),
        }
    };
    check_copy(&mut read, region.size, dest)
}

/// What the bootloader tells the firmware it booted: the slot and whether
/// on trial. Encoded in one word: `0x4D57_AB00 | trial << 1 | slot`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handoff {
    /// The booted slot.
    pub slot: Slot,
    /// Booted on trial.
    pub trial: bool,
}

const HANDOFF_MAGIC: u32 = 0x4D57_AB00;

impl Handoff {
    /// The handoff word.
    pub const fn encode(&self) -> u32 {
        HANDOFF_MAGIC | (self.trial as u32) << 1 | self.slot as u32
    }

    /// Decodes a handoff word; `None` for anything the bootloader did not
    /// write (a cold start without the bootloader, a debugger load).
    pub const fn decode(word: u32) -> Option<Handoff> {
        if word & !3 != HANDOFF_MAGIC {
            return None;
        }
        let slot = if word & 1 == 0 { Slot::A } else { Slot::B };
        Some(Handoff {
            slot,
            trial: word & 2 != 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::fw_ota::record::{FwRecord, Pending};
    use middleware::loader::trust::Version;

    fn header(v: u32) -> SlotHeader {
        SlotHeader {
            version: Version(v),
            len: 1000,
            sha256: [v as u8; 32],
        }
    }

    /// A checker over fixed verdicts, recording the order of the checks.
    fn verdicts(
        a: Result<SlotHeader, Refusal>,
        b: Result<SlotHeader, Refusal>,
        order: &mut heapless::Vec<Slot, 4>,
    ) -> impl FnMut(Slot) -> Result<SlotHeader, Refusal> + '_ {
        move |s| {
            let _ = order.push(s);
            match s {
                Slot::A => a,
                Slot::B => b,
            }
        }
    }

    fn requested(slot: Slot, attempts: u8) -> BootRecord {
        BootRecord {
            fw: FwRecord {
                min_version: Version(2),
                pending: Some(Pending {
                    slot,
                    version: Version(2),
                    len: 1256,
                    sha256: [2; 32],
                    rebooted: true,
                }),
            },
            confirmed: slot.other(),
            attempts,
        }
    }

    fn booted(d: &Decision) -> Option<(Slot, bool, bool)> {
        match *d {
            Decision::Boot {
                slot,
                trial,
                persist,
                ..
            } => Some((slot, trial, persist.is_some())),
            Decision::Recovery { .. } => None,
        }
    }

    #[test]
    fn rt6_the_confirmed_slot_boots_when_it_passes() {
        for confirmed in [Slot::A, Slot::B] {
            let r = BootRecord {
                confirmed,
                ..BootRecord::FACTORY
            };
            let mut order = heapless::Vec::new();
            let d = decide(&r, verdicts(Ok(header(1)), Ok(header(2)), &mut order));
            assert_eq!(booted(&d), Some((confirmed, false, false)));
            assert_eq!(order.as_slice(), &[confirmed], "one check");
        }
        // A committed update not yet rebooted does not change the boot.
        let mut committed = requested(Slot::B, 0);
        if let Some(p) = committed.fw.pending.as_mut() {
            p.rebooted = false;
        }
        let mut order = heapless::Vec::new();
        let d = decide(
            &committed,
            verdicts(Ok(header(1)), Ok(header(2)), &mut order),
        );
        assert_eq!(booted(&d), Some((Slot::A, false, false)));
    }

    #[test]
    fn rt6_a_pending_update_boots_once_on_trial_with_the_attempt_persisted_first() {
        let r = requested(Slot::B, 0);
        let mut order = heapless::Vec::new();
        let d = decide(&r, verdicts(Ok(header(1)), Ok(header(2)), &mut order));
        assert_eq!(
            d,
            Decision::Boot {
                slot: Slot::B,
                trial: true,
                header: header(2),
                persist: Some(r.with_attempt()),
            }
        );
        assert_eq!(order.as_slice(), &[Slot::B]);
        assert_eq!(
            d.handoff(),
            Some(Handoff {
                slot: Slot::B,
                trial: true
            })
        );
    }

    #[test]
    fn rt6_an_unconfirmed_trial_rolls_back() {
        // The trial booted (attempt persisted) and reset without confirm.
        let r = requested(Slot::B, 0).with_attempt();
        let mut order = heapless::Vec::new();
        let d = decide(&r, verdicts(Ok(header(1)), Ok(header(2)), &mut order));
        assert_eq!(booted(&d), Some((Slot::A, false, false)));
        assert_eq!(order.as_slice(), &[Slot::A]);
        // The same when the attempt could not be persisted: the shell
        // decides again with it counted in RAM.
        let again = decide(&requested(Slot::B, 0).with_attempt(), |s| match s {
            Slot::A => Ok(header(1)),
            Slot::B => Ok(header(2)),
        });
        assert_eq!(booted(&again), Some((Slot::A, false, false)));
        // A confirmed trial (bought, record not yet settled) boots normally.
        let bought = r.confirmed_as(Slot::B);
        let mut order = heapless::Vec::new();
        let d = decide(&bought, verdicts(Ok(header(1)), Ok(header(2)), &mut order));
        assert_eq!(booted(&d), Some((Slot::B, false, false)));
    }

    #[test]
    fn rt6_a_failing_slot_never_boots_and_the_other_boots_if_it_passes() {
        let bad = Err(Refusal::HashMismatch);
        // Confirmed slot fails: the other boots.
        let mut order = heapless::Vec::new();
        let d = decide(
            &BootRecord::FACTORY,
            verdicts(bad, Ok(header(2)), &mut order),
        );
        assert_eq!(booted(&d), Some((Slot::B, false, false)));
        assert_eq!(
            order.as_slice(),
            &[Slot::A, Slot::B],
            "the booted slot is checked last"
        );
        // A trial whose slot fails: the other boots normally, nothing
        // persisted.
        let mut order = heapless::Vec::new();
        let d = decide(
            &requested(Slot::B, 0),
            verdicts(Ok(header(1)), bad, &mut order),
        );
        assert_eq!(booted(&d), Some((Slot::A, false, false)));
        assert_eq!(order.as_slice(), &[Slot::B, Slot::A]);
        // A spent trial whose rollback slot fails: recovery. The spent
        // trial slot is never checked or booted again, however good it is.
        let spent = requested(Slot::B, 1);
        let mut order = heapless::Vec::new();
        let d = decide(&spent, verdicts(bad, Ok(header(2)), &mut order));
        assert_eq!(
            d,
            Decision::Recovery {
                a: Refusal::HashMismatch,
                b: Refusal::TrialSpent,
            }
        );
        assert_eq!(order.as_slice(), &[Slot::A]);
    }

    #[test]
    fn rt6_neither_slot_passing_is_recovery_with_both_reasons() {
        // (record, B's reason, slots checked): a spent trial in B is not
        // checked again and reports `TrialSpent`.
        for (r, b, checked) in [
            (BootRecord::FACTORY, Refusal::PastSlot, 2),
            (requested(Slot::A, 0), Refusal::PastSlot, 2),
            (requested(Slot::B, 1), Refusal::TrialSpent, 1),
        ] {
            let mut order = heapless::Vec::new();
            let d = decide(
                &r,
                verdicts(Err(Refusal::BadMagic), Err(Refusal::PastSlot), &mut order),
            );
            assert_eq!(d, Decision::Recovery { a: Refusal::BadMagic, b });
            assert_eq!(d.handoff(), None);
            assert_eq!(order.len(), checked);
        }
    }

    #[test]
    fn rt6_the_handoff_word_round_trips_and_rejects_anything_else() {
        for slot in [Slot::A, Slot::B] {
            for trial in [false, true] {
                let h = Handoff { slot, trial };
                assert_eq!(Handoff::decode(h.encode()), Some(h));
            }
        }
        assert_eq!(
            Handoff {
                slot: Slot::B,
                trial: true
            }
            .encode(),
            0x4D57_AB03
        );
        assert_eq!(Handoff::decode(0), None);
        assert_eq!(Handoff::decode(0xFFFF_FFFF), None);
        assert_eq!(Handoff::decode(0x4D57_AB04), None);
    }
}
