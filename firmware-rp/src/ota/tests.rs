//! Host tests of the RP2350 updater: the platform on the flash model, the
//! factory image, and whole updates driven by `mwctl` through the request
//! service, on a model of the boot ROM's A/B choice.
//!
//! The boot ROM model ([`rom_boot`]) is the behaviour design.md Y14 lists
//! to verify on silicon: without the partition table nothing is
//! translated; a flash-update reboot boots the valid image of its target
//! partition (TBYB or not); otherwise the bought image with the higher
//! version boots and TBYB images are skipped.

extern crate std;

use std::collections::VecDeque;
use std::io;
use std::time::Duration;
use std::vec;
use std::vec::Vec;

use middleware::Key;
use middleware::fw_ota::picobin::{ImageDef, find_image_def_in};
use middleware::fw_ota::record::{FwRecord, Pending};
use middleware::fw_ota::{FwPlatform, Slot, StateCode, keys as fw_keys};
use middleware::loader::trust::Version;
use middleware::serial::frame::{self, Frame, MAX_RAW_FRAME, MAX_WIRE_FRAME, Split, Splitter};
use middleware::transport::HandlerError;
use middleware::SenderId;
use mwctl::device::Device;
use mwctl::fw::{UpdateOptions, Updated, load_image};
use mwctl::link::{Link, Timeouts, Transport};
use mwctl::sign::Seed;

use super::AbUpdate;
use super::factory::{FactoryError, check_update, clear_tbyb, factory_flash, factory_len};
use super::image::{FwVersion, SENTINEL_OFFSET, START_BLOCK_OFFSET, UPDATE_FLAGS, sentinel_block, start_block};
use super::platform::{PlatformError, RpPlatform};
use super::rom::{Reboot, Rom, XIP_BASE, XipMap};
use super::table::partition_table_bytes;
use crate::layout::{AbMap, RP2350, Region};
use crate::ram_flash::VecFlash;
use crate::service::{Facts, Reply, Service};

const NODE: SenderId = SenderId([0x05, 0x6E, 0x9F, 0x22, 0x90, 0x64, 0x46, 0x3F]);
const FACTS: Facts = Facts { chip: "rp2350", firmware: "1.0", runtime: true, node: NODE, max_image: 64 * 1024 };
const FLASH: usize = RP2350.flash_size as usize;
const V1_0: FwVersion = FwVersion { major: 1, minor: 0 };
const V1_1: FwVersion = FwVersion { major: 1, minor: 1 };
const V1_2: FwVersion = FwVersion { major: 1, minor: 2 };
const V1_3: FwVersion = FwVersion { major: 1, minor: 3 };

fn ab() -> AbMap {
    RP2350.ab().unwrap()
}

fn put(buf: &mut [u8], at: u32, words: &[u32]) {
    for (i, w) in words.iter().enumerate() {
        let o = at as usize + 4 * i;
        buf[o..o + 4].copy_from_slice(&w.to_le_bytes());
    }
}

/// An update image laid out as the firmware's (vector table, IMAGE_DEF
/// at 0x200, sentinel at 0xF00, code from 0x1000), `len` bytes.
fn image(v: FwVersion, len: usize, seed: u8) -> Vec<u8> {
    let mut img = vec![0xFFu8; len];
    for (i, b) in img.iter_mut().enumerate().take(0x110) {
        *b = (i as u8).wrapping_add(seed);
    }
    put(&mut img, START_BLOCK_OFFSET, &start_block(UPDATE_FLAGS, v));
    put(&mut img, SENTINEL_OFFSET, &sentinel_block());
    for (i, b) in img.iter_mut().enumerate().skip(0x1000) {
        *b = (i as u8).wrapping_mul(31).wrapping_add(seed);
    }
    img
}

/// A new Pico 2 programmed with the factory flash for `v`.
fn factory(v: FwVersion) -> Vec<u8> {
    let img = image(v, 40_000, 1);
    let mut mem = vec![0xFFu8; FLASH];
    let n = factory_flash(&img, &RP2350, &mut mem).unwrap();
    assert_eq!(n, factory_len(&RP2350, img.len()).unwrap());
    mem
}

fn def(mem: &[u8], r: Region) -> Option<ImageDef> {
    find_image_def_in(&mem[r.offset as usize..r.end() as usize])
}

/// The boot ROM's choice for a boot after `reboot` (see the module docs).
fn rom_boot(mem: &[u8], reboot: Option<Reboot>) -> XipMap {
    let ab = ab();
    let table = partition_table_bytes(&ab);
    if mem[..table.len()] != table {
        return XipMap { base: 0, size: 0x400 * 4096 };
    }
    let target = match reboot {
        Some(Reboot::FlashUpdate { start }) => Some(start - XIP_BASE),
        _ => None,
    };
    let valid = |r: Region| def(mem, r).filter(|d| d.is_exe() && d.is_rp2350());
    let mut best: Option<(u32, Region)> = None;
    for r in [ab.a, ab.b] {
        let Some(d) = valid(r) else { continue };
        if target == Some(r.offset) {
            best = Some((u32::MAX, r));
            break;
        }
        if d.is_tbyb() {
            continue;
        }
        if best.is_none_or(|(v, _)| d.version_u32() > v) {
            best = Some((d.version_u32(), r));
        }
    }
    let r = best.map(|(_, r)| r).expect("a bootable image");
    XipMap { base: r.offset, size: r.size }
}

/// The ROM and chip: records reboots, the watchdog and masked sections.
#[derive(Debug, Default)]
struct TestRom {
    xip: Option<XipMap>,
    reboots: Vec<(Reboot, u32)>,
    watchdog_stopped: bool,
    masked: usize,
}

impl Rom for TestRom {
    type Error = ();

    fn xip_map(&mut self) -> XipMap {
        self.xip.unwrap_or(XipMap { base: 0, size: 0 })
    }

    fn reboot(&mut self, reboot: Reboot, delay_ms: u32) -> Result<(), ()> {
        self.reboots.push((reboot, delay_ms));
        Ok(())
    }

    fn stop_watchdog(&mut self) {
        self.watchdog_stopped = true;
    }

    fn uninterrupted<T>(&mut self, f: impl FnOnce() -> T) -> T {
        self.masked += 1;
        f()
    }
}

type Svc = Service<VecFlash, AbUpdate<TestRom>>;

/// A Pico 2 on the bench: the request service over the flash model, its
/// USB byte stream, and resets through the boot ROM model.
struct Board {
    svc: Svc,
    usb: bool,
    splitter: Splitter<MAX_WIRE_FRAME>,
    tx: VecDeque<u8>,
    boots: usize,
}

fn start(mem: Vec<u8>, reboot: Option<Reboot>, usb: bool) -> Svc {
    let xip = rom_boot(&mem, reboot);
    let mut flash = VecFlash::new(mem);
    let rom = TestRom { xip: Some(xip), ..TestRom::default() };
    let (fw, _) = AbUpdate::boot(&mut flash, rom, ab());
    let (mut svc, _) = Service::open(flash, &RP2350, FACTS, fw);
    svc.set_usb_configured(usb);
    svc
}

impl Board {
    fn power_on(mem: Vec<u8>) -> Board {
        Board { svc: start(mem, None, true), usb: true, splitter: Splitter::new(), tx: VecDeque::new(), boots: 1 }
    }

    fn mem(&mut self) -> &mut Vec<u8> {
        &mut self.svc.store_mut().unwrap().flash().mem
    }

    /// Resets the chip: the ROM boots again (after `reboot`, or a power
    /// cycle / watchdog when `None`).
    fn reset(&mut self, reboot: Option<Reboot>) {
        let mem = core::mem::take(self.mem());
        self.svc = start(mem, reboot, self.usb);
        self.boots += 1;
    }

    fn fw(&self) -> &AbUpdate<TestRom> {
        self.svc.fw()
    }

    fn running(&self) -> Slot {
        self.fw().boot_info().unwrap().running
    }

    fn emit(&mut self, f: &Frame<'_>) {
        let mut scratch = [0u8; MAX_RAW_FRAME];
        let mut out = [0u8; MAX_WIRE_FRAME];
        let n = frame::encode(f, &mut scratch, &mut out).expect("encodable");
        self.tx.extend(&out[..n]);
    }

    fn on_frame(&mut self, run: &mut [u8]) {
        let Ok(Frame::Request { id, key, payload }) = frame::decode_wire(run) else {
            return;
        };
        let mut out = [0u8; 128];
        let h = self.svc.handle(key, payload, &mut out);
        let result = match h.reply {
            Reply::Now(r) => r.map(|n| out[..n].to_vec()),
            Reply::Start => Ok(Vec::new()),
        };
        let body = result.as_deref().map_err(|e| *e);
        self.emit(&Frame::Response { id, result: body });
        // A requested reboot happens once the reply is out.
        if let Some(&(r, _)) = self.fw().rom().reboots.last() {
            self.reset(Some(r));
        }
    }
}

impl Transport for Board {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        for &b in bytes {
            if let Split::Frame(_) = self.splitter.push(b) {
                let mut run = self.splitter.frame().to_vec();
                self.on_frame(&mut run);
            }
        }
        Ok(())
    }

    fn read(&mut self, buf: &mut [u8], _timeout: Duration) -> io::Result<usize> {
        let n = buf.len().min(self.tx.len());
        for (dst, src) in buf.iter_mut().zip(self.tx.drain(..n)) {
            *dst = src;
        }
        Ok(n)
    }
}

fn device(board: Board) -> Device<Link<Board>> {
    let t = Timeouts { request: Duration::from_millis(50), begin: Duration::from_millis(50), retries: 1 };
    Device::new(Link::new(board, t))
}

fn board(d: &mut Device<Link<Board>>) -> &mut Board {
    d.link_mut().transport_mut()
}

fn fw_image(v: FwVersion, seed: u8) -> mwctl::fw::FwImage {
    load_image(&image(v, 30_000 + usize::from(seed) * 1000, seed), None).unwrap()
}

#[test]
fn y12_slots_are_bounded_and_the_running_one_is_never_written() {
    let mut flash = VecFlash::new(factory(V1_0));
    let mut rom = TestRom { xip: Some(XipMap { base: ab().a.offset, size: ab().a.size }), ..TestRom::default() };
    let mut p = RpPlatform::new(&mut flash, &mut rom, ab(), None);
    let boot = p.boot_info().unwrap();
    assert_eq!((boot.running, boot.trial), (Slot::A, false), "the factory image is bought");
    assert_eq!(p.slot_size(), 0x17_E000);
    assert_eq!(p.erase(Slot::A, 0), Err(PlatformError::Running));
    assert_eq!(p.write(Slot::A, 0, &[0; 4]), Err(PlatformError::Running));
    let mut b = [0u8; 4];
    p.read(Slot::A, START_BLOCK_OFFSET, &mut b).unwrap();
    assert_eq!(u32::from_le_bytes(b), 0xffff_ded3, "the running slot reads at its partition");
    // Slot B: bounded, a short tail padded, odd reads bounced.
    let end = p.slot_size();
    assert_eq!(p.erase(Slot::B, end / 4096), Err(PlatformError::Bounds));
    assert_eq!(p.read(Slot::B, end - 2, &mut b), Err(PlatformError::Bounds));
    assert_eq!(p.write(Slot::B, end, &[1]), Err(PlatformError::Bounds));
    p.erase(Slot::B, 1).unwrap();
    p.write(Slot::B, 4096, &[1, 2, 3, 4, 5, 6, 7]).unwrap();
    let mut odd = [0u8; 5];
    p.read(Slot::B, 4097, &mut odd).unwrap();
    assert_eq!(odd, [2, 3, 4, 5, 6]);
    let mut long = vec![0u8; 700];
    p.read(Slot::B, 4095, &mut long).unwrap();
    assert_eq!(&long[..9], &[0xFF, 1, 2, 3, 4, 5, 6, 7, 0xFF]);
    let mut tail = [0u8; 2];
    p.read(Slot::B, 4103, &mut tail).unwrap();
    assert_eq!(tail, [0xFF, 0xFF], "the tail was padded with 0xFF");
    // Slot B is at its partition offset in flash.
    assert_eq!(&flash.mem[0x18_1000..0x18_1007], &[1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(rom.reboots, Vec::new());
}

#[test]
fn y12_the_record_alternates_sectors_and_survives_power_cuts() {
    let rec = |n: u32| FwRecord {
        min_version: Version(n),
        pending: Some(Pending { slot: Slot::B, version: Version(n), len: n * 100, sha256: [n as u8; 32], rebooted: n.is_multiple_of(2) }),
    };
    let record = ab().record;
    let mut flash = VecFlash::erased(FLASH);
    let mut rom = TestRom::default();
    {
        let mut p = RpPlatform::new(&mut flash, &mut rom, ab(), Some(Slot::A));
        assert_eq!(p.load_record().unwrap(), FwRecord::EMPTY, "a new board has none");
        for n in 1..=5 {
            p.persist(&rec(n)).unwrap();
            assert_eq!(p.load_record().unwrap(), rec(n));
        }
    }
    // Save k went to sector (k - 1) % 2: save 5 in sector 0, save 4 in 1.
    let seq = |mem: &[u8], s: u32| FwRecord::decode(&mem[(record.offset + s * 4096) as usize..]);
    assert_eq!(seq(&flash.mem, 0), Some((5, rec(5))));
    assert_eq!(seq(&flash.mem, 1), Some((4, rec(4))));
    // A cut at each of the three operations of a save (erase, fields,
    // magic): the previous record or the new one, never anything else.
    for op in 0..3 {
        let before = flash.mem.clone();
        flash.cut_at(op);
        let r = RpPlatform::new(&mut flash, &mut rom, ab(), Some(Slot::A)).persist(&rec(6));
        assert!(r.is_err(), "op {op}");
        flash.restore();
        let got = RpPlatform::new(&mut flash, &mut rom, ab(), Some(Slot::A)).load_record().unwrap();
        assert_eq!(got, rec(5), "op {op}: the torn save does not count");
        flash.mem = before;
    }
    // Nothing outside the record sectors was written.
    assert!(flash.mem[..record.offset as usize].iter().all(|&b| b == 0xFF));
    assert!(flash.mem[record.end() as usize..].iter().all(|&b| b == 0xFF));
}

/// A board on its first trial of `v` in slot B (factory 1.0 in A).
fn trial_in_b(v: FwVersion) -> (VecFlash, TestRom) {
    let mut mem = factory(V1_0);
    let img = image(v, 25_000, 7);
    let b = ab().b.offset as usize;
    mem[b..b + img.len()].copy_from_slice(&img);
    let xip = rom_boot(&mem, Some(Reboot::FlashUpdate { start: XIP_BASE + ab().b.offset }));
    assert_eq!(xip.base, ab().b.offset);
    (VecFlash::new(mem), TestRom { xip: Some(xip), ..TestRom::default() })
}

#[test]
fn y12_explicit_buy_clears_tbyb_in_place_masked_and_stops_the_watchdog() {
    let (mut flash, mut rom) = trial_in_b(V1_1);
    let before = flash.mem.clone();
    {
        let mut p = RpPlatform::new(&mut flash, &mut rom, ab(), None);
        assert_eq!(p.explicit_buy(), Err(PlatformError::NoBoot), "boot_info first");
        let boot = p.boot_info().unwrap();
        assert_eq!((boot.running, boot.trial), (Slot::B, true), "a running TBYB image is a trial");
        p.explicit_buy().unwrap();
        assert!(!p.boot_info().unwrap().trial);
    }
    assert_eq!((rom.masked, rom.watchdog_stopped), (1, true));
    // Only the IMAGE_TYPE flags changed: the TBYB bit, in slot B's first sector.
    let diff: Vec<usize> = (0..FLASH).filter(|&i| flash.mem[i] != before[i]).collect();
    assert_eq!(diff, vec![ab().b.offset as usize + START_BLOCK_OFFSET as usize + 7]);
    assert_eq!(def(&flash.mem, ab().b).unwrap().flags, 0x1021);
    // Bought: a normal boot now picks B (1.1 > 1.0).
    assert_eq!(rom_boot(&flash.mem, None).base, ab().b.offset);
    // Buying again only stops the watchdog.
    let erases = flash.erase_calls;
    rom.watchdog_stopped = false;
    let mut p = RpPlatform::new(&mut flash, &mut rom, ab(), Some(Slot::B));
    p.explicit_buy().unwrap();
    assert_eq!((flash.erase_calls, rom.watchdog_stopped, rom.masked), (erases, true, 1));
}

#[test]
fn y12_a_torn_buy_leaves_slot_b_unbootable_and_a_boots() {
    for op in 0..2 {
        let (mut flash, mut rom) = trial_in_b(V1_1);
        let mut p = RpPlatform::new(&mut flash, &mut rom, ab(), None);
        p.boot_info().unwrap();
        flash.cut_at(op);
        let r = RpPlatform::new(&mut flash, &mut rom, ab(), Some(Slot::B)).explicit_buy();
        assert!(matches!(r, Err(PlatformError::Flash(_))), "op {op}: {r:?}");
        assert!(!rom.watchdog_stopped, "op {op}: a failed buy leaves the trial watchdog running");
        flash.restore();
        assert_eq!(def(&flash.mem, ab().b), None, "op {op}: the sentinel keeps a torn sector unbootable");
        assert_eq!(rom_boot(&flash.mem, None).base, ab().a.offset, "op {op}");
    }
}

#[test]
fn y12_without_the_partition_table_the_updater_is_unavailable() {
    // An image at flash offset 0 with no table: nothing is translated.
    let mut mem = vec![0xFFu8; FLASH];
    let img = image(V1_0, 20_000, 3);
    mem[..img.len()].copy_from_slice(&img);
    let mut flash = VecFlash::new(mem);
    let rom = TestRom { xip: Some(rom_boot(&flash.mem, None)), ..TestRom::default() };
    let (fw, boot) = AbUpdate::boot(&mut flash, rom, ab());
    assert_eq!(boot.map(|_| ()), Err(PlatformError::NotPartitioned { base: 0 }));
    assert_eq!(fw.state(), None);
    let (mut svc, _) = Service::open(flash, &RP2350, FACTS, fw);
    let mut out = [0u8; 64];
    for k in fw_keys::ALL {
        let h = svc.handle(k, &[], &mut out);
        assert_eq!(h.reply, Reply::Now(Err(HandlerError::Unavailable)), "{}", k.as_str());
    }
    // The payload side works as before.
    let h = svc.handle(middleware::loader::keys::INFO, &[], &mut out);
    assert!(matches!(h.reply, Reply::Now(Ok(_))));
}

#[test]
fn y13_factory_flash_is_the_table_and_the_update_image_bought_in_slot_a() {
    let img = image(V1_0, 40_000, 1);
    let def_update = check_update(&img, ab().a.size).unwrap();
    let mem = factory(V1_0);
    let a = ab().a.offset as usize;
    // Table at 0, erased up to slot A.
    let table = partition_table_bytes(&ab());
    assert_eq!(&mem[..table.len()], &table);
    assert!(mem[table.len()..a].iter().all(|&b| b == 0xFF));
    // Slot A: the update image with exactly the TBYB bit cleared.
    let slot = &mem[a..a + img.len()];
    let diff: Vec<usize> = (0..img.len()).filter(|&i| slot[i] != img[i]).collect();
    assert_eq!(diff, vec![def_update.type_offset as usize + 3]);
    let d = find_image_def_in(slot).unwrap();
    assert!(!d.is_tbyb() && d.is_exe() && d.is_rp2350());
    assert_eq!((d.version_u32(), d.window_tail), (0x0001_0000, SENTINEL_OFFSET));
    // Slot B and everything after the image are erased.
    assert!(mem[a + img.len()..].iter().all(|&b| b == 0xFF));
    assert_eq!(rom_boot(&mem, None).base, ab().a.offset);
    // mwctl accepts the update image and refuses the factory one (bought).
    let loaded = load_image(&img, Some(Version(0x0001_0000))).unwrap();
    assert_eq!(loaded.bytes, img);
    assert!(load_image(slot, None).is_err());
}

#[test]
fn y13_images_mwctl_refuses_are_refused_here_too() {
    let good = image(V1_1, 9_000, 2);
    assert!(check_update(&good, ab().a.size).is_ok());
    let mut not_tbyb = good.clone();
    let d = find_image_def_in(&good).unwrap();
    clear_tbyb(&mut not_tbyb, &d).unwrap();
    assert_eq!(check_update(&not_tbyb, ab().a.size), Err(FactoryError::NotTbyb));
    let mut no_sentinel = good.clone();
    no_sentinel[SENTINEL_OFFSET as usize..0x1000].fill(0xFF);
    put(&mut no_sentinel, START_BLOCK_OFFSET + 20, &[0]);
    assert_eq!(check_update(&no_sentinel, ab().a.size), Err(FactoryError::NoSentinel));
    let mut rp2040 = good.clone();
    put(&mut rp2040, START_BLOCK_OFFSET + 4, &[0x42 | 1 << 8 | 0x8021 << 16]);
    assert_eq!(check_update(&rp2040, ab().a.size), Err(FactoryError::NotRp2350Exe));
    assert_eq!(check_update(&[0xFF; 8192], ab().a.size), Err(FactoryError::NoImageDef));
    assert_eq!(check_update(&good, 8_000), Err(FactoryError::TooLarge { len: 9_000, slot: 8_000 }));
    for bad in [&not_tbyb, &no_sentinel, &rp2040] {
        assert!(load_image(bad, None).is_err(), "mwctl agrees");
    }
    let mut out = vec![0u8; 100];
    assert_eq!(factory_flash(&good, &RP2350, &mut out), Err(FactoryError::Layout));
    assert_eq!(factory_flash(&good, &crate::layout::RP2040, &mut out), Err(FactoryError::Layout));
    // clear_tbyb refuses an IMAGE_DEF that does not match the bytes.
    let mut short = good[..0x100].to_vec();
    assert_eq!(clear_tbyb(&mut short, &d), Err(FactoryError::TypeItem));
}

#[test]
fn y12_mwctl_updates_confirms_and_rolls_back_a_pico_2() {
    let seed = Seed::new([42; 32]);
    let mut d = device(Board::power_on(factory(V1_0)));
    // (1) Factory: slot A, 1.0, idle.
    let i = d.fw_info().unwrap();
    assert_eq!((i.state, i.running, i.slots[0].version, i.slots[1].version), (StateCode::Idle, Slot::A, Some(Version(0x0001_0000)), None));
    assert!(i.self_test_ok);
    d.trust_claim(seed.public_key()).unwrap();
    // (2) Update to 1.1, signed: into B, flash-update reboot, trial.
    let img = fw_image(V1_1, 1);
    let opts = UpdateOptions { key: Some(&seed), reboot: true };
    let done = d.fw_update(&img, opts, |_| {}).unwrap();
    assert_eq!(done, Updated { slot: Slot::B, rebooted: true, reboot_unanswered: false });
    assert_eq!(board(&mut d).boots, 2);
    let i = d.fw_info().unwrap();
    assert_eq!((i.state, i.running, i.version), (StateCode::Trial, Slot::B, Version(0x0001_0001)));
    assert!(i.slots[1].tbyb);
    assert_eq!(i.min_version, Version(0x0001_0001));
    // (3) Confirm: bought in place, watchdog stopped, then a reset boots B.
    d.fw_confirm(3, || {}).unwrap();
    assert_eq!(d.fw_info().unwrap().state, StateCode::Confirmed);
    assert!(board(&mut d).fw().rom().watchdog_stopped);
    board(&mut d).reset(None);
    let i = d.fw_info().unwrap();
    assert_eq!((i.state, i.running, i.slots[1].tbyb), (StateCode::Idle, Slot::B, false));
    assert_eq!(i.slots[0].version, Some(Version(0x0001_0000)), "the factory image stays in A");
    // (4) Update to 1.2 into A, reset during the trial without a confirm.
    let img = fw_image(V1_2, 2);
    let done = d.fw_update(&img, opts, |_| {}).unwrap();
    assert_eq!(done.slot, Slot::A);
    let i = d.fw_info().unwrap();
    assert_eq!((i.state, i.running), (StateCode::Trial, Slot::A));
    board(&mut d).reset(None);
    let i = d.fw_info().unwrap();
    assert_eq!((i.state, i.running, i.version), (StateCode::RolledBack, Slot::B, Version(0x0001_0002)));
    assert!(i.slots[0].tbyb, "the unbought image is left in A");
    // 1.2 is used up; 1.3 goes through.
    assert!(d.fw_update(&img, opts, |_| {}).is_err());
    let img = fw_image(V1_3, 3);
    d.fw_update(&img, opts, |_| {}).unwrap();
    d.fw_confirm(3, || {}).unwrap();
    board(&mut d).reset(None);
    let i = d.fw_info().unwrap();
    assert_eq!((i.state, i.running, i.slots[0].version), (StateCode::Idle, Slot::A, Some(Version(0x0001_0003))));
    // The payload store and the trust record were never touched by any of it.
    assert!(d.trust_info().unwrap().state.is_claimed());
}

#[test]
fn y12_confirm_waits_for_the_self_test() {
    let mut d = device(Board::power_on(factory(V1_0)));
    let img = fw_image(V1_1, 4);
    d.fw_update(&img, UpdateOptions { key: None, reboot: false }, |_| {}).unwrap();
    assert_eq!(d.fw_info().unwrap().state, StateCode::Committed);
    // The new firmware boots, but USB is not configured yet.
    board(&mut d).usb = false;
    d.fw_reboot().unwrap();
    let i = d.fw_info().unwrap();
    assert_eq!((i.state, i.self_test_ok), (StateCode::Trial, false));
    let err = d.fw_confirm(2, || {}).unwrap_err();
    assert_eq!(err.status(), Some(HandlerError::Unavailable));
    // USB comes up: the next confirm buys the image at once.
    board(&mut d).usb = true;
    board(&mut d).svc.set_usb_configured(true);
    let mut tries = 0;
    d.fw_confirm(2, || tries += 1).unwrap();
    assert_eq!((d.fw_info().unwrap().state, tries), (StateCode::Confirmed, 0));
}

#[test]
fn y12_requests_reach_the_updater_and_unknown_fw_keys_are_not_found() {
    let mut b = Board::power_on(factory(V1_0));
    let mut out = [0u8; 64];
    let h = b.svc.handle(Key::from_static("sys/fw/nope"), &[], &mut out);
    assert_eq!(h.reply, Reply::Now(Err(HandlerError::NotFound)));
    let h = b.svc.handle(fw_keys::CONFIRM, &[], &mut out);
    assert!(matches!(h.reply, Reply::Now(Err(_))), "no trial to confirm");
    assert_eq!(b.running(), Slot::A);
}

/// `mwctl fw update --no-reboot` against the board model with the image
/// `build.sh rp2350` produced (`RP_BASE_UPDATE_IMAGE`, a `.bin` or UF2),
/// then the reboot, trial and confirm. Skipped when the variable is unset.
#[test]
fn y13_the_built_update_image_installs_through_mwctl() {
    let Some(path) = std::env::var_os("RP_BASE_UPDATE_IMAGE") else {
        std::eprintln!("RP_BASE_UPDATE_IMAGE not set: skipped");
        return;
    };
    let bytes = std::fs::read(&path).expect("RP_BASE_UPDATE_IMAGE is readable");
    let img = load_image(&bytes, None).expect("mwctl accepts the image");
    assert!(img.bytes.len() <= ab().a.size as usize);
    check_update(&img.bytes, ab().a.size).unwrap();
    // A factory board one version lower (0.1) than anything shipped.
    let mut d = device(Board::power_on(factory(FwVersion { major: 0, minor: 1 })));
    let done = d.fw_update(&img, UpdateOptions { key: None, reboot: false }, |_| {}).unwrap();
    assert_eq!((done.slot, done.rebooted), (Slot::B, false));
    let i = d.fw_info().unwrap();
    assert_eq!((i.state, i.target, i.version, i.len), (StateCode::Committed, Some(Slot::B), img.version, img.bytes.len() as u32));
    d.fw_reboot().unwrap();
    assert_eq!(d.fw_info().unwrap().state, StateCode::Trial);
    d.fw_confirm(2, || {}).unwrap();
    // Bought in place, slot B now holds exactly the factory image of it.
    let mut factory_mem = vec![0xFFu8; FLASH];
    factory_flash(&img.bytes, &RP2350, &mut factory_mem).unwrap();
    let (a, b) = (ab().a.offset as usize, ab().b.offset as usize);
    let n = img.bytes.len();
    assert_eq!(&board(&mut d).mem()[b..b + n], &factory_mem[a..a + n]);
}

#[test]
fn y13_uf2s_reassemble_to_the_images_and_the_factory_one_erases_b_and_the_record() {
    use super::uf2::{self, BLOCK, FAMILY_ABSOLUTE, FAMILY_RP2350_ARM_S, PAGE};
    let img = image(V1_0, 20_001, 5);
    let to_uf2 = |pages: Vec<uf2::Page>, family| {
        let mut out = vec![0u8; pages.len() * BLOCK];
        assert_eq!(uf2::write(pages.into_iter(), family, &mut out), Some(out.len()));
        out
    };
    // Update: mwctl reads it back as the image (the last page padded).
    let update = to_uf2(uf2::update_pages(&img).collect(), FAMILY_RP2350_ARM_S);
    assert_eq!(update.len(), img.len().div_ceil(PAGE) * BLOCK);
    let back = mwctl::fw::uf2_to_bin(&update).unwrap();
    assert_eq!(&back[..img.len()], &img[..]);
    assert!(back[img.len()..].iter().all(|&b| b == 0xFF));
    assert_eq!(load_image(&update, None).unwrap().version, Version(0x0001_0000));
    // Every block: magic, family flag, its address, 256 bytes, index/count.
    let n = (update.len() / BLOCK) as u32;
    for (i, b) in update.chunks(BLOCK).enumerate() {
        let w = |at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
        assert_eq!((w(0), w(4), w(8), w(16), w(20), w(24), w(28)), (0x0A32_4655, 0x9E5D_5157, 0x2000, 256, i as u32, n, FAMILY_RP2350_ARM_S));
        assert_eq!((w(12), w(508)), (0x1000_0000 + (i * PAGE) as u32, 0x0AB1_6F30));
    }
    // Factory: absolute, from flash offset 0, plus erased pages at slot B
    // and at both record sectors; payload store and trust untouched.
    let mut flat = vec![0u8; factory_len(&RP2350, img.len()).unwrap()];
    factory_flash(&img, &RP2350, &mut flat).unwrap();
    let factory_uf2 = to_uf2(uf2::factory_pages(&flat, &RP2350).collect(), FAMILY_ABSOLUTE);
    let addrs: Vec<u32> = factory_uf2.chunks(BLOCK).map(|b| u32::from_le_bytes(b[12..16].try_into().unwrap())).collect();
    assert_eq!(addrs[0], 0x1000_0000);
    let extra = &addrs[flat.len().div_ceil(PAGE)..];
    assert_eq!(extra, &[0x1018_0000, 0x102F_E000, 0x102F_F000]);
    assert!(addrs.iter().all(|&a| a < 0x1030_0000), "payload store and trust record kept");
    let back = mwctl::fw::uf2_to_bin(&factory_uf2).unwrap();
    assert_eq!(&back[..flat.len()], &flat[..]);
    assert!(back[flat.len()..].iter().all(|&b| b == 0xFF));
    assert_eq!(rom_boot(&back, None).base, ab().a.offset, "the factory UF2 boots slot A");
    assert_eq!(uf2::write(uf2::update_pages(&img), FAMILY_ABSOLUTE, &mut [0u8; 100]), None);
}
