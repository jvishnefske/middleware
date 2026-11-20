//! The request service over the flash model: `sys/info`, the loader and
//! trust rules (esp-base's, unchanged), `sys/fw/*`, and power loss at every
//! durable store of a claim and a signed upload.

use super::*;
use crate::flash::SectorFlash;
use crate::flash::mem::{Disk, MemSectors};
use crate::layout::SECTORS;
use middleware::loader::DeviceInfo;
use middleware::loader::trust::{TrustInfo, Version};
use mwctl::plan::upload_plan;
use mwctl::sign::{Seed, sign_commit};

/// The golden vector's key (`web/test/fixtures/trust-vector.json`, TEST
/// KEY ONLY: seed 00..1f).
pub fn test_seed() -> Seed {
    let mut b = [0u8; 32];
    for (i, x) in b.iter_mut().enumerate() {
        *x = i as u8;
    }
    Seed::new(b)
}

pub const NODE: SenderId = SenderId([0x9a, 0x51, 0x0c, 0x77, 0x02, 0xe4, 0x18, 0x3b]);

pub fn facts() -> Facts {
    Facts { chip: CHIP, firmware: "0.6.0", node: NODE, board: None, control: None }
}

type Svc<'d> = Service<SectorFlash<MemSectors<'d>>>;

fn open(disk: &mut Disk, cut: Option<usize>) -> (Svc<'_>, Boot) {
    let sectors = MemSectors::new(disk);
    let sectors = match cut {
        Some(n) => sectors.cut_after(n),
        None => sectors,
    };
    Service::open(SectorFlash::open(sectors, SECTORS).expect("flash"), facts())
}

fn call(s: &mut Svc<'_>, key: Key<'_>, body: &[u8]) -> Result<Vec<u8>, HandlerError> {
    let mut out = [0u8; 256];
    match s.handle(key, body, &mut out).reply {
        Reply::Now(r) => r.map(|n| out[..n].to_vec()),
        Reply::Start => Ok(Vec::new()),
    }
}

/// An image of `len` bytes (not WASM: the service only stores it).
fn image(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(7).wrapping_add(seed)).collect()
}

/// Uploads `img`, signed as `version` when given; the commit's reply.
fn upload(s: &mut Svc<'_>, img: &[u8], version: Option<u32>) -> Result<Vec<u8>, HandlerError> {
    let auth = version.and_then(|v| sign_commit(img, &test_seed(), Version(v)).ok());
    let steps = upload_plan(img, None, auth).expect("plan");
    let (commit, transfer) = steps.split_last().expect("steps");
    for step in transfer {
        call(s, step.key, &step.body)?;
    }
    call(s, commit.key, &commit.body)
}

fn installed(s: &mut Svc<'_>) -> Option<Vec<u8>> {
    let store = s.store_mut()?;
    let h = store.header()?;
    let mut buf = vec![0u8; h.len as usize];
    store.load(h, &mut buf).ok()?;
    Some(buf)
}

#[test]
fn pn5_sys_info_names_the_linux_node_and_fw_keys_are_not_found() {
    let mut disk = Disk::default();
    let (mut s, boot) = open(&mut disk, None);
    assert_eq!(boot.installed, Some(Err(StoreError::NoImage)));
    assert_eq!(boot.trust.map(|p| p.trust), Some(TrustState::Unclaimed));
    let info = call(&mut s, keys::INFO, &[]).expect("info");
    let info = DeviceInfo::decode(&info).expect("decode");
    assert_eq!(info.chip, CHIP);
    assert!(CHIP.starts_with("linux-") || CHIP == "host", "{CHIP}");
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        assert_eq!(CHIP, "linux-x86_64");
    }
    if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        assert_eq!(CHIP, "linux-aarch64");
    }
    assert_eq!(info.firmware, "0.6.0");
    assert_eq!(info.node, Some(NODE));
    assert_eq!(info.caps, middleware::loader::Caps::SERVE);
    assert_eq!(info.capacity, 252 * 1024);
    assert_eq!(info.abi_version, middleware::wasm_abi::ABI_VERSION);
    assert_eq!(info.status, PayloadStatus::Empty);
    // A board without firmware OTA: every sys/fw/* key is unknown.
    for k in middleware::fw_ota::keys::ALL {
        assert_eq!(call(&mut s, k, b"x"), Err(HandlerError::NotFound), "{}", k.as_str());
    }
    // Start with nothing installed is refused, as on a board.
    assert_eq!(call(&mut s, keys::START, &[]), Err(HandlerError::InvalidRequest));
}

#[test]
fn pn5_signed_uploads_follow_the_base_firmware_trust_rules_and_survive_restart() {
    let mut disk = Disk::default();
    let old = image(3000, 1);
    let new = image(5000, 2);
    {
        let (mut s, _) = open(&mut disk, None);
        // Unclaimed: trust on first use, unsigned accepted.
        upload(&mut s, &old, None).expect("unsigned upload");
        assert_eq!(installed(&mut s), Some(old.clone()));
        call(&mut s, keys::TRUST_CLAIM, test_seed().public_key().as_bytes()).expect("claim");
        let ti = TrustInfo::decode(&call(&mut s, keys::TRUST_INFO, &[]).expect("info")).expect("decode");
        assert!(matches!(ti.state, TrustState::Claimed { min_version: Version(0), .. }), "{ti:?}");
        // Claimed: unsigned and a second claim are denied.
        assert_eq!(upload(&mut s, &new, None), Err(HandlerError::Denied));
        assert_eq!(call(&mut s, keys::TRUST_CLAIM, Seed::new([9; 32]).public_key().as_bytes()), Err(HandlerError::Denied));
        assert_eq!(installed(&mut s), Some(old.clone()));
        // Signed: accepted, min_version raised; a replay is denied.
        upload(&mut s, &new, Some(100)).expect("signed upload");
        assert_eq!(installed(&mut s), Some(new.clone()));
        assert_eq!(upload(&mut s, &old, Some(100)), Err(HandlerError::Denied));
        assert_eq!(upload(&mut s, &old, Some(99)), Err(HandlerError::Denied));
        assert_eq!(installed(&mut s), Some(new.clone()));
    }
    // Restart: the image, the key and min_version are what the disk holds.
    let (mut s, boot) = open(&mut disk, None);
    assert!(matches!(boot.installed, Some(Ok(h)) if h.len == 5000));
    assert!(matches!(
        boot.trust.map(|p| p.trust),
        Some(TrustState::Claimed { min_version: Version(100), .. })
    ));
    assert_eq!(s.status(), PayloadStatus::Stopped);
    assert_eq!(upload(&mut s, &old, Some(100)), Err(HandlerError::Denied));
    upload(&mut s, &old, Some(101)).expect("newer version");
}

/// Claims and installs a signed image on a node whose flash loses power
/// after `cut` stores; the replies.
fn claim_and_install(s: &mut Svc<'_>, img: &[u8]) -> (Result<Vec<u8>, HandlerError>, Result<Vec<u8>, HandlerError>) {
    let claim = call(s, keys::TRUST_CLAIM, test_seed().public_key().as_bytes());
    let commit = match claim {
        Ok(_) => upload(s, img, Some(7)),
        Err(e) => Err(e),
    };
    (claim, commit)
}

#[test]
fn pn3_power_loss_at_any_store_leaves_the_old_or_the_new_image_and_record() {
    let old = image(2000, 3);
    let new = image(9000, 4);
    // A disk with `old` installed, unclaimed.
    let mut base = Disk::default();
    {
        let (mut s, _) = open(&mut base, None);
        upload(&mut s, &old, None).expect("old");
    }
    // How many stores the whole operation takes without a cut.
    let total = {
        let mut disk = base.clone();
        let (mut s, _) = open(&mut disk, None);
        let (claim, commit) = claim_and_install(&mut s, &new);
        claim.expect("claim");
        commit.expect("commit");
        s.store_mut().map(|st| st.flash().sectors().stores()).expect("store")
    };
    assert!(total > 4, "{total}");
    for cut in 0..total {
        let mut disk = base.clone();
        let (claim_ok, commit_ok) = {
            let (mut s, _) = open(&mut disk, Some(cut));
            let (claim, commit) = claim_and_install(&mut s, &new);
            (claim.is_ok(), commit.is_ok())
        };
        // Reboot on what the disk holds.
        let (mut s, boot) = open(&mut disk, None);
        let img = installed(&mut s).expect("an image survives every cut");
        assert!(img == old || img == new, "cut {cut}: neither image");
        let trust = boot.trust.expect("a readable record").trust;
        match trust {
            TrustState::Unclaimed => assert!(!claim_ok, "cut {cut}: a confirmed claim was lost"),
            TrustState::Claimed { min_version, .. } => {
                assert!(min_version == Version(0) || min_version == Version(7), "cut {cut}: {min_version:?}");
                // The new image never runs with the old min_version.
                if img == new {
                    assert_eq!(min_version, Version(7), "cut {cut}");
                }
            }
        }
        // A confirmed commit is durable.
        if commit_ok {
            assert_eq!(img, new, "cut {cut}: a confirmed install was lost");
        }
    }
}

#[test]
fn pn5_an_unreadable_trust_record_refuses_installs() {
    let mut disk = Disk::default();
    // A poisoned flash: the first store fails, and every read after it.
    let (mut s, _) = open(&mut disk, Some(0));
    assert_eq!(call(&mut s, keys::TRUST_CLAIM, test_seed().public_key().as_bytes()), Err(HandlerError::Internal));
    // Now the flash is poisoned; reopening the service on it reads no
    // trust record, so installs fail closed.
    let flash = s.store_mut().map(|st| st.flash().poisoned());
    assert_eq!(flash, Some(true));
    let mut disk2 = Disk::default();
    let sectors = MemSectors::new(&mut disk2).cut_after(0);
    let mut f = SectorFlash::open(sectors, SECTORS).expect("flash");
    embedded_storage::nor_flash::NorFlash::write(&mut f, 0, &[0; 4]).expect("buffered");
    assert!(f.sync().is_err());
    let (mut s, boot) = Service::open(f, facts());
    assert_eq!(boot.trust, None);
    assert_eq!(call(&mut s, keys::BEGIN, &[0; 8]), Err(HandlerError::Unavailable));
    assert_eq!(call(&mut s, keys::TRUST_CLAIM, &[0; 32]), Err(HandlerError::Unavailable));
    assert!(call(&mut s, keys::INFO, &[]).is_ok());
}

/// `sys/manifest` names the board from the device tree; a node without one
/// sends no board record. `sys/info`'s chip stays the architecture.
#[test]
fn pn14_the_manifest_names_the_board() {
    use middleware::loader::manifest::{Manifest, tag};
    let kd240 = crate::board_from_model(b"ZynqMP KD240 revA/B/1\0").expect("a name");
    assert_eq!(kd240.as_str(), Some("ZynqMP KD240 revA/B/1"));
    assert_eq!(crate::board_from_model(b"\0"), None);
    assert_eq!(crate::board_from_model(b"  Raspberry Pi 4 Model B Rev 1.4\n\0").unwrap().as_str(),
        Some("Raspberry Pi 4 Model B Rev 1.4"));

    let mut disk = Disk::default();
    let sectors = MemSectors::new(&mut disk);
    let (mut s, _) = Service::open(
        SectorFlash::open(sectors, SECTORS).expect("flash"),
        Facts { board: Some(kd240), ..facts() },
    );
    let m = Manifest::decode(&call(&mut s, keys::MANIFEST, &[]).expect("manifest")).expect("decodes");
    assert_eq!(m.board, Some(kd240));
    assert_eq!(m.groups, MANIFEST.groups);
    let raw = call(&mut s, keys::INFO, &[]).expect("info");
    let info = DeviceInfo::decode(&raw).expect("info");
    assert_eq!(info.chip, CHIP);

    let mut disk = Disk::default();
    let (mut s, _) = open(&mut disk, None);
    let raw = call(&mut s, keys::MANIFEST, &[]).expect("manifest");
    assert!(middleware::loader::manifest::records(&raw).unwrap().1.all(|r| r.unwrap().0 != tag::BOARD));
}

const MESH: esp_base::ota::Origin = esp_base::ota::Origin::Air([0x9a, 0x51, 0x0c, 0x77, 0x02, 0xe4]);
const SERIAL: esp_base::ota::Origin = esp_base::ota::Origin::Serial;

fn claimed(disk: &mut Disk) {
    let (mut s, _) = open(disk, None);
    call(&mut s, keys::TRUST_CLAIM, test_seed().public_key().as_bytes()).expect("claim");
}

/// `sys/loader/begin`'s body for `img`.
fn begin_body(img: &[u8]) -> Vec<u8> {
    upload_plan(img, None, None).expect("plan")[0].body.clone()
}

#[test]
fn pn24_an_accepted_control_counter_is_persisted_with_the_trust_record() {
    use middleware::loader::trust::ControlCounter;
    let mut disk = Disk::default();
    claimed(&mut disk);
    {
        let (mut s, _) = open(&mut disk, None);
        assert_eq!(s.state().control(), ControlCounter(0));
        s.accept_control(ControlCounter(10)).expect("accepted");
        assert_eq!(s.state().control(), ControlCounter(10));
        // Not above the last: refused, nothing written.
        assert_eq!(s.accept_control(ControlCounter(10)), Err(StoreError::Rollback));
        assert_eq!(s.accept_control(ControlCounter(3)), Err(StoreError::Rollback));
        assert_eq!(s.state().control(), ControlCounter(10));
    }
    // Durable, and the trust state is unchanged.
    let (s, boot) = open(&mut disk, None);
    let p = boot.trust.expect("record");
    assert_eq!(p.control, ControlCounter(10));
    assert!(matches!(p.trust, TrustState::Claimed { .. }));
    assert_eq!(s.state().control(), ControlCounter(10));
}

#[test]
fn pn24_a_counter_that_cannot_be_saved_is_refused() {
    use middleware::loader::trust::ControlCounter;
    let mut disk = Disk::default();
    claimed(&mut disk);
    {
        let (mut s, _) = open(&mut disk, Some(0));
        assert_eq!(s.accept_control(ControlCounter(10)), Err(StoreError::Flash));
    }
    let (_, boot) = open(&mut disk, None);
    assert_eq!(boot.trust.expect("record").control, ControlCounter(0));
    // Without a trust record there is nothing to persist it in.
    let mut disk2 = Disk::default();
    let mut f = SectorFlash::open(MemSectors::new(&mut disk2).cut_after(0), SECTORS).expect("flash");
    embedded_storage::nor_flash::NorFlash::write(&mut f, 0, &[0; 4]).expect("buffered");
    assert!(f.sync().is_err());
    let (mut s, _) = Service::open(f, facts());
    assert_eq!(s.accept_control(ControlCounter(10)), Err(StoreError::Flash));
}

#[test]
fn pn23_a_mesh_begin_stages_beside_the_running_payload_and_a_serial_one_stops_it() {
    let old = image(2000, 5);
    let new = image(3000, 6);
    let mut disk = Disk::default();
    let (mut s, _) = open(&mut disk, None);
    upload(&mut s, &old, None).expect("old");
    s.started();
    assert_eq!(s.origin(), SERIAL);
    s.set_origin(MESH);
    let mut out = [0u8; 64];
    let h = s.handle(keys::BEGIN, &begin_body(&new), &mut out);
    assert_eq!(h.reply, Reply::Now(Ok(0)));
    assert!(!h.stop, "a staged begin leaves the payload running");
    assert_eq!(s.status(), PayloadStatus::Running);
    assert_eq!(s.loader_owner(), Some(MESH));
    // The same begin over a serial link stops it, as on a board.
    s.set_origin(SERIAL);
    let h = s.handle(keys::BEGIN, &begin_body(&new), &mut out);
    assert!(h.stop);
    assert_eq!(s.status(), PayloadStatus::Loading);
    assert_eq!(s.loader_owner(), Some(SERIAL));
}

#[test]
fn pn25_the_transfer_belongs_to_the_requester_whose_begin_started_it() {
    let img = image(1500, 7);
    let steps = upload_plan(&img, None, None).expect("plan");
    let (commit, transfer) = steps.split_last().expect("steps");
    let mut disk = Disk::default();
    let (mut s, _) = open(&mut disk, None);
    assert_eq!(s.loader_owner(), None);
    s.set_origin(MESH);
    call(&mut s, transfer[0].key, &transfer[0].body).expect("mesh begin");
    assert_eq!(s.loader_owner(), Some(MESH));
    // A serial chunk or commit does not continue the mesh's transfer.
    s.set_origin(SERIAL);
    assert_eq!(call(&mut s, transfer[1].key, &transfer[1].body), Err(HandlerError::InvalidRequest));
    assert_eq!(call(&mut s, commit.key, &commit.body), Err(HandlerError::InvalidRequest));
    assert_eq!(s.loader_owner(), Some(MESH));
    // A serial begin takes it over; then the mesh's chunk is refused.
    for step in transfer {
        call(&mut s, step.key, &step.body).expect("serial transfer");
    }
    assert_eq!(s.loader_owner(), Some(SERIAL));
    s.set_origin(MESH);
    assert_eq!(call(&mut s, transfer[1].key, &transfer[1].body), Err(HandlerError::InvalidRequest));
    // The owner's commit ends it: nobody owns the loader any more.
    s.set_origin(SERIAL);
    call(&mut s, commit.key, &commit.body).expect("commit");
    assert_eq!(s.loader_owner(), None);
    assert_eq!(installed(&mut s), Some(img));
}

#[test]
fn pn23_a_refused_binding_says_so_not_that_a_claim_was_refused() {
    use middleware::loader::binding::SignedBinding;
    use middleware::loader::trust::Signature;
    let mut disk = Disk::default();
    let img = image(1200, 3);
    let (mut s, _) = open(&mut disk, None);
    upload(&mut s, &img, None).expect("install");
    call(&mut s, keys::TRUST_CLAIM, test_seed().public_key().as_bytes()).expect("claim");
    let mut body = mwctl::bindspec::record(mwctl::sign::image_sha256(&img), 3, &[]).expect("record");
    body.extend_from_slice(&SignedBinding { version: Version(3), signature: Signature([9; 64]) }.encode());
    let mut out = [0u8; 64];
    let h = s.handle(keys::BIND, &body, &mut out);
    assert_eq!(h.reply, Reply::Now(Err(HandlerError::Denied)));
    assert!(!h.notes.contains(&Note::ClaimDenied), "{:?}", h.notes);
    assert!(h.notes.contains(&Note::BindDenied), "{:?}", h.notes);
}
