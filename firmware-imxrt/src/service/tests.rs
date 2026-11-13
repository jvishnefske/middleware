//! The request service on the RAM HyperFlash model (RT12): `sys/info`,
//! payload uploads through the loader with whole-block erases, signed
//! payload trust (as RP V4), the fail-closed trust record (as RP V13),
//! and `sys/fw/*` driving a full A/B update to the record the bootloader
//! reads.

extern crate std;

use super::*;
use crate::ab::boot::{Decision, check_slot, decide};
use crate::ab::header::{HEADER_LEN, SlotHeader};
use crate::ab::record;
use crate::ab::test_flash::{BlockFlash, Op};
use crate::layout::{AbMap, BLOCK, RT1050_EVKB};
use ed25519_dalek::{Signer, SigningKey};
use embedded_storage::nor_flash::{ErrorType, NorFlashErrorKind, ReadNorFlash};
use middleware::crc::Crc32;
use middleware::fw_ota::wire::{Command, FwInfo};
use middleware::fw_ota::{CHUNK_LEN, FwUpdate as FwState, Slot, keys as fw_keys};
use middleware::loader::DeviceInfo;
use middleware::loader::sha256::ImageHash;
use middleware::loader::trust::{
    PublicKey, SIGNED_COMMIT_LEN, Signature, SignedCommit, Version, signed_message,
};
use std::vec;
use std::vec::Vec as StdVec;

pub const MAP: FlashMap = RT1050_EVKB;
const AB: AbMap = RT1050_EVKB.ab();

pub const NODE: SenderId = SenderId([0x02, 0x10, 0x50, 0x0B, 0x83, 0x1F, 0x2A, 0x0B]);

pub const FACTS: Facts = Facts {
    chip: "rt1050",
    firmware: "0.1.0",
    runtime: true,
    node: NODE,
    max_image: 64 * 1024,
};

/// The factory boot: slot A, confirmed.
pub const FACTORY: Option<Handoff> = Some(Handoff {
    slot: Slot::A,
    trial: false,
});

/// The whole map's flash, erased (sparse: only written blocks are held).
pub fn flash() -> BlockFlash {
    BlockFlash::erased(MAP.slot_b.end() as usize)
}

type Svc<'f> = Service<&'f mut BlockFlash>;

pub fn open(flash: &mut BlockFlash) -> (Svc<'_>, Boot<NorFlashErrorKind>) {
    Service::open(flash, &MAP, FACTS, FACTORY).unwrap()
}

/// Sends one request; returns the reply with its body copied out.
fn call<F: NorFlash>(
    s: &mut Service<F>,
    key: Key<'_>,
    body: &[u8],
) -> (Result<StdVec<u8>, HandlerError>, Handled) {
    let mut out = [0u8; 128];
    let h = s.handle(key, body, &mut out);
    let r = match h.reply {
        Reply::Now(r) => r.map(|n| out[..n].to_vec()),
        Reply::Start => Ok(StdVec::new()),
    };
    (r, h)
}

fn ok<F: NorFlash>(s: &mut Service<F>, key: Key<'_>, body: &[u8]) {
    let (r, _) = call(s, key, body);
    assert!(r.is_ok(), "{}: {r:?}", key.as_str());
}

fn begin(len: usize, crc: u32) -> [u8; 8] {
    let mut b = [0u8; 8];
    b[..4].copy_from_slice(&(len as u32).to_le_bytes());
    b[4..].copy_from_slice(&crc.to_le_bytes());
    b
}

/// Begin and chunks of `image` (48-byte chunks); commit not sent.
fn transfer<F: NorFlash>(s: &mut Service<F>, image: &[u8]) {
    assert_eq!(
        call(s, keys::BEGIN, &begin(image.len(), Crc32::checksum(image))).0,
        Ok(StdVec::new())
    );
    for (i, part) in image.chunks(48).enumerate() {
        let mut c = (i as u32 * 48).to_le_bytes().to_vec();
        c.extend_from_slice(part);
        assert_eq!(call(s, keys::CHUNK, &c).0, Ok(StdVec::new()));
    }
}

pub fn image(n: usize, seed: u8) -> StdVec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn installed<F: NorFlash>(s: &mut Service<F>) -> Option<StdVec<u8>> {
    let store = s.store_mut();
    let h = store.header()?;
    let mut buf = vec![0u8; h.len as usize];
    store.load(h, &mut buf).ok()?;
    Some(buf)
}

fn info<F: NorFlash>(s: &mut Service<F>) -> (StdVec<u8>, Handled) {
    let (r, h) = call(s, keys::INFO, &[]);
    (r.unwrap(), h)
}

#[test]
fn rt12_info_reports_the_rt1050_chip_firmware_runtime_and_node_id() {
    let mut f = flash();
    let (mut s, boot) = open(&mut f);
    assert_eq!(boot.installed, Err(StoreError::NoImage));
    assert_eq!(boot.trust, Some(Persisted::default()));
    assert_eq!(
        boot.fw,
        Ok(BootInfo {
            running: Slot::A,
            trial: false
        })
    );
    let (body, h) = info(&mut s);
    let i = DeviceInfo::decode(&body).unwrap();
    assert_eq!((i.chip, i.firmware), ("rt1050", "0.1.0"));
    assert_eq!(
        i.capacity,
        64 * 1024,
        "the profile's max_image, under the store's 124 KiB"
    );
    assert_eq!(i.status, PayloadStatus::Empty);
    assert_eq!(i.node, Some(NODE));
    assert_eq!(
        i.caps,
        Caps::SERVE,
        "the runtime serves requests on payload keys"
    );
    assert!(!h.stop && h.notes.is_empty() && h.reset.is_none() && !h.release_watchdog);
    assert_eq!(s.node(), NODE);
    // The store's own cap: a profile allowing more is held to 124 KiB.
    drop(s);
    let big = Facts {
        max_image: 1024 * 1024,
        ..FACTS
    };
    let (mut s, _) = Service::open(&mut f, &MAP, big, FACTORY).unwrap();
    let (body, _) = info(&mut s);
    assert_eq!(
        DeviceInfo::decode(&body).unwrap().capacity,
        PAYLOAD_CAPACITY
    );
    // Without the runtime: no SERVE capability, start is NotFound.
    drop(s);
    let bare = Facts {
        firmware: "0.1.0+no-wasm",
        runtime: false,
        ..FACTS
    };
    let (mut s, _) = Service::open(&mut f, &MAP, bare, FACTORY).unwrap();
    let (body, _) = info(&mut s);
    let i = DeviceInfo::decode(&body).unwrap();
    assert_eq!((i.firmware, i.caps), ("0.1.0+no-wasm", Caps::NONE));
    transfer(&mut s, &image(500, 1));
    ok(&mut s, keys::COMMIT, &[]);
    assert_eq!(
        call(&mut s, keys::START, &[]).0,
        Err(HandlerError::NotFound)
    );
    // `sys/board` has no esp-base profile here.
    assert_eq!(
        call(&mut s, keys::BOARD, &[]).0,
        Err(HandlerError::NotFound)
    );
}

#[test]
fn rt12_open_refuses_a_broken_map() {
    let mut f = flash();
    let bad = FlashMap {
        payload: crate::layout::Region::blocks(5, 1),
        ..MAP
    };
    let r = Service::open(&mut f, &bad, FACTS, FACTORY);
    assert!(matches!(
        r,
        Err(OpenError::Layout(LayoutError::PayloadShape))
    ));
}

#[test]
fn rt12_an_upload_survives_a_reboot_and_the_store_erases_whole_blocks() {
    let img = image(60_000, 7);
    let mut f = flash();
    {
        let (mut s, _) = open(&mut f);
        transfer(&mut s, &img);
        let (r, h) = call(&mut s, keys::COMMIT, &[]);
        assert_eq!(r, Ok(StdVec::new()));
        assert_eq!(h.notes.as_slice(), &[Note::Installed { len: 60_000 }]);
        assert_eq!(s.status(), PayloadStatus::Stopped);
        // A claim, so the trust record is written too.
        ok(&mut s, keys::TRUST_CLAIM, &public(1).0);
    }
    // Reboot: reopen on the same flash.
    let (mut s, boot) = open(&mut f);
    assert!(matches!(boot.installed, Ok(h) if h.len == 60_000));
    assert_eq!(s.status(), PayloadStatus::Stopped);
    assert_eq!(installed(&mut s).as_deref(), Some(&img[..]));
    let (_, h) = call(&mut s, keys::START, &[]);
    assert_eq!(h.reply, Reply::Start);
    s.started();
    assert_eq!(s.status(), PayloadStatus::Running);
    // A second upload goes to the other payload slot; the begin stops the
    // payload, the old image stays until the commit.
    let new = image(3000, 8);
    let (_, h) = call(
        &mut s,
        keys::BEGIN,
        &begin(new.len(), Crc32::checksum(&new)),
    );
    assert!(h.stop);
    drop(s);
    let (mut s, _) = open(&mut f);
    transfer(&mut s, &new);
    let auth = signed(1, &new, 1);
    ok(&mut s, keys::COMMIT, &auth);
    drop(s);
    let (mut s, _) = open(&mut f);
    assert_eq!(installed(&mut s).as_deref(), Some(&new[..]));
    drop(s);
    // Every erase is whole 256 KiB blocks of the trust or payload region.
    let erases: StdVec<_> = f
        .log
        .iter()
        .filter_map(|op| match *op {
            Op::Erase(from, to) => Some((from, to)),
            Op::Write(..) => None,
        })
        .collect();
    assert!(erases.len() >= 4, "{erases:?}");
    for (from, to) in erases {
        assert!(
            from % BLOCK == 0 && to % BLOCK == 0 && to > from,
            "{from:#x}..{to:#x}"
        );
        let inside = |r: crate::layout::Region| from >= r.offset && to <= r.end();
        assert!(
            inside(MAP.trust) || inside(MAP.payload),
            "{from:#x}..{to:#x}"
        );
    }
    // Every write too stays in those regions.
    for op in &f.log {
        if let Op::Write(at, len) = *op {
            let end = at + len as u32;
            let inside = |r: crate::layout::Region| at >= r.offset && end <= r.end();
            assert!(inside(MAP.trust) || inside(MAP.payload), "{at:#x}+{len}");
        }
    }
}

fn signer(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn public(seed: u8) -> PublicKey {
    PublicKey(signer(seed).verifying_key().to_bytes())
}

fn signed(seed: u8, image: &[u8], version: u32) -> [u8; SIGNED_COMMIT_LEN] {
    let m = signed_message(
        image.len() as u32,
        Version(version),
        &ImageHash::digest(image),
    );
    let signature = Signature(signer(seed).sign(&m).to_bytes());
    SignedCommit {
        version: Version(version),
        signature,
    }
    .encode()
}

#[test]
fn rt12_a_pinned_key_and_its_min_version_survive_reboots() {
    let img = image(1500, 3);
    let mut f = flash();
    {
        let (mut s, _) = open(&mut f);
        // Trust on first use: unsigned accepted.
        transfer(&mut s, &img);
        assert_eq!(call(&mut s, keys::COMMIT, &[]).0, Ok(StdVec::new()));
        let (r, h) = call(&mut s, keys::TRUST_CLAIM, &public(1).0);
        assert_eq!(r, Ok(StdVec::new()));
        assert!(matches!(h.notes.as_slice(), [Note::Pinned { .. }]));
    }
    {
        let (mut s, boot) = open(&mut f);
        let pinned = TrustState::Claimed {
            key: public(1),
            min_version: Version(0),
        };
        assert_eq!(boot.trust.map(|p| p.trust), Some(pinned));
        // Unsigned and foreign commits are denied.
        for body in [StdVec::new(), signed(2, &img, 5).to_vec()] {
            transfer(&mut s, &img);
            let (r, h) = call(&mut s, keys::COMMIT, &body);
            assert_eq!(r, Err(HandlerError::Denied));
            assert_eq!(h.notes.as_slice(), &[Note::CommitDenied { min_version: 0 }]);
        }
        transfer(&mut s, &img);
        let (r, h) = call(&mut s, keys::COMMIT, &signed(1, &img, 5));
        assert_eq!(r, Ok(StdVec::new()));
        assert_eq!(
            h.notes.as_slice(),
            &[
                Note::Installed { len: 1500 },
                Note::SignedAccepted { min_version: 5 }
            ]
        );
        // A second claim is refused: no re-key window on this board.
        let (r, h) = call(&mut s, keys::TRUST_CLAIM, &public(2).0);
        assert_eq!(r, Err(HandlerError::Denied));
        assert_eq!(h.notes.as_slice(), &[Note::ClaimDenied]);
    }
    let (mut s, boot) = open(&mut f);
    let raised = TrustState::Claimed {
        key: public(1),
        min_version: Version(5),
    };
    assert_eq!(boot.trust.map(|p| p.trust), Some(raised));
    transfer(&mut s, &img);
    assert_eq!(
        call(&mut s, keys::COMMIT, &signed(1, &img, 5)).0,
        Err(HandlerError::Denied),
        "replay"
    );
}

#[test]
fn rt12_min_version_reaches_flash_before_the_new_header() {
    let old = image(1000, 4);
    let new = image(1200, 5);
    let mut f = flash();
    {
        let (mut s, _) = open(&mut f);
        transfer(&mut s, &old);
        ok(&mut s, keys::COMMIT, &[]);
        ok(&mut s, keys::TRUST_CLAIM, &public(1).0);
        transfer(&mut s, &new);
        // The trust save is ops 0-2 (erase, fields, magic); the slot
        // header's first write, op 3, is cut.
        s.store_mut().flash().raw().cut_at(3);
        let (r, h) = call(&mut s, keys::COMMIT, &signed(1, &new, 9));
        assert_eq!(r, Err(HandlerError::Internal));
        assert!(matches!(h.notes.as_slice(), [Note::Storage(_)]));
        // RAM adopts what flash holds: the raised min_version.
        let raised = TrustState::Claimed {
            key: public(1),
            min_version: Version(9),
        };
        assert_eq!(s.state().trust(), raised);
        s.store_mut().flash().raw().restore();
    }
    let (mut s, boot) = open(&mut f);
    assert_eq!(
        installed(&mut s).as_deref(),
        Some(&old[..]),
        "the old image stays active"
    );
    let raised = TrustState::Claimed {
        key: public(1),
        min_version: Version(9),
    };
    assert_eq!(boot.trust.map(|p| p.trust), Some(raised));
}

/// [`BlockFlash`] whose trust region cannot be read.
struct NoTrust<'a>(&'a mut BlockFlash);

impl ErrorType for NoTrust<'_> {
    type Error = NorFlashErrorKind;
}

impl ReadNorFlash for NoTrust<'_> {
    const READ_SIZE: usize = BlockFlash::READ_SIZE;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        if offset >= MAP.trust.offset && offset < MAP.trust.end() {
            return Err(NorFlashErrorKind::Other);
        }
        self.0.read(offset, bytes)
    }

    fn capacity(&self) -> usize {
        self.0.capacity()
    }
}

impl NorFlash for NoTrust<'_> {
    const WRITE_SIZE: usize = BlockFlash::WRITE_SIZE;
    const ERASE_SIZE: usize = BlockFlash::ERASE_SIZE;

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        self.0.write(offset, bytes)
    }

    fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        self.0.erase(from, to)
    }
}

/// The encoded body of a `sys/fw/*` command.
fn fw_body(cmd: &Command<'_>) -> StdVec<u8> {
    let mut out = [0u8; 600];
    let n = cmd.encode_body(&mut out).unwrap();
    out[..n].to_vec()
}

#[test]
fn rt12_an_unreadable_trust_record_refuses_installs_and_claims() {
    let img = image(600, 14);
    let mut f = flash();
    let erases = f.erases;
    let (mut s, boot) = Service::open(NoTrust(&mut f), &MAP, FACTS, FACTORY).unwrap();
    assert_eq!(boot.trust, None);
    assert_eq!(boot.installed, Err(StoreError::NoImage), "the store opened");
    let mut chunk = 0u32.to_le_bytes().to_vec();
    chunk.extend_from_slice(&img[..48]);
    let claim = public(1).0;
    let fw_image = slot_image(V1_1, 9, 600);
    let fw_begin = fw_body(&Command::Begin {
        len: fw_image.len() as u32,
        version: Version(V1_1),
        sha256: ImageHash::digest(&fw_image),
    });
    let fw_chunk = fw_body(&Command::Chunk {
        offset: 0,
        data: &fw_image[..CHUNK_LEN.min(fw_image.len())],
    });
    for (k, body) in [
        (keys::BEGIN, &begin(img.len(), Crc32::checksum(&img))[..]),
        (keys::CHUNK, &chunk[..]),
        (keys::COMMIT, &[][..]),
        (keys::COMMIT, &signed(1, &img, 5)[..]),
        (keys::TRUST_CLAIM, &claim[..]),
        (fw_keys::BEGIN, &fw_begin[..]),
        (fw_keys::CHUNK, &fw_chunk[..]),
        (fw_keys::COMMIT, &[][..]),
    ] {
        let (r, h) = call(&mut s, k, body);
        assert_eq!(r, Err(HandlerError::Unavailable), "{}", k.as_str());
        assert!(!h.stop && h.notes.is_empty() && h.reset.is_none());
    }
    assert_eq!(s.status(), PayloadStatus::Empty);
    // Reads still work.
    assert!(call(&mut s, keys::INFO, &[]).0.is_ok());
    assert!(call(&mut s, keys::TRUST_INFO, &[]).0.is_ok());
    assert!(call(&mut s, fw_keys::INFO, &[]).0.is_ok());
    s.set_link_ready(true);
    assert!(
        !s.self_test().ok(),
        "a trial is never confirmed without the trust record"
    );
    drop(s);
    assert_eq!(f.erases, erases, "nothing was erased");
    // With the record readable the same requests go through.
    let (mut s, _) = open(&mut f);
    transfer(&mut s, &img);
    assert_eq!(call(&mut s, keys::COMMIT, &[]).0, Ok(StdVec::new()));
}

const V1_0: u32 = 0x0001_0000;
const V1_1: u32 = 0x0001_0001;

/// A slot image (header + `body` bytes) of version `v`.
fn slot_image(v: u32, seed: u8, body: usize) -> StdVec<u8> {
    let body: StdVec<u8> = (0..body)
        .map(|i| (i as u8).wrapping_mul(29) ^ seed)
        .collect();
    let mut out = SlotHeader::for_image(Version(v), &body)
        .unwrap()
        .encode()
        .to_vec();
    out.extend_from_slice(&body);
    out
}

/// The bootloader: load the record, decide, persist a trial's attempt
/// first.
fn bootloader(flash: &mut BlockFlash) -> Decision {
    let mut dest = vec![0u8; AB.slot_size() as usize];
    let stored = record::load(flash, AB.record).unwrap();
    let d = decide(&stored.record, |s| check_slot(flash, &AB, s, &mut dest[..]));
    if let Decision::Boot {
        persist: Some(r), ..
    } = d
    {
        record::save(flash, AB.record, &stored, r).unwrap();
    }
    d
}

fn fw_call<F: NorFlash>(
    s: &mut Service<F>,
    cmd: &Command<'_>,
) -> (Result<StdVec<u8>, HandlerError>, Handled) {
    call(s, cmd.key(), &fw_body(cmd))
}

fn fw_info<F: NorFlash>(s: &mut Service<F>) -> FwInfo {
    FwInfo::decode(&fw_call(s, &Command::Info).0.unwrap()).unwrap()
}

#[test]
fn rt12_sys_fw_keys_go_to_the_updater_only() {
    let img = image(800, 6);
    let mut f = flash();
    f.program(AB.a.offset as usize, &slot_image(V1_0, 1, 2000));
    let (mut s, _) = open(&mut f);
    transfer(&mut s, &img);
    let before = s.state();
    for k in fw_keys::ALL {
        let (_, h) = call(&mut s, k, b"anything");
        assert!(!h.stop && h.reset.is_none());
    }
    let i = fw_info(&mut s);
    assert_eq!(
        (i.running, i.slots[0].version),
        (Slot::A, Some(Version(V1_0)))
    );
    assert_eq!(s.state(), before, "the payload transfer is untouched");
    assert_eq!(call(&mut s, keys::COMMIT, &[]).0, Ok(StdVec::new()));
    assert_eq!(installed(&mut s).as_deref(), Some(&img[..]));
    // An unknown key under the prefix is the updater's too.
    assert_eq!(
        call(&mut s, Key::from_static("sys/fw/nope"), &[]).0,
        Err(HandlerError::NotFound)
    );
    drop(s);
    // Without a bootloader handoff the updater stays down.
    let (mut s, boot) = Service::open(&mut f, &MAP, FACTS, None).unwrap();
    assert_eq!(boot.fw, Err(FwBootError::NoHandoff));
    assert!(!s.fw().is_up());
    for k in fw_keys::ALL {
        assert_eq!(
            call(&mut s, k, &[]).0,
            Err(HandlerError::Unavailable),
            "{}",
            k.as_str()
        );
    }
    assert!(call(&mut s, keys::INFO, &[]).0.is_ok());
}

#[test]
fn rt12_a_full_ab_update_via_requests_reaches_the_bootloaders_record() {
    let v1 = slot_image(V1_0, 1, 2000);
    let v2 = slot_image(V1_1, 2, BLOCK as usize + 1000);
    let payload = image(900, 3);
    let mut f = flash();
    f.program(AB.a.offset as usize, &v1);

    // Factory boot: slot A, not on trial. A payload is installed too.
    let d = bootloader(&mut f);
    let handoff = d.handoff();
    assert_eq!(handoff, FACTORY);
    let (mut s, _) = Service::open(&mut f, &MAP, FACTS, handoff).unwrap();
    transfer(&mut s, &payload);
    ok(&mut s, keys::COMMIT, &[]);

    // The update, as mwctl sends it: begin answers the inactive slot.
    let (r, h) = fw_call(
        &mut s,
        &Command::Begin {
            len: v2.len() as u32,
            version: Version(V1_1),
            sha256: ImageHash::digest(&v2),
        },
    );
    assert_eq!(r, Ok(vec![Slot::B as u8]));
    assert!(h.reset.is_none());
    for (i, data) in v2.chunks(CHUNK_LEN).enumerate() {
        let offset = (i * CHUNK_LEN) as u32;
        assert_eq!(
            fw_call(&mut s, &Command::Chunk { offset, data }).0,
            Ok(StdVec::new())
        );
    }
    assert_eq!(
        fw_call(&mut s, &Command::Commit { signature: None }).0,
        Ok(StdVec::new())
    );
    assert!(matches!(s.fw().state(), Some(FwState::Committed { .. })));
    // Reboot: the reset comes back as a value; nothing was reset.
    let (r, h) = fw_call(&mut s, &Command::Reboot);
    assert_eq!(r, Ok(StdVec::new()));
    assert_eq!(h.reset, Some(ResetRequest::Update(Slot::B)));
    assert_eq!(
        h.notes.as_slice(),
        &[Note::Reset(ResetRequest::Update(Slot::B))]
    );
    assert_eq!(
        s.status(),
        PayloadStatus::Stopped,
        "the payload state is the loader's"
    );
    drop(s);

    // The record the bootloader reads asks for B's trial.
    let rec = record::load(&mut f, AB.record).unwrap().record;
    assert_eq!(
        (rec.trial(), rec.attempts, rec.confirmed),
        (Some(Slot::B), 0, Slot::A)
    );
    let d = bootloader(&mut f);
    assert_eq!(
        d.handoff(),
        Some(Handoff {
            slot: Slot::B,
            trial: true
        })
    );
    assert!(
        matches!(d, Decision::Boot { header, .. } if header.len as usize == v2.len() - HEADER_LEN)
    );

    // The trial firmware: confirm waits for the self-test, then confirms.
    let (mut s, boot) = Service::open(&mut f, &MAP, FACTS, d.handoff()).unwrap();
    assert_eq!(
        boot.fw,
        Ok(BootInfo {
            running: Slot::B,
            trial: true
        })
    );
    assert!(
        matches!(boot.installed, Ok(h) if h.len == 900),
        "the payload survived the update"
    );
    assert_eq!(
        fw_call(&mut s, &Command::Confirm).0,
        Err(HandlerError::Unavailable)
    );
    s.set_link_ready(true);
    let (r, h) = fw_call(&mut s, &Command::Confirm);
    assert_eq!(r, Ok(StdVec::new()));
    assert!(h.release_watchdog && h.reset.is_none());
    assert_eq!(h.notes.as_slice(), &[Note::Confirmed]);
    assert!(s.fw().trial_released());
    drop(s);
    let rec = record::load(&mut f, AB.record).unwrap().record;
    assert_eq!((rec.trial(), rec.confirmed), (None, Slot::B));
    assert_eq!(
        bootloader(&mut f).handoff(),
        Some(Handoff {
            slot: Slot::B,
            trial: false
        })
    );

    // Nothing the payload or trust stores own was touched by the update,
    // and the updater wrote only the record and slot B.
    for op in &f.log {
        let (at, end) = match *op {
            Op::Erase(from, to) => (from, to),
            Op::Write(at, len) => (at, at + len as u32),
        };
        let inside = |r: crate::layout::Region| at >= r.offset && end <= r.end();
        assert!(
            inside(MAP.payload) || inside(AB.record) || inside(AB.b),
            "{op:?}"
        );
    }
}

#[test]
fn rt12_an_unconfirmed_trial_resets_back_to_the_confirmed_slot() {
    let mut f = flash();
    f.program(AB.a.offset as usize, &slot_image(V1_0, 1, 2000));
    let v2 = slot_image(V1_1, 2, 3000);
    let (mut s, _) = open(&mut f);
    ok(
        &mut s,
        fw_keys::BEGIN,
        &fw_body(&Command::Begin {
            len: v2.len() as u32,
            version: Version(V1_1),
            sha256: ImageHash::digest(&v2),
        }),
    );
    for (i, data) in v2.chunks(CHUNK_LEN).enumerate() {
        let offset = (i * CHUNK_LEN) as u32;
        ok(
            &mut s,
            fw_keys::CHUNK,
            &fw_body(&Command::Chunk { offset, data }),
        );
    }
    ok(&mut s, fw_keys::COMMIT, &[]);
    assert_eq!(
        fw_call(&mut s, &Command::Reboot).1.reset,
        Some(ResetRequest::Update(Slot::B))
    );
    drop(s);
    let d = bootloader(&mut f);
    // The trial aborts (or the watchdog resets it): the next boot is A.
    let (mut s, _) = Service::open(&mut f, &MAP, FACTS, d.handoff()).unwrap();
    let (r, h) = fw_call(&mut s, &Command::Abort);
    assert_eq!(r, Ok(StdVec::new()));
    assert_eq!(h.reset, Some(ResetRequest::Normal));
    assert!(!h.release_watchdog);
    drop(s);
    assert_eq!(bootloader(&mut f).handoff(), FACTORY);
}

#[test]
fn rt12_notes_read_as_log_lines() {
    use std::string::ToString;
    assert_eq!(
        Note::Installed { len: 12 }.to_string(),
        "payload installed (12 bytes)"
    );
    assert_eq!(
        Note::Pinned {
            prefix: [0xAB, 1, 2, 3]
        }
        .to_string(),
        "trust: key ab010203... pinned, min_version 0"
    );
    assert!(
        Note::CommitDenied { min_version: 4 }
            .to_string()
            .contains("version > 4")
    );
    assert!(
        Note::Storage(StoreError::Crc)
            .to_string()
            .starts_with("storage: ")
    );
    assert_eq!(
        Note::Reset(ResetRequest::Update(Slot::B)).to_string(),
        "firmware: resetting into slot B on trial"
    );
    assert_eq!(
        Note::Reset(ResetRequest::Normal).to_string(),
        "firmware: resetting"
    );
    assert_eq!(Note::Confirmed.to_string(), "firmware: trial confirmed");
}

#[test]
fn rt12_start_failures_and_traps_update_the_status() {
    let img = image(700, 9);
    let mut f = flash();
    let (mut s, _) = open(&mut f);
    transfer(&mut s, &img);
    ok(&mut s, keys::COMMIT, &[]);
    s.start_failed(StartFailure::Load);
    assert_eq!(s.status(), PayloadStatus::Trapped);
    s.started();
    s.trapped();
    assert_eq!(s.status(), PayloadStatus::Trapped);
    let (_, h) = call(&mut s, keys::STOP, &[]);
    assert!(h.stop);
    assert_eq!(s.status(), PayloadStatus::Stopped);
    s.start_failed(StartFailure::Store);
    assert_eq!(s.status(), PayloadStatus::Stopped);
    assert_eq!(s.capacity(), 64 * 1024);
}

/// [`BlockFlash`] whose `region` cannot be read.
struct Unreadable<'a>(&'a mut BlockFlash, crate::layout::Region);

impl ErrorType for Unreadable<'_> {
    type Error = NorFlashErrorKind;
}

impl ReadNorFlash for Unreadable<'_> {
    const READ_SIZE: usize = BlockFlash::READ_SIZE;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        if offset >= self.1.offset && offset < self.1.end() {
            return Err(NorFlashErrorKind::Other);
        }
        self.0.read(offset, bytes)
    }

    fn capacity(&self) -> usize {
        self.0.capacity()
    }
}

impl NorFlash for Unreadable<'_> {
    const WRITE_SIZE: usize = BlockFlash::WRITE_SIZE;
    const ERASE_SIZE: usize = BlockFlash::ERASE_SIZE;

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        self.0.write(offset, bytes)
    }

    fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        self.0.erase(from, to)
    }
}

const EP: &str = "tcp/192.168.1.10:7447";

#[test]
fn fl7_the_zenoh_endpoint_is_not_found_until_set_then_reads_back_across_reboots() {
    let mut f = flash();
    let (mut s, boot) = open(&mut f);
    assert_eq!(boot.zenoh, Ok(None));
    assert_eq!(s.zenoh_endpoint(), None);
    assert_eq!(
        call(&mut s, ZENOH_ENDPOINT, &[]).0,
        Err(HandlerError::NotFound)
    );
    let (r, h) = call(&mut s, ZENOH_ENDPOINT, EP.as_bytes());
    assert_eq!(r, Ok(StdVec::new()));
    let ep = Endpoint::parse(EP).unwrap();
    assert_eq!(h.notes.as_slice(), [Note::ZenohEndpoint(ep)]);
    assert!(!h.stop && h.reset.is_none());
    assert_eq!(s.zenoh_endpoint(), Some(ep));
    assert_eq!(
        call(&mut s, ZENOH_ENDPOINT, &[]).0,
        Ok(EP.as_bytes().to_vec())
    );
    // The same endpoint again writes nothing.
    let ops = s.store_mut().flash().raw().log.len();
    let (r, h) = call(&mut s, ZENOH_ENDPOINT, EP.as_bytes());
    assert_eq!((r, h.notes.len()), (Ok(StdVec::new()), 0));
    assert_eq!(s.store_mut().flash().raw().log.len(), ops);
    // A reply buffer too small for the string.
    let mut tiny = [0u8; 8];
    assert_eq!(
        s.handle(ZENOH_ENDPOINT, &[], &mut tiny).reply,
        Reply::Now(Err(HandlerError::BufferTooSmall))
    );
    drop(s);
    let (mut s, boot) = open(&mut f);
    assert_eq!(boot.zenoh, Ok(Some(ep)));
    assert_eq!(s.zenoh_endpoint(), Some(ep));
    // A second endpoint replaces it, also across a reboot.
    ok(&mut s, ZENOH_ENDPOINT, b"tcp/10.0.0.2:7448");
    drop(s);
    let (s, _) = open(&mut f);
    assert_eq!(s.zenoh_endpoint().map(|e| e.port), Some(7448));
    // Only the configuration region's blocks were erased.
    assert!(f.log.iter().all(|op| match *op {
        Op::Erase(from, to) =>
            !(from < MAP.config.end() && MAP.config.offset < to)
                || (from >= MAP.config.offset && to <= MAP.config.end()),
        Op::Write(..) => true,
    }));
    let config_erases = f
        .log
        .iter()
        .filter(|op| matches!(op, Op::Erase(from, _) if *from >= MAP.config.offset && *from < MAP.config.end()))
        .count();
    assert_eq!(config_erases, 2);
}

#[test]
fn fl7_a_malformed_endpoint_is_invalid_request_and_changes_nothing() {
    let mut f = flash();
    let (mut s, _) = open(&mut f);
    ok(&mut s, ZENOH_ENDPOINT, EP.as_bytes());
    let before = s.store_mut().flash().raw().log.len();
    let long = [b'1'; 65];
    for body in [
        &b"udp/192.168.1.10:7447"[..],
        b"tcp/router.local:7447",
        b"tcp/192.168.1.10",
        b"tcp/192.168.1.10:0",
        b"tcp/192.168.1.300:7447",
        b"tcp/[::1]:7447",
        &[0xff, 0xfe, 0xfd],
        &long,
    ] {
        let (r, h) = call(&mut s, ZENOH_ENDPOINT, body);
        assert_eq!(r, Err(HandlerError::InvalidRequest), "{body:?}");
        assert!(h.notes.is_empty() && !h.stop);
    }
    assert_eq!(s.store_mut().flash().raw().log.len(), before);
    assert_eq!(s.zenoh_endpoint(), Some(Endpoint::parse(EP).unwrap()));
}

#[test]
fn fl7_a_save_cut_at_any_flash_op_keeps_the_previous_endpoint() {
    let mut base = flash();
    {
        let (mut s, _) = open(&mut base);
        ok(&mut s, ZENOH_ENDPOINT, EP.as_bytes());
    }
    let old = Endpoint::parse(EP).unwrap();
    let new = "tcp/10.1.2.3:9000";
    for cut in 0..4 {
        let mut f = base.clone();
        let (mut s, _) = open(&mut f);
        s.store_mut().flash().raw().cut_at(cut);
        let (r, h) = call(&mut s, ZENOH_ENDPOINT, new.as_bytes());
        s.store_mut().flash().raw().restore();
        let done = r.is_ok();
        assert_eq!(done, cut >= 3, "cut {cut}: an erase and two programs");
        if !done {
            assert_eq!(r, Err(HandlerError::Internal));
            assert_eq!(h.notes.as_slice(), [Note::Storage(StoreError::Flash)]);
            assert_eq!(s.zenoh_endpoint(), Some(old), "RAM keeps the old one");
        }
        drop(s);
        let (mut s, boot) = open(&mut f);
        let want = if done {
            Endpoint::parse(new).unwrap()
        } else {
            old
        };
        assert_eq!(boot.zenoh, Ok(Some(want)), "cut {cut}");
        // The next set after a cut works.
        ok(&mut s, ZENOH_ENDPOINT, b"tcp/10.9.9.9:1");
        drop(s);
        let (s, _) = open(&mut f);
        assert_eq!(s.zenoh_endpoint().map(|e| e.addr), Some([10, 9, 9, 9]));
    }
}

#[test]
fn fl7_an_unreadable_configuration_is_unavailable_and_never_overwritten() {
    let mut f = flash();
    {
        let (mut s, _) = open(&mut f);
        ok(&mut s, ZENOH_ENDPOINT, EP.as_bytes());
    }
    let erases = f.erases;
    let (mut s, boot) =
        Service::open(Unreadable(&mut f, MAP.config), &MAP, FACTS, FACTORY).unwrap();
    assert!(matches!(boot.zenoh, Err(IoError::Flash(_))));
    assert_eq!(s.zenoh_endpoint(), None);
    for body in [&[][..], b"tcp/10.0.0.1:1"] {
        let (r, h) = call(&mut s, ZENOH_ENDPOINT, body);
        assert_eq!(r, Err(HandlerError::Unavailable));
        assert!(h.notes.is_empty());
    }
    // The rest of the service works.
    assert!(call(&mut s, keys::INFO, &[]).0.is_ok());
    drop(s);
    assert_eq!(f.erases, erases, "nothing was erased");
    let (s, _) = open(&mut f);
    assert_eq!(s.zenoh_endpoint(), Some(Endpoint::parse(EP).unwrap()));
}

#[test]
fn fl7_the_endpoint_note_reads_as_a_log_line() {
    use std::string::ToString;
    let n = Note::ZenohEndpoint(Endpoint::parse(EP).unwrap());
    assert_eq!(n.to_string(), "zenoh: endpoint tcp/192.168.1.10:7447 saved");
}
