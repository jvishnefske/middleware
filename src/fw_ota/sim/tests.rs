//! The whole update lifecycle against the bootrom model, through
//! [`FwService`] and the wire encoding, including a power cut at every
//! mutating operation.

use super::*;
use crate::fw_ota::service::{Context, FwService};
use crate::fw_ota::wire::{Command, FwInfo, INFO_LEN, SlotInfo, signed_message};
use crate::fw_ota::{CHUNK_LEN, FwUpdate, StateCode};
use crate::loader::trust::{PublicKey, Signature, TrustState, Version};
use crate::transport::{HandlerError, SenderId};
use ed25519_dalek::{Signer, SigningKey};

const SLOT_SIZE: u32 = 64 * 1024;
const NODE: SenderId = SenderId([0x02, 0xFA, 0x4E, 0, 0, 3, 0, 0]);
const V1_0: u32 = 0x0001_0000;
const V1_1: u32 = 0x0001_0001;
const V1_2: u32 = 0x0001_0002;

fn key() -> SigningKey {
    SigningKey::from_bytes(&[0x11; 32])
}

fn claimed() -> Context {
    let key = PublicKey(key().verifying_key().to_bytes());
    Context {
        node: NODE,
        trust: TrustState::Claimed {
            key,
            min_version: Version(0),
        },
        self_test_ok: true,
    }
}

/// A device with the factory image v1.0 (bought) in slot A, powered on.
fn device() -> (Rp2350Sim, FwService) {
    let mut sim = Rp2350Sim::new(SLOT_SIZE);
    sim.program(Slot::A, &image(20_000, V1_0, false, 1));
    assert_eq!(
        sim.power_on(),
        Some(BootInfo {
            running: Slot::A,
            trial: false
        })
    );
    let svc = FwService::boot(&mut sim).unwrap();
    (sim, svc)
}

fn sha(img: &[u8]) -> [u8; 32] {
    crate::loader::sha256::ImageHash::digest(img)
}

/// One request through the wire encoding.
fn call(
    svc: &mut FwService,
    sim: &mut Rp2350Sim,
    ctx: &Context,
    cmd: Command<'_>,
) -> Result<Vec<u8>, HandlerError> {
    let mut body = [0u8; 4 + CHUNK_LEN];
    let n = cmd.encode_body(&mut body).unwrap();
    let mut out = [0u8; 64];
    let r = svc
        .handle(sim, ctx, cmd.key(), &body[..n], &mut out)
        .expect("a sys/fw key");
    r.map(|n| out[..n].to_vec())
}

fn info(svc: &mut FwService, sim: &mut Rp2350Sim, ctx: &Context) -> FwInfo {
    let b = call(svc, sim, ctx, Command::Info).unwrap();
    assert_eq!(b.len(), INFO_LEN);
    FwInfo::decode(&b).unwrap()
}

fn signature(img: &[u8], version: u32, slot: Slot, node: &SenderId) -> Signature {
    let m = signed_message(img.len() as u32, Version(version), slot, node, &sha(img));
    Signature(key().sign(&m).to_bytes())
}

fn chunks(
    svc: &mut FwService,
    sim: &mut Rp2350Sim,
    ctx: &Context,
    img: &[u8],
) -> Result<(), HandlerError> {
    for (i, c) in img.chunks(CHUNK_LEN).enumerate() {
        call(
            svc,
            sim,
            ctx,
            Command::Chunk {
                offset: (i * CHUNK_LEN) as u32,
                data: c,
            },
        )?;
    }
    Ok(())
}

/// What a host does up to the reboot, stopping at the first error: begin,
/// every chunk, commit (signed), reboot.
fn upload(
    svc: &mut FwService,
    sim: &mut Rp2350Sim,
    ctx: &Context,
    img: &[u8],
    version: u32,
) -> Result<Slot, HandlerError> {
    let slot = call(
        svc,
        sim,
        ctx,
        Command::Begin {
            len: img.len() as u32,
            version: Version(version),
            sha256: sha(img),
        },
    )?;
    let slot = Slot::from_byte(slot[0]).unwrap();
    chunks(svc, sim, ctx, img)?;
    let sig = signature(img, version, slot, &ctx.node);
    call(
        svc,
        sim,
        ctx,
        Command::Commit {
            signature: Some(sig),
        },
    )?;
    call(svc, sim, ctx, Command::Reboot)?;
    Ok(slot)
}

/// After the reboot: the device resets into the trial, boots the updater,
/// and the host confirms.
fn reboot_and_confirm(sim: &mut Rp2350Sim, ctx: &Context) -> Result<FwService, HandlerError> {
    sim.reset().ok_or(HandlerError::Internal)?;
    let mut svc = FwService::boot(sim).map_err(|_| HandlerError::Internal)?;
    call(&mut svc, sim, ctx, Command::Confirm)?;
    Ok(svc)
}

#[test]
fn y7_full_lifecycle_updates_both_slots_in_turn() {
    let ctx = claimed();
    let (mut sim, mut svc) = device();
    let i = info(&mut svc, &mut sim, &ctx);
    assert_eq!(
        (i.state, i.running, i.node),
        (StateCode::Idle, Slot::A, NODE)
    );
    assert_eq!(
        i.slots[0],
        SlotInfo {
            version: Some(Version(V1_0)),
            tbyb: false
        }
    );
    assert_eq!(i.slots[1], SlotInfo::default());

    let new = image(30_000, V1_1, true, 2);
    assert_eq!(upload(&mut svc, &mut sim, &ctx, &new, V1_1), Ok(Slot::B));
    assert_eq!(sim.scheduled(), Some(RebootKind::FlashUpdate(Slot::B)));
    assert_eq!(&sim.slot(Slot::B)[..new.len()], &new[..]);
    let i = info(&mut svc, &mut sim, &ctx);
    assert_eq!(
        (i.state, i.target, i.version, i.min_version),
        (
            StateCode::Committed,
            Some(Slot::B),
            Version(V1_1),
            Version(V1_1)
        )
    );
    assert_eq!(
        i.slots[1],
        SlotInfo {
            version: Some(Version(V1_1)),
            tbyb: true
        }
    );

    // Reset into the trial.
    assert_eq!(
        sim.reset(),
        Some(BootInfo {
            running: Slot::B,
            trial: true
        })
    );
    let mut svc = FwService::boot(&mut sim).unwrap();
    let i = info(&mut svc, &mut sim, &ctx);
    assert_eq!(
        (i.state, i.running, i.version),
        (StateCode::Trial, Slot::B, Version(V1_1))
    );
    call(&mut svc, &mut sim, &ctx, Command::Confirm).unwrap();
    assert_eq!(info(&mut svc, &mut sim, &ctx).state, StateCode::Confirmed);
    assert_eq!(
        sim.image_def(Slot::B).map(|d| d.is_tbyb()),
        Some(false),
        "bought: TBYB cleared"
    );
    assert_eq!(sim.record().pending, None);

    // A later reset boots the bought image normally (higher version).
    assert_eq!(
        sim.reset(),
        Some(BootInfo {
            running: Slot::B,
            trial: false
        })
    );
    let mut svc = FwService::boot(&mut sim).unwrap();
    assert_eq!(info(&mut svc, &mut sim, &ctx).state, StateCode::Idle);

    // The next update goes into A.
    let newer = image(9_000, V1_2, true, 3);
    assert_eq!(upload(&mut svc, &mut sim, &ctx, &newer, V1_2), Ok(Slot::A));
    let svc = reboot_and_confirm(&mut sim, &ctx).unwrap();
    assert_eq!(
        svc.state(),
        FwUpdate::Confirmed {
            slot: Slot::A,
            version: Some(Version(V1_2))
        }
    );
    assert_eq!(
        sim.reset(),
        Some(BootInfo {
            running: Slot::A,
            trial: false
        })
    );
}

#[test]
fn y7_trial_without_confirm_rolls_back_and_its_version_is_used_up() {
    let ctx = claimed();
    let (mut sim, mut svc) = device();
    let new = image(10_000, V1_1, true, 2);
    upload(&mut svc, &mut sim, &ctx, &new, V1_1).unwrap();
    assert_eq!(
        sim.reset(),
        Some(BootInfo {
            running: Slot::B,
            trial: true
        })
    );
    // The self-test has not passed: confirm is refused, retryable.
    let mut svc = FwService::boot(&mut sim).unwrap();
    let untested = Context {
        self_test_ok: false,
        ..ctx
    };
    assert_eq!(
        call(&mut svc, &mut sim, &untested, Command::Confirm),
        Err(HandlerError::Unavailable)
    );
    // The watchdog bites: the unbought image is skipped.
    assert_eq!(
        sim.reset(),
        Some(BootInfo {
            running: Slot::A,
            trial: false
        })
    );
    let mut svc = FwService::boot(&mut sim).unwrap();
    let i = info(&mut svc, &mut sim, &ctx);
    assert_eq!(
        (i.state, i.target, i.version),
        (StateCode::RolledBack, Some(Slot::B), Version(V1_1))
    );
    assert_eq!(sim.record().pending, None, "settled at boot");
    // Replaying the same signed update: refused at begin (version used).
    let r = call(
        &mut svc,
        &mut sim,
        &ctx,
        Command::Begin {
            len: 10_000,
            version: Version(V1_1),
            sha256: sha(&new),
        },
    );
    assert_eq!(r, Err(HandlerError::Denied));
    // A newer image goes through.
    let newer = image(10_000, V1_2, true, 4);
    upload(&mut svc, &mut sim, &ctx, &newer, V1_2).unwrap();
    reboot_and_confirm(&mut sim, &ctx).unwrap();
    assert_eq!(
        sim.reset(),
        Some(BootInfo {
            running: Slot::B,
            trial: false
        })
    );
}

#[test]
fn y7_abort_rejects_a_trial_and_abandons_transfers() {
    let ctx = claimed();
    let (mut sim, mut svc) = device();
    let new = image(10_000, V1_1, true, 2);
    upload(&mut svc, &mut sim, &ctx, &new, V1_1).unwrap();
    sim.reset();
    let mut svc = FwService::boot(&mut sim).unwrap();
    call(&mut svc, &mut sim, &ctx, Command::Abort).unwrap();
    assert_eq!(sim.scheduled(), Some(RebootKind::Normal));
    assert_eq!(
        sim.reset(),
        Some(BootInfo {
            running: Slot::A,
            trial: false
        })
    );
    let mut svc = FwService::boot(&mut sim).unwrap();
    assert_eq!(info(&mut svc, &mut sim, &ctx).state, StateCode::RolledBack);

    // Abort a committed update: its first sector erased, nothing pending.
    let newer = image(10_000, V1_2, true, 3);
    upload(&mut svc, &mut sim, &ctx, &newer, V1_2).unwrap();
    call(&mut svc, &mut sim, &ctx, Command::Abort).unwrap();
    assert_eq!(sim.image_def(Slot::B), None);
    assert_eq!(sim.record().pending, None);
    assert_eq!(
        sim.record().min_version,
        Version(V1_2),
        "the floor stays raised"
    );
    // The flash-update reboot requested before the abort still fires, but
    // finds no image in B and boots A normally.
    assert_eq!(
        sim.reset(),
        Some(BootInfo {
            running: Slot::A,
            trial: false
        })
    );
    let mut svc = FwService::boot(&mut sim).unwrap();
    assert_eq!(info(&mut svc, &mut sim, &ctx).state, StateCode::Idle);

    // Abort mid-transfer.
    let img = image(10_000, 0x0001_0003, true, 5);
    call(
        &mut svc,
        &mut sim,
        &ctx,
        Command::Begin {
            len: 10_000,
            version: Version(0x0001_0003),
            sha256: sha(&img),
        },
    )
    .unwrap();
    chunks(&mut svc, &mut sim, &ctx, &img[..4096]).unwrap();
    call(&mut svc, &mut sim, &ctx, Command::Abort).unwrap();
    assert_eq!(info(&mut svc, &mut sim, &ctx).state, StateCode::Idle);
}

#[test]
fn y7_refusals_leave_the_running_image_alone() {
    let ctx = claimed();
    let (mut sim, mut svc) = device();
    let a_before = sim.slot(Slot::A).to_vec();
    let new = image(10_000, V1_1, true, 2);
    let begin = Command::Begin {
        len: 10_000,
        version: Version(V1_1),
        sha256: sha(&new),
    };
    // Reboot and confirm before anything is committed.
    assert_eq!(
        call(&mut svc, &mut sim, &ctx, Command::Reboot),
        Err(HandlerError::InvalidRequest)
    );
    assert_eq!(
        call(&mut svc, &mut sim, &ctx, Command::Confirm),
        Err(HandlerError::InvalidRequest)
    );
    // Not newer than the running image.
    let old = Command::Begin {
        len: 10_000,
        version: Version(V1_0),
        sha256: sha(&new),
    };
    assert_eq!(
        call(&mut svc, &mut sim, &ctx, old),
        Err(HandlerError::Denied)
    );
    // Too large for the slot.
    let huge = Command::Begin {
        len: SLOT_SIZE + 1,
        version: Version(V1_1),
        sha256: sha(&new),
    };
    assert_eq!(
        call(&mut svc, &mut sim, &ctx, huge),
        Err(HandlerError::BufferTooSmall)
    );
    assert_eq!(call(&mut svc, &mut sim, &ctx, begin).unwrap(), vec![1]);
    chunks(&mut svc, &mut sim, &ctx, &new).unwrap();
    // Unsigned, wrongly signed (other slot, other node): Denied, and the
    // transfer is kept for a good commit.
    assert_eq!(
        call(
            &mut svc,
            &mut sim,
            &ctx,
            Command::Commit { signature: None }
        ),
        Err(HandlerError::Denied)
    );
    let wrong_slot = signature(&new, V1_1, Slot::A, &NODE);
    assert_eq!(
        call(
            &mut svc,
            &mut sim,
            &ctx,
            Command::Commit {
                signature: Some(wrong_slot)
            }
        ),
        Err(HandlerError::Denied)
    );
    let wrong_node = signature(&new, V1_1, Slot::B, &SenderId([9; 8]));
    assert_eq!(
        call(
            &mut svc,
            &mut sim,
            &ctx,
            Command::Commit {
                signature: Some(wrong_node)
            }
        ),
        Err(HandlerError::Denied)
    );
    assert_eq!(
        sim.image_def(Slot::B),
        None,
        "no image in B before an authorized commit"
    );
    let good = signature(&new, V1_1, Slot::B, &NODE);
    call(
        &mut svc,
        &mut sim,
        &ctx,
        Command::Commit {
            signature: Some(good),
        },
    )
    .unwrap();
    assert_eq!(
        sim.slot(Slot::A),
        &a_before[..],
        "the running slot was never touched"
    );
    // Non-fw keys are not ours.
    let mut out = [0u8; 8];
    assert_eq!(
        svc.handle(&mut sim, &ctx, crate::loader::keys::INFO, &[], &mut out),
        None
    );
}

#[test]
fn y7_corrupted_flash_or_a_bad_image_fails_the_commit() {
    let ctx = claimed();
    for case in 0..3 {
        let (mut sim, mut svc) = device();
        let new = match case {
            0 => image(10_000, V1_1, false, 2), // not TBYB
            1 => image(10_000, V1_2, true, 2),  // IMAGE_DEF says 1.2, declared 1.1
            _ => image(10_000, V1_1, true, 2),  // flash corrupted below
        };
        call(
            &mut svc,
            &mut sim,
            &ctx,
            Command::Begin {
                len: 10_000,
                version: Version(V1_1),
                sha256: sha(&new),
            },
        )
        .unwrap();
        chunks(&mut svc, &mut sim, &ctx, &new).unwrap();
        if case == 2 {
            sim.slots[Slot::B as usize][5000] ^= 0xFF;
        }
        let sig = signature(&new, V1_1, Slot::B, &NODE);
        assert_eq!(
            call(
                &mut svc,
                &mut sim,
                &ctx,
                Command::Commit {
                    signature: Some(sig)
                }
            ),
            Err(HandlerError::InvalidRequest),
            "case {case}"
        );
        assert_eq!(
            info(&mut svc, &mut sim, &ctx).state,
            StateCode::Idle,
            "abandoned"
        );
        assert_eq!(sim.image_def(Slot::B), None, "sector 0 never written");
        assert_eq!(sim.record(), FwRecord::EMPTY, "nothing persisted");
    }
}

#[test]
fn y7_resent_requests_are_idempotent() {
    let ctx = Context {
        trust: TrustState::Unclaimed,
        ..claimed()
    };
    let (mut sim, mut svc) = device();
    let new = image(9_000, V1_1, true, 2);
    let begin = Command::Begin {
        len: 9_000,
        version: Version(V1_1),
        sha256: sha(&new),
    };
    call(&mut svc, &mut sim, &ctx, begin).unwrap();
    for (i, c) in new.chunks(CHUNK_LEN).enumerate() {
        let cmd = Command::Chunk {
            offset: (i * CHUNK_LEN) as u32,
            data: c,
        };
        call(&mut svc, &mut sim, &ctx, cmd).unwrap();
        let ops = sim.ops();
        call(&mut svc, &mut sim, &ctx, cmd).unwrap();
        assert_eq!(sim.ops(), ops, "a resent chunk is not written again");
    }
    // A resent begin resumes; nothing is erased.
    let ops = sim.ops();
    assert_eq!(call(&mut svc, &mut sim, &ctx, begin).unwrap(), vec![1]);
    assert_eq!(sim.ops(), ops);
    assert_eq!(info(&mut svc, &mut sim, &ctx).received, 9_000);
    // Unclaimed: an unsigned commit is accepted (TOFU); resent: still Ok.
    call(
        &mut svc,
        &mut sim,
        &ctx,
        Command::Commit { signature: None },
    )
    .unwrap();
    call(
        &mut svc,
        &mut sim,
        &ctx,
        Command::Commit { signature: None },
    )
    .unwrap();
    call(&mut svc, &mut sim, &ctx, Command::Reboot).unwrap();
    let mut svc = reboot_and_confirm(&mut sim, &ctx).unwrap();
    call(&mut svc, &mut sim, &ctx, Command::Confirm).unwrap();
    call(&mut svc, &mut sim, &ctx, Command::Abort).unwrap();
    assert_eq!(sim.scheduled(), None, "abort after confirm does nothing");
}

/// The invariants after any power cut: something boots; what boots is the
/// factory image or a bought new one; no slot holds a bootable
/// (non-TBYB) image other than those; the updater starts.
fn check_after_cut(sim: &mut Rp2350Sim, k: u64, new: &[u8]) -> FwService {
    let boot = sim.power_on().unwrap_or_else(|| panic!("cut {k}: bricked"));
    assert!(!boot.trial, "cut {k}: a power-on is a normal boot");
    for s in [Slot::A, Slot::B] {
        if let Some(d) = sim.image_def(s).filter(|d| !d.is_tbyb()) {
            let v = d.version_u32();
            assert!(
                v == V1_0 || v == V1_1,
                "cut {k}: slot {s:?} bootable as {v:#x}"
            );
            if v == V1_1 {
                // Bought: the slot holds the new image with TBYB cleared.
                let mut bought = new.to_vec();
                bought[d.type_offset as usize + 3] &= 0x7f;
                assert_eq!(&sim.slot(s)[..new.len()], &bought[..], "cut {k}");
            }
        }
    }
    FwService::boot(sim).unwrap_or_else(|e| panic!("cut {k}: boot {e:?}"))
}

fn new_len() -> usize {
    3 * SECTOR as usize + 700
}

/// The host finishes the job from whatever state it finds, as `mwctl fw
/// update` followed by `fw confirm` would on a reconnect.
fn finish(mut svc: FwService, sim: &mut Rp2350Sim, ctx: &Context, k: u64) {
    // Bounded: each pass completes or moves the state forward.
    for _ in 0..4 {
        let i = info(&mut svc, sim, ctx);
        let running = sim.image_def(i.running).map(|d| d.version_u32());
        match i.state {
            StateCode::Committed => {
                call(&mut svc, sim, ctx, Command::Reboot).unwrap();
                svc = reboot_and_confirm(sim, ctx).unwrap();
            }
            StateCode::Trial => call(&mut svc, sim, ctx, Command::Confirm)
                .map(drop)
                .unwrap(),
            StateCode::Confirmed => break,
            _ if running.is_some_and(|v| v > V1_0) => break,
            _ => {
                let version = if i.min_version.0 >= V1_1 { V1_2 } else { V1_1 };
                let img = image(new_len(), version, true, 9);
                upload(&mut svc, sim, ctx, &img, version)
                    .unwrap_or_else(|e| panic!("cut {k}: retry {e:?}"));
                svc = reboot_and_confirm(sim, ctx).unwrap();
            }
        }
    }
    let boot = sim.reset().unwrap();
    let v = sim.image_def(boot.running).unwrap().version_u32();
    assert!(v >= V1_1 && !boot.trial, "cut {k}: ended on {v:#x}");
}

/// Power is cut at every mutating operation of a complete update (begin
/// through confirm, including the bootrom's buy and the boot-time
/// reconciliation), the device is powered on again, and the host then
/// finishes the update: the device never bricks, never boots an image that
/// was not authorized, and always completes.
#[test]
fn y8_power_loss_at_every_operation_is_recoverable() {
    let ctx = claimed();
    let new = image(new_len(), V1_1, true, 2);
    // Count the operations of an uninterrupted run.
    let (mut sim, mut svc) = device();
    let base = sim.ops();
    upload(&mut svc, &mut sim, &ctx, &new, V1_1).unwrap();
    reboot_and_confirm(&mut sim, &ctx).unwrap();
    let total = sim.ops() - base;
    assert!(total > 10, "{total}");
    for k in 0..total {
        let (mut sim, mut svc) = device();
        sim.cut_power_at(sim.ops() + k);
        let r = upload(&mut svc, &mut sim, &ctx, &new, V1_1)
            .and_then(|_| reboot_and_confirm(&mut sim, &ctx).map(drop));
        // Only the last operation, saving the settled record after the
        // buy, is answered as a success (the image is bought; the next boot
        // settles the record).
        assert!(
            r.is_err() || k + 1 == total,
            "cut {k}: the run should have been interrupted"
        );
        assert!(!sim.powered(), "cut {k}");
        let svc = check_after_cut(&mut sim, k, &new);
        finish(svc, &mut sim, &ctx, k);
    }
}

/// A power cut during the boot-time reconciliation itself (settling a
/// rolled-back update) leaves a state the next boot settles again.
#[test]
fn y8_power_loss_while_settling_at_boot() {
    let ctx = claimed();
    let (mut sim, mut svc) = device();
    let new = image(10_000, V1_1, true, 2);
    upload(&mut svc, &mut sim, &ctx, &new, V1_1).unwrap();
    sim.reset(); // trial
    sim.cut_power_at(sim.ops()); // the watchdog reboot's settle
    sim.reset(); // watchdog: back to A
    assert!(FwService::boot(&mut sim).is_err());
    assert_eq!(
        sim.power_on(),
        Some(BootInfo {
            running: Slot::A,
            trial: false
        })
    );
    let mut svc = FwService::boot(&mut sim).unwrap();
    assert_eq!(info(&mut svc, &mut sim, &ctx).state, StateCode::RolledBack);
}
