//! The A/B lifecycle on a RAM HyperFlash model: the bootloader's decision
//! and record writes, the firmware's `FwService::<RtImage>` over
//! [`RtPlatform`] (RT7), and a power cut at every flash operation of an
//! update (RT8).

extern crate std;

use super::boot::{Decision, Handoff, check_slot, decide};
use super::header::{HEADER_LEN, SlotHeader};
use super::platform::{AbError, AbState, ResetRequest, RtImage, RtPlatform};
use super::record::{self, BootRecord};
use super::test_flash::{BlockFlash, Op};
use crate::layout::{AbMap, BLOCK, RT1050_EVKB};
use embedded_storage::nor_flash::NorFlashErrorKind;
use middleware::fw_ota::service::{Context, FwService};
use middleware::fw_ota::wire::{Command, FwInfo};
use middleware::fw_ota::{CHUNK_LEN, FwPlatform, FwUpdate, Slot, StateCode};
use middleware::loader::sha256::ImageHash;
use middleware::loader::trust::{TrustState, Version};
use middleware::transport::{HandlerError, SenderId};
use std::vec;
use std::vec::Vec;

const AB: AbMap = RT1050_EVKB.ab();
const NODE: SenderId = SenderId([0x02, 0x10, 0x50, 0, 0, 1, 0, 0]);
const V1_0: u32 = 0x0001_0000;
const V1_1: u32 = 0x0001_0001;
const V1_2: u32 = 0x0001_0002;
/// The image body: a block and a bit, so a slot image spans two blocks.
const BODY: usize = BLOCK as usize + 2000;

type Service = FwService<RtImage>;

fn ctx() -> Context {
    Context {
        node: NODE,
        trust: TrustState::Unclaimed,
        self_test_ok: true,
    }
}

/// A slot image (header + body) of version `v`.
fn slot_image(v: u32, seed: u8) -> Vec<u8> {
    let body: Vec<u8> = (0..BODY)
        .map(|i| (i as u8).wrapping_mul(31) ^ seed)
        .collect();
    let mut out = SlotHeader::for_image(Version(v), &body)
        .unwrap()
        .encode()
        .to_vec();
    out.extend_from_slice(&body);
    out
}

/// A factory-flashed board: `image` in slot A, no record.
fn factory(image: &[u8]) -> BlockFlash {
    let mut f = BlockFlash::erased(AB.b.end() as usize);
    f.program(AB.a.offset as usize, image);
    f
}

/// The SDRAM image region model (reused across boots).
fn sdram() -> Vec<u8> {
    vec![0u8; AB.slot_size() as usize]
}

/// The bootloader: load the record, decide, persist the attempt first.
/// `Err` when the flash fails (power cut) before the jump.
fn bootloader(flash: &mut BlockFlash, dest: &mut [u8]) -> Result<Decision, NorFlashErrorKind> {
    let stored = record::load(flash, AB.record).map_err(|_| NorFlashErrorKind::Other)?;
    let d = decide(&stored.record, |s| check_slot(flash, &AB, s, dest));
    if let Decision::Boot {
        persist: Some(r), ..
    } = d
    {
        record::save(flash, AB.record, &stored, r).map_err(|_| NorFlashErrorKind::Other)?;
    }
    Ok(d)
}

/// The firmware starting after `handoff`.
fn firmware(
    flash: &mut BlockFlash,
    handoff: Handoff,
) -> Result<(AbState, Service), AbError<NorFlashErrorKind>> {
    let mut state = AbState::new(handoff);
    let mut p = RtPlatform::new(flash, &mut state, AB)?;
    let svc = Service::boot_for(&mut p)?;
    Ok((state, svc))
}

/// One request.
fn call(
    svc: &mut Service,
    flash: &mut BlockFlash,
    state: &mut AbState,
    cmd: &Command<'_>,
) -> Result<Vec<u8>, HandlerError> {
    let mut p = RtPlatform::new(flash, state, AB).map_err(|_| HandlerError::Internal)?;
    let mut out = [0u8; 64];
    let n = svc.handle_command(&mut p, &ctx(), cmd, &mut out)?;
    Ok(out[..n].to_vec())
}

/// Chunk offsets in a scrambled order: odd chunks ascending, then even
/// chunks descending (sector 0's chunks last but one).
fn order(len: usize) -> Vec<u32> {
    let n = len.div_ceil(CHUNK_LEN) as u32;
    let odd = (0..n).filter(|i| i % 2 == 1);
    let even = (0..n).filter(|i| i % 2 == 0).rev();
    odd.chain(even).map(|i| i * CHUNK_LEN as u32).collect()
}

/// begin, every chunk, commit, reboot.
fn send(
    svc: &mut Service,
    flash: &mut BlockFlash,
    state: &mut AbState,
    image: &[u8],
    version: u32,
) -> Result<Slot, HandlerError> {
    let begin = Command::Begin {
        len: image.len() as u32,
        version: Version(version),
        sha256: ImageHash::digest(image),
    };
    let slot = call(svc, flash, state, &begin)?;
    for offset in order(image.len()) {
        let end = (offset as usize + CHUNK_LEN).min(image.len());
        let data = &image[offset as usize..end];
        call(svc, flash, state, &Command::Chunk { offset, data })?;
    }
    call(svc, flash, state, &Command::Commit { signature: None })?;
    call(svc, flash, state, &Command::Reboot)?;
    Slot::from_byte(slot[0]).ok_or(HandlerError::Internal)
}

/// The body the decision booted, as copied to SDRAM.
fn booted_body(d: &Decision, dest: &[u8]) -> Option<Vec<u8>> {
    match d {
        Decision::Boot { header, .. } => Some(dest[..header.len as usize].to_vec()),
        Decision::Recovery { .. } => None,
    }
}

fn info(svc: &mut Service, flash: &mut BlockFlash, state: &mut AbState) -> FwInfo {
    FwInfo::decode(&call(svc, flash, state, &Command::Info).unwrap()).unwrap()
}

#[test]
fn rt7_a_full_update_trial_and_confirm() {
    let v1 = slot_image(V1_0, 1);
    let v2 = slot_image(V1_1, 2);
    let mut flash = factory(&v1);
    let mut dest = sdram();

    // Factory boot: slot A, confirmed by default.
    let d = bootloader(&mut flash, &mut dest).unwrap();
    assert_eq!(
        d.handoff(),
        Some(Handoff {
            slot: Slot::A,
            trial: false
        })
    );
    assert_eq!(booted_body(&d, &dest).unwrap(), &v1[HEADER_LEN..]);
    let (mut state, mut svc) = firmware(&mut flash, d.handoff().unwrap()).unwrap();
    assert_eq!(svc.state(), FwUpdate::Idle);
    let i = info(&mut svc, &mut flash, &mut state);
    assert_eq!(
        (i.running, i.slots[0].version, i.slots[1].version),
        (Slot::A, Some(Version(V1_0)), None)
    );

    // begin answers the inactive slot; the update goes there.
    let slot = send(&mut svc, &mut flash, &mut state, &v2, V1_1).unwrap();
    assert_eq!(slot, Slot::B);
    assert_eq!(state.take_reset(), Some(ResetRequest::Update(Slot::B)));
    assert_eq!(state.take_reset(), None);
    let rec = record::load(&mut flash, AB.record).unwrap().record;
    assert_eq!(
        (rec.trial(), rec.attempts, rec.confirmed),
        (Some(Slot::B), 0, Slot::A)
    );
    assert_eq!(rec.fw.min_version, Version(V1_1));

    // The bootloader boots B on trial, the attempt persisted first.
    let d = bootloader(&mut flash, &mut dest).unwrap();
    assert_eq!(
        d.handoff(),
        Some(Handoff {
            slot: Slot::B,
            trial: true
        })
    );
    assert_eq!(booted_body(&d, &dest).unwrap(), &v2[HEADER_LEN..]);
    assert_eq!(
        record::load(&mut flash, AB.record).unwrap().record.attempts,
        1
    );

    // The trial confirms: the record marks B confirmed.
    let (mut state, mut svc) = firmware(&mut flash, d.handoff().unwrap()).unwrap();
    assert!(matches!(svc.state(), FwUpdate::Trial { slot: Slot::B, .. }));
    assert!(!state.trial_released());
    call(&mut svc, &mut flash, &mut state, &Command::Confirm).unwrap();
    assert!(state.trial_released());
    assert!(matches!(
        svc.state(),
        FwUpdate::Confirmed { slot: Slot::B, .. }
    ));
    let rec = record::load(&mut flash, AB.record).unwrap().record;
    assert_eq!(
        rec,
        BootRecord {
            fw: rec.fw.settled(),
            confirmed: Slot::B,
            attempts: 0
        }
    );

    // Every later boot runs B, normally.
    let d = bootloader(&mut flash, &mut dest).unwrap();
    assert_eq!(
        d.handoff(),
        Some(Handoff {
            slot: Slot::B,
            trial: false
        })
    );
    let (mut state, mut svc) = firmware(&mut flash, d.handoff().unwrap()).unwrap();
    assert_eq!(svc.state(), FwUpdate::Idle);
    let i = info(&mut svc, &mut flash, &mut state);
    assert_eq!(i.state, StateCode::Idle);
    assert_eq!(
        (i.slots[0].version, i.slots[1].version),
        (Some(Version(V1_0)), Some(Version(V1_1)))
    );
    assert!(!i.slots[0].tbyb && !i.slots[1].tbyb);

    // And the next update targets A.
    let v3 = slot_image(V1_2, 3);
    assert_eq!(
        send(&mut svc, &mut flash, &mut state, &v3, V1_2),
        Ok(Slot::A)
    );
}

#[test]
fn rt7_each_slot_block_is_erased_once_before_its_first_program_and_the_header_is_written_last() {
    let v2 = slot_image(V1_1, 2);
    let mut flash = factory(&slot_image(V1_0, 1));
    // Slot B holds an old image everywhere, so every erase is needed.
    flash.program(AB.b.offset as usize, &vec![0x5A; 2 * BLOCK as usize]);
    let (mut state, mut svc) = firmware(
        &mut flash,
        Handoff {
            slot: Slot::A,
            trial: false,
        },
    )
    .unwrap();
    flash.log.clear();
    send(&mut svc, &mut flash, &mut state, &v2, V1_1).unwrap();
    let b = AB.b;
    let in_b = |at: u32| at >= b.offset && at < b.end();
    let slot_erases: Vec<u32> = flash
        .log
        .iter()
        .filter_map(|op| match *op {
            Op::Erase(from, to) if in_b(from) => {
                assert_eq!(to - from, BLOCK);
                Some(from - b.offset)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        slot_erases,
        [0, BLOCK],
        "block 0 at begin, block 1 at its first chunk"
    );
    // Each block's erase precedes every write into it.
    for (k, op) in flash.log.iter().enumerate() {
        if let Op::Write(at, _) = *op
            && in_b(at)
        {
            let block = (at - b.offset) / BLOCK;
            let erased_at = flash.log.iter().position(|o| {
                *o == Op::Erase(b.offset + block * BLOCK, b.offset + (block + 1) * BLOCK)
            });
            assert!(erased_at.is_some_and(|e| e < k), "write {k} at {at:#x}");
        }
    }
    // The header (slot offset 0) is the last write into the slot, after
    // the record that makes it pending was saved.
    let last_slot_write = flash
        .log
        .iter()
        .rposition(|o| matches!(*o, Op::Write(at, _) if in_b(at)));
    assert_eq!(
        flash.log[last_slot_write.unwrap()],
        Op::Write(b.offset, 4096)
    );
    let record_write = flash.log.iter().position(
        |o| matches!(*o, Op::Write(at, _) if at >= AB.record.offset && at < AB.record.end()),
    );
    assert!(record_write.unwrap() < last_slot_write.unwrap());
    // What was written is the image.
    assert_eq!(flash.bytes(b.offset as usize, v2.len()), v2);
}

#[test]
fn rt7_commit_refuses_an_image_whose_header_does_not_match_what_was_received() {
    let body: Vec<u8> = vec![0x42; 5000];
    let with = |header: SlotHeader, body: &[u8]| {
        let mut img = header.encode().to_vec();
        img.extend_from_slice(body);
        img
    };
    let good = SlotHeader::for_image(Version(V1_1), &body).unwrap();
    let cases: [(&str, Vec<u8>, u32); 4] = [
        // The header says another version than the one declared.
        (
            "version",
            with(SlotHeader::for_image(Version(V1_2), &body).unwrap(), &body),
            V1_1,
        ),
        // The header's length is not what was received (extra bytes).
        (
            "length",
            {
                let mut img = with(good, &body);
                img.extend_from_slice(&[0; 2]);
                img
            },
            V1_1,
        ),
        // The image does not hash to the header's SHA-256.
        ("hash", with(good, &[0x43; 5000]), V1_1),
        // Not an RT1050 slot image at all.
        ("magic", vec![0xFF; 6000], V1_1),
    ];
    for (what, img, version) in cases {
        let mut flash = factory(&slot_image(V1_0, 1));
        let (mut state, mut svc) = firmware(
            &mut flash,
            Handoff {
                slot: Slot::A,
                trial: false,
            },
        )
        .unwrap();
        let r = send(&mut svc, &mut flash, &mut state, &img, version);
        assert_eq!(r, Err(HandlerError::InvalidRequest), "{what}");
        assert_eq!(svc.state(), FwUpdate::Idle, "{what}: abandoned");
        // Nothing pending; the header sector was never written.
        let rec = record::load(&mut flash, AB.record).unwrap();
        assert_eq!(rec.record.fw.pending, None, "{what}");
        assert_eq!(flash.bytes(AB.b.offset as usize, 4), [0xFF; 4], "{what}");
    }
    // The good one is accepted.
    let mut flash = factory(&slot_image(V1_0, 1));
    let (mut state, mut svc) = firmware(
        &mut flash,
        Handoff {
            slot: Slot::A,
            trial: false,
        },
    )
    .unwrap();
    assert_eq!(
        send(&mut svc, &mut flash, &mut state, &with(good, &body), V1_1),
        Ok(Slot::B)
    );
}

#[test]
fn rt7_an_unconfirmed_trial_or_an_abort_rolls_back_and_the_record_settles() {
    for abort in [false, true] {
        let v1 = slot_image(V1_0, 1);
        let mut flash = factory(&v1);
        let mut dest = sdram();
        let (mut state, mut svc) = firmware(
            &mut flash,
            Handoff {
                slot: Slot::A,
                trial: false,
            },
        )
        .unwrap();
        send(&mut svc, &mut flash, &mut state, &slot_image(V1_1, 2), V1_1).unwrap();
        let d = bootloader(&mut flash, &mut dest).unwrap();
        assert_eq!(
            d.handoff(),
            Some(Handoff {
                slot: Slot::B,
                trial: true
            })
        );
        let (mut state, mut svc) = firmware(&mut flash, d.handoff().unwrap()).unwrap();
        if abort {
            call(&mut svc, &mut flash, &mut state, &Command::Abort).unwrap();
            assert_eq!(state.take_reset(), Some(ResetRequest::Normal));
        }
        // Reset without confirm (watchdog, abort): back to A.
        let d = bootloader(&mut flash, &mut dest).unwrap();
        assert_eq!(
            d.handoff(),
            Some(Handoff {
                slot: Slot::A,
                trial: false
            })
        );
        assert_eq!(booted_body(&d, &dest).unwrap(), &v1[HEADER_LEN..]);
        let (_, svc) = firmware(&mut flash, d.handoff().unwrap()).unwrap();
        assert!(matches!(
            svc.state(),
            FwUpdate::RolledBack { slot: Slot::B, .. }
        ));
        let rec = record::load(&mut flash, AB.record).unwrap().record;
        assert_eq!(
            (rec.fw.pending, rec.confirmed, rec.fw.min_version),
            (None, Slot::A, Version(V1_1))
        );
    }
}

#[test]
fn rt7_the_platform_refuses_the_running_slot_and_a_foreign_geometry() {
    let mut flash = factory(&slot_image(V1_0, 1));
    let mut state = AbState::new(Handoff {
        slot: Slot::A,
        trial: false,
    });
    let mut p = RtPlatform::new(&mut flash, &mut state, AB).unwrap();
    assert_eq!(p.erase(Slot::A, 0), Err(AbError::Running));
    assert_eq!(p.write(Slot::A, 0, &[0; 2]), Err(AbError::Running));
    assert_eq!(p.erase(Slot::B, 1024), Err(AbError::Bounds));
    let mut buf = [0u8; 4];
    assert_eq!(
        p.read(Slot::B, AB.slot_size() - 2, &mut buf),
        Err(AbError::Bounds)
    );
    assert_eq!(p.explicit_buy(), Err(AbError::NotTrial));
    assert_eq!(p.reboot_flash_update(Slot::B), Err(AbError::NotPending));
    assert_eq!(p.slot_size(), 4 * 1024 * 1024);
    let mut small = super::test_flash::SmallEraseFlash;
    let mut state = AbState::new(Handoff {
        slot: Slot::A,
        trial: false,
    });
    assert!(matches!(
        RtPlatform::new(&mut small, &mut state, AB),
        Err(AbError::Geometry)
    ));
}

#[test]
fn rt7_a_fallback_boot_records_the_running_slot_so_the_next_update_still_boots_on_trial() {
    // Slot A (confirmed) is damaged; B holds a good image and boots.
    let mut flash = factory(&slot_image(V1_0, 1));
    flash.program(AB.a.offset as usize + HEADER_LEN + 10, &[0x00]);
    let v2 = slot_image(V1_1, 2);
    flash.program(AB.b.offset as usize, &v2);
    let mut dest = sdram();
    let d = bootloader(&mut flash, &mut dest).unwrap();
    assert_eq!(
        d.handoff(),
        Some(Handoff {
            slot: Slot::B,
            trial: false
        })
    );
    // An update of A (the inactive slot) boots on trial, then confirms.
    let (mut state, mut svc) = firmware(&mut flash, d.handoff().unwrap()).unwrap();
    let v3 = slot_image(V1_2, 3);
    assert_eq!(
        send(&mut svc, &mut flash, &mut state, &v3, V1_2),
        Ok(Slot::A)
    );
    let d = bootloader(&mut flash, &mut dest).unwrap();
    assert_eq!(
        d.handoff(),
        Some(Handoff {
            slot: Slot::A,
            trial: true
        })
    );
    // Unconfirmed: back to B, the slot that ran.
    let d = bootloader(&mut flash, &mut dest).unwrap();
    assert_eq!(
        d.handoff(),
        Some(Handoff {
            slot: Slot::B,
            trial: false
        })
    );
}

/// What a power-cycled board does: boot (bootloader, then the firmware
/// reconciling its state), up to three times with a reset and no confirm
/// in between, each boot running a verified image of `images`.
fn recovers(
    flash: &mut BlockFlash,
    dest: &mut [u8],
    images: &[&[u8]],
    why: &str,
) -> (AbState, Service) {
    let mut last = None;
    for boot in 0..3 {
        let d = bootloader(flash, dest).unwrap();
        let body =
            booted_body(&d, dest).unwrap_or_else(|| panic!("{why}, boot {boot}: recovery {d:?}"));
        assert!(
            images.iter().any(|i| i[HEADER_LEN..] == body[..]),
            "{why}, boot {boot}: an image that was not sent"
        );
        last = Some(firmware(flash, d.handoff().unwrap()).unwrap());
    }
    last.unwrap()
}

/// The update lifecycle of [`rt7_a_full_update_trial_and_confirm`] from the
/// factory board, stopping at the first failed step (the power cut).
fn lifecycle(flash: &mut BlockFlash, dest: &mut [u8], v2: &[u8]) -> Result<(), ()> {
    let d = bootloader(flash, dest).map_err(|_| ())?;
    let (mut state, mut svc) = firmware(flash, d.handoff().ok_or(())?).map_err(|_| ())?;
    send(&mut svc, flash, &mut state, v2, V1_1).map_err(|_| ())?;
    let d = bootloader(flash, dest).map_err(|_| ())?;
    let (mut state, mut svc) = firmware(flash, d.handoff().ok_or(())?).map_err(|_| ())?;
    call(&mut svc, flash, &mut state, &Command::Confirm).map_err(|_| ())?;
    let d = bootloader(flash, dest).map_err(|_| ())?;
    firmware(flash, d.handoff().ok_or(())?).map_err(|_| ())?;
    Ok(())
}

#[test]
fn rt8_a_power_cut_at_every_flash_operation_leaves_a_bootable_verified_slot() {
    let v1 = slot_image(V1_0, 1);
    let v2 = slot_image(V1_1, 2);
    let v3 = slot_image(V1_2, 3);
    let board = factory(&v1);
    let mut dest = sdram();

    // The uncut lifecycle, to count its operations.
    let mut whole = board.clone();
    whole.cut_at(usize::MAX);
    lifecycle(&mut whole, &mut dest, &v2).unwrap();
    let total = whole.ops();
    assert!(
        total > BODY / CHUNK_LEN,
        "every chunk is an operation ({total})"
    );
    let d = bootloader(&mut whole, &mut dest).unwrap();
    assert_eq!(
        d.handoff(),
        Some(Handoff {
            slot: Slot::B,
            trial: false
        })
    );

    for cut in 0..total {
        let mut flash = board.clone();
        flash.cut_at(cut);
        // Stops at the cut (a failed persist after a buy is answered as
        // success, so the lifecycle may run on to the next write).
        let _ = lifecycle(&mut flash, &mut dest, &v2);
        assert!(flash.is_cut(), "cut {cut} was reached");
        flash.restore();
        let why = std::format!("cut at {cut} ({:?})", flash.log.last());
        let (mut state, mut svc) = recovers(&mut flash, &mut dest, &[&v1, &v2], &why);
        // The board is not wedged: a further update (a higher version: a
        // committed version is used up) installs, boots on trial and is
        // confirmed. A committed v1.1 that was never rebooted, or a trial
        // of it still running, is abandoned first.
        if send(&mut svc, &mut flash, &mut state, &v3, V1_2).is_err() {
            let st = svc.state();
            assert!(
                matches!(st, FwUpdate::Committed(_) | FwUpdate::Trial { .. }),
                "{why}: {st:?}"
            );
            call(&mut svc, &mut flash, &mut state, &Command::Abort).unwrap();
            let (mut state, mut svc) = recovers(&mut flash, &mut dest, &[&v1, &v2], &why);
            send(&mut svc, &mut flash, &mut state, &v3, V1_2)
                .unwrap_or_else(|e| panic!("{why}: {e:?}"));
        }
        let d = bootloader(&mut flash, &mut dest).unwrap();
        assert!(matches!(d, Decision::Boot { trial: true, .. }), "{why}");
        assert_eq!(booted_body(&d, &dest).unwrap(), &v3[HEADER_LEN..], "{why}");
        let (mut state, mut svc) = firmware(&mut flash, d.handoff().unwrap()).unwrap();
        call(&mut svc, &mut flash, &mut state, &Command::Confirm).unwrap();
        let d = bootloader(&mut flash, &mut dest).unwrap();
        assert_eq!(booted_body(&d, &dest).unwrap(), &v3[HEADER_LEN..], "{why}");
        assert!(matches!(d, Decision::Boot { trial: false, .. }), "{why}");
    }
}

#[test]
fn rt8_a_power_cut_during_the_bootloaders_attempt_write_keeps_the_board_bootable() {
    let v1 = slot_image(V1_0, 1);
    let v2 = slot_image(V1_1, 2);
    let mut flash = factory(&v1);
    let mut dest = sdram();
    let (mut state, mut svc) = firmware(
        &mut flash,
        Handoff {
            slot: Slot::A,
            trial: false,
        },
    )
    .unwrap();
    send(&mut svc, &mut flash, &mut state, &v2, V1_1).unwrap();
    // The attempt save is an erase and a program.
    for cut in 0..2 {
        let mut f = flash.clone();
        f.cut_at(cut);
        assert!(
            bootloader(&mut f, &mut dest).is_err(),
            "cut {cut}: no jump without the attempt"
        );
        f.restore();
        // The record still requests the trial: it boots now, once.
        let d = bootloader(&mut f, &mut dest).unwrap();
        assert_eq!(
            d.handoff(),
            Some(Handoff {
                slot: Slot::B,
                trial: true
            }),
            "cut {cut}"
        );
        let d = bootloader(&mut f, &mut dest).unwrap();
        assert_eq!(
            d.handoff(),
            Some(Handoff {
                slot: Slot::A,
                trial: false
            }),
            "cut {cut}"
        );
        assert_eq!(booted_body(&d, &dest).unwrap(), &v1[HEADER_LEN..]);
    }
}
