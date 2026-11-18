//! In-memory fake device for the unit tests.
//!
//! It mirrors `firmware/src/dispatch.rs` on top of the real device-side
//! pieces from the `middleware` crate: requests are cut with the same
//! [`Splitter`], parsed with [`Command::parse`], transfers run through the
//! device's [`Loader`] state machine (so resend deduplication and the
//! commit CRC check are the firmware's own), and `sys/info` is encoded with
//! [`DeviceInfo::encode`] with the 0.5 trailing node id ([`NODE`]). Storage is a `Vec` staged apart from the
//! installed image, like the firmware's two-slot store: a failed or
//! refused commit keeps the previous image. Without the runtime,
//! `sys/payload/start` answers `NotFound`, as in a `+no-wasm` build.
//!
//! Trust mirrors `middleware::loader::trust`: `sys/trust/claim` pins a key
//! when unclaimed (or when built [`FakeDevice::with_rekey_window`], until a
//! claim closes it) and is `Denied` otherwise; once claimed, a commit must
//! be signed by that key over `signed_message(len, version, sha256)` with a
//! version above `min_version`, else `Denied`. The check is mwctl's host
//! copy ([`crate::sign::verify`], Ed25519 and P-256), because mwctl builds
//! `middleware` with no features (the crate's own check needs `trust`).
//!
//! (`esp_base` itself is not used: it requires rustc 1.95 through its own
//! toolchain file, which the host workspace does not use.)
//!
//! With [`FakeDevice::with_fw`] it is also a Pico 2: `sys/fw/*` requests
//! go to the real `middleware::fw_ota::service::FwService` running on the
//! in-memory RP2350 bootrom model (`fw_ota::sim::Rp2350Sim`, feature
//! `fw-sim`, enabled for mwctl's tests only), with the fake's trust state
//! and node id. When a request schedules a reboot the fake sends its reply,
//! then resets the model (as the bootrom does after the reboot delay) and
//! boots a fresh updater; [`FakeDevice::fw_reset`] resets it at any time
//! (the trial watchdog, a reset button).
//!
//! With [`FakeDevice::with_fw_rt`] it is an i.MX RT1050 as far as the
//! updater's image checks go: the service runs with the RT slot image
//! format (`fw_ota::rt_image::RtSlotImage`), so an update must send an
//! `MWRT` slot image (header first). The slots, record and reboots are
//! still the RP2350 bootrom model's, so the images ([`rt_image`]) are RT
//! slot images whose bytes also hold an IMAGE_DEF block loop the model's
//! bootrom can boot (test images only; a real RT image has none).
//!
//! A [`Script`] injects faults by request ordinal (1-based, counting every
//! request frame received): drop the request, drop its reply, or hold the
//! reply back until the next reply is sent. A read with nothing queued
//! returns `Ok(0)` at once, which the link treats as a timeout.

use crate::link::Transport;
use crate::sign::{image_sha256, is_valid_key, verify};
use middleware::Key;
use middleware::fw_ota::picobin::{
    EXE_TBYB, RP2350_ARM_S_EXE, image_def_block_bytes, sentinel_block_bytes,
};
use middleware::fw_ota::rt_image::{RtHeader, RtSlotImage};
use middleware::fw_ota::service::{Context, FwService};
use middleware::fw_ota::sim::Rp2350Sim;
use middleware::loader::trust::{SignedCommit, TrustState, Version, signed_message};
use middleware::loader::{Caps, Command, DeviceInfo, Effect, Loader, NetAddr, PayloadStatus, keys};
use middleware::serial::frame::{self, Frame, MAX_RAW_FRAME, MAX_WIRE_FRAME, Split, Splitter};
use middleware::transport::HandlerError;
use middleware::wasm_abi::ABI_VERSION;
use std::collections::VecDeque;
use std::io;
use std::time::Duration;

/// The node id the fake reports in `sys/info` (as 0.5 firmware does).
pub const NODE: middleware::SenderId = middleware::SenderId::from_mac([0x02, 0xFA, 0x4E, 0, 0, 1]);

/// Faults to inject, by request ordinal.
#[derive(Debug, Clone, Default)]
pub struct Script {
    /// Requests the device never sees.
    pub drop_request: Vec<usize>,
    /// Requests whose reply is lost.
    pub drop_reply: Vec<usize>,
    /// Requests whose reply is sent only just before the next reply.
    pub late_reply: Vec<usize>,
    /// Bytes emitted before the first reply (boot garbage).
    pub preamble: Vec<u8>,
    /// A USB CDC port that disappears when the device resets: the reply
    /// to the request that triggered the reset is lost and the port then
    /// fails every read and write with a broken pipe until
    /// [`FakeDevice::reappear`] (the host reopening the path).
    pub vanish_on_reset: bool,
}

pub struct FakeDevice {
    chip: &'static str,
    firmware: &'static str,
    capacity: u32,
    runtime: bool,
    caps: Caps,
    script: Script,
    splitter: Splitter<MAX_WIRE_FRAME>,
    raw_rx: Vec<u8>,
    tx: VecDeque<u8>,
    held: Vec<u8>,
    requests: usize,
    loader: Loader,
    status: PayloadStatus,
    staging: Vec<u8>,
    installed: Option<Vec<u8>>,
    declared_crc: Option<u32>,
    trust: TrustState,
    rekey_open: bool,
    board: Option<Vec<u8>>,
    manifest: Option<Vec<u8>>,
    /// The stored `sys/payload/bind` record.
    binding: Option<Vec<u8>>,
    fw: Option<Fw>,
    net: Vec<NetAddr>,
    gone: bool,
}

/// The firmware updater on the bootrom model.
struct Fw {
    sim: Rp2350Sim,
    svc: Svc,
    self_test_ok: bool,
}

/// The updater, by image format.
enum Svc {
    /// The RP2350's (picobin).
    Rp(FwService),
    /// The i.MX RT1050's slot image.
    Rt(FwService<RtSlotImage>),
}

impl Fw {
    fn boot(mut sim: Rp2350Sim, self_test_ok: bool, rt: bool) -> Fw {
        if sim.booted().is_none() {
            sim.power_on().expect("a bootable image");
        }
        let svc = if rt {
            Svc::Rt(FwService::boot_for(&mut sim).expect("updater boots"))
        } else {
            Svc::Rp(FwService::boot(&mut sim).expect("updater boots"))
        };
        Fw {
            sim,
            svc,
            self_test_ok,
        }
    }

    fn reset(self) -> Fw {
        let rt = matches!(self.svc, Svc::Rt(_));
        let mut sim = self.sim;
        sim.reset().expect("a bootable image");
        Fw::boot(sim, self.self_test_ok, rt)
    }

    fn handle(
        &mut self,
        ctx: &Context,
        key: Key<'_>,
        body: &[u8],
        out: &mut [u8],
    ) -> Option<Result<usize, HandlerError>> {
        match &mut self.svc {
            Svc::Rp(svc) => svc.handle(&mut self.sim, ctx, key, body, out),
            Svc::Rt(svc) => svc.handle(&mut self.sim, ctx, key, body, out),
        }
    }
}

/// Slot offset of [`rt_image`]'s IMAGE_DEF block (after the RT header).
const RT_IMAGE_DEF_AT: usize = 0x140;
/// Slot offset of [`rt_image`]'s sentinel block (the first sector's last
/// page, as the model's bootrom requires).
const RT_SENTINEL_AT: usize = 0xF40;

/// An RT slot image (header and a `len`-byte image, `len` at least
/// 4 KiB) of version `version` (`major << 16 | minor`) for
/// [`FakeDevice::with_fw_rt`]. Its bytes also hold an IMAGE_DEF block loop
/// of the same version, flagged TBYB when `tbyb`, so the bootrom model
/// boots it; the other image bytes are a function of `seed`.
pub fn rt_image(len: usize, version: u32, tbyb: bool, seed: u8) -> Vec<u8> {
    let at = |slot_offset: usize| slot_offset - middleware::fw_ota::rt_image::HEADER_LEN;
    let mut image: Vec<u8> = (0..len)
        .map(|i| {
            (i as u32)
                .wrapping_mul(2_654_435_761)
                .wrapping_add(u32::from(seed)) as u8
        })
        .collect();
    let flags = RP2350_ARM_S_EXE | if tbyb { EXE_TBYB } else { 0 };
    let link = (RT_SENTINEL_AT - RT_IMAGE_DEF_AT) as i32;
    let def = image_def_block_bytes(flags, (version >> 16) as u16, version as u16, link);
    image[at(RT_IMAGE_DEF_AT)..at(RT_IMAGE_DEF_AT) + def.len()].copy_from_slice(&def);
    let sentinel = sentinel_block_bytes(-link);
    image[at(RT_SENTINEL_AT)..at(RT_SENTINEL_AT) + sentinel.len()].copy_from_slice(&sentinel);
    let header = RtHeader::for_image(Version(version), &image).expect("a small image");
    let mut slot = header.encode().to_vec();
    slot.extend_from_slice(&image);
    slot
}

impl FakeDevice {
    pub fn new(chip: &'static str, firmware: &'static str, capacity: u32, runtime: bool) -> Self {
        FakeDevice {
            chip,
            firmware,
            capacity,
            runtime,
            caps: Caps::NONE,
            script: Script::default(),
            splitter: Splitter::new(),
            raw_rx: Vec::new(),
            tx: VecDeque::new(),
            held: Vec::new(),
            requests: 0,
            loader: Loader::Idle,
            status: PayloadStatus::Empty,
            staging: Vec::new(),
            installed: None,
            declared_crc: None,
            trust: TrustState::Unclaimed,
            rekey_open: false,
            board: None,
            manifest: None,
            binding: None,
            fw: None,
            net: Vec::new(),
            gone: false,
        }
    }

    /// Answers `sys/manifest` with `body`; without it the key is
    /// `NotFound`, as on firmware without a manifest.
    pub fn with_manifest(self, body: Vec<u8>) -> Self {
        FakeDevice {
            manifest: Some(body),
            ..self
        }
    }

    /// Answers `sys/board` with `body`; without it the key is `NotFound`,
    /// as on firmware older than 0.6.
    pub fn with_board(self, body: Vec<u8>) -> Self {
        FakeDevice {
            board: Some(body),
            ..self
        }
    }

    /// The same device reporting `caps` in `sys/info` (none by default).
    /// A two-link board answering `sys/net/addr` (the Colorlight
    /// 5A-75B's defaults), as its gateware does.
    pub fn with_links(self) -> Self {
        let link = |n: u8| NetAddr {
            ip: [192, 168, 1 + n, 50],
            mac: [0x02, 0x5A, 0x75, 0xB0, 0, n],
            peer_ip: [0; 4],
            peer_port: 0,
        };
        FakeDevice { net: vec![link(0), link(1)], ..self }
    }

    /// Link `n`'s current addresses.
    pub fn net(&self, n: usize) -> NetAddr {
        self.net[n]
    }

    pub fn with_caps(self, caps: Caps) -> Self {
        FakeDevice { caps, ..self }
    }

    /// A Pico 2 with the A/B firmware updater on `sim` (powered on here if
    /// it is not).
    pub fn with_fw(self, sim: Rp2350Sim) -> Self {
        FakeDevice {
            fw: Some(Fw::boot(sim, true, false)),
            ..self
        }
    }

    /// An i.MX RT1050's updater (the RT slot image format) on `sim`, whose
    /// slots hold [`rt_image`]s (see the module docs).
    pub fn with_fw_rt(self, sim: Rp2350Sim) -> Self {
        FakeDevice {
            fw: Some(Fw::boot(sim, true, true)),
            ..self
        }
    }

    /// The port is back after a [`Script::vanish_on_reset`] reset (the
    /// host reopened it).
    pub fn reappear(&mut self) {
        self.gone = false;
    }

    /// Resets the bootrom model now (watchdog or reset button) and boots a
    /// fresh updater.
    pub fn fw_reset(&mut self) {
        self.fw = self.fw.take().map(Fw::reset);
    }

    /// The bootrom model, with [`FakeDevice::with_fw`] or
    /// [`FakeDevice::with_fw_rt`].
    pub fn fw_sim(&self) -> Option<&Rp2350Sim> {
        self.fw.as_ref().map(|fw| &fw.sim)
    }

    /// Sets whether the firmware's self-test passed.
    pub fn fw_self_test(&mut self, ok: bool) {
        if let Some(fw) = &mut self.fw {
            fw.self_test_ok = ok;
        }
    }

    /// Already claimed with `key` (as if claimed before this session).
    pub fn with_claim(self, key: middleware::loader::trust::PublicKey) -> Self {
        FakeDevice {
            trust: TrustState::Claimed {
                key,
                min_version: Version(0),
            },
            ..self
        }
    }

    /// The re-key window is open (as if BOOT had been held).
    pub fn with_rekey_window(self) -> Self {
        FakeDevice {
            rekey_open: true,
            ..self
        }
    }

    /// The device's commit check (`TrustState::accept_commit`).
    fn accept(
        &self,
        image: &[u8],
        auth: Option<&SignedCommit>,
    ) -> Result<TrustState, HandlerError> {
        let TrustState::Claimed { key, min_version } = self.trust else {
            return Ok(self.trust);
        };
        let auth = auth.ok_or(HandlerError::Denied)?;
        if auth.version <= min_version {
            return Err(HandlerError::Denied);
        }
        let len = u32::try_from(image.len()).map_err(|_| HandlerError::Denied)?;
        let m = signed_message(len, auth.version, &image_sha256(image));
        if !verify(&key, &m, &auth.signature) {
            return Err(HandlerError::Denied);
        }
        Ok(TrustState::Claimed {
            key,
            min_version: auth.version,
        })
    }

    fn resting(&self) -> PayloadStatus {
        if self.installed.is_some() {
            PayloadStatus::Stopped
        } else {
            PayloadStatus::Empty
        }
    }

    pub fn with_script(self, script: Script) -> Self {
        let mut d = FakeDevice { script, ..self };
        d.tx.extend(std::mem::take(&mut d.script.preamble));
        d
    }

    pub fn raw_rx(&self) -> &[u8] {
        &self.raw_rx
    }

    pub fn requests(&self) -> usize {
        self.requests
    }

    pub fn status(&self) -> PayloadStatus {
        self.status
    }

    pub fn installed(&self) -> Option<&[u8]> {
        self.installed.as_deref()
    }

    pub fn declared_crc(&self) -> Option<u32> {
        self.declared_crc
    }

    fn emit(&mut self, f: &Frame<'_>) -> Vec<u8> {
        let mut scratch = [0u8; MAX_RAW_FRAME];
        let mut out = [0u8; MAX_WIRE_FRAME];
        let n = frame::encode(f, &mut scratch, &mut out).expect("encodable");
        out[..n].to_vec()
    }

    fn publish(&mut self, key: Key<'_>, payload: &[u8]) {
        let bytes = self.emit(&Frame::Publish { key, payload });
        self.tx.extend(bytes);
    }

    fn set_status(&mut self, s: PayloadStatus) {
        if self.status != s {
            self.status = s;
            self.publish(keys::STATUS, &[s as u8]);
        }
    }

    /// Mirrors `dispatch::handle_request` plus the shell's actions.
    fn handle(&mut self, key: Key<'_>, body: &[u8], out: &mut [u8]) -> Result<usize, HandlerError> {
        if let Some(fw) = &mut self.fw {
            let ctx = Context {
                node: NODE,
                trust: self.trust,
                self_test_ok: fw.self_test_ok,
            };
            if let Some(r) = fw.handle(&ctx, key, body, out) {
                return r;
            }
        }
        if key == keys::NET_ADDR && !self.net.is_empty() {
            let link = *body.first().ok_or(HandlerError::InvalidRequest)?;
            let slot = self.net.get_mut(usize::from(link)).ok_or(HandlerError::InvalidRequest)?;
            return match body.len() {
                1 => {
                    let r = slot.set_body(link);
                    out.get_mut(..r.len()).ok_or(HandlerError::BufferTooSmall)?.copy_from_slice(&r);
                    Ok(r.len())
                }
                n if n == 1 + NetAddr::LEN => {
                    *slot = NetAddr::decode(&body[1..]).ok_or(HandlerError::InvalidRequest)?;
                    Ok(0)
                }
                _ => Err(HandlerError::InvalidRequest),
            };
        }
        let cmd = Command::parse(key, body)?.ok_or(HandlerError::NotFound)?;
        match cmd {
            Command::Info => DeviceInfo {
                abi_version: ABI_VERSION,
                capacity: self.capacity,
                status: self.status,
                chip: self.chip,
                firmware: self.firmware,
                node: Some(NODE),
                caps: self.caps,
            }
            .encode(out)
            .ok_or(HandlerError::BufferTooSmall),
            Command::Bind { .. } | Command::Binding if !self.runtime => Err(HandlerError::NotFound),
            Command::Bind { request } => {
                self.binding = Some(request.record.to_vec());
                Ok(0)
            }
            Command::Binding => {
                let body = self.binding.as_deref().ok_or(HandlerError::NotFound)?;
                out.get_mut(..body.len())
                    .ok_or(HandlerError::BufferTooSmall)?
                    .copy_from_slice(body);
                Ok(body.len())
            }
            Command::Manifest => {
                let body = self.manifest.as_deref().ok_or(HandlerError::NotFound)?;
                out.get_mut(..body.len())
                    .ok_or(HandlerError::BufferTooSmall)?
                    .copy_from_slice(body);
                Ok(body.len())
            }
            Command::Board => {
                let body = self.board.as_deref().ok_or(HandlerError::NotFound)?;
                out.get_mut(..body.len())
                    .ok_or(HandlerError::BufferTooSmall)?
                    .copy_from_slice(body);
                Ok(body.len())
            }
            Command::Start if !self.runtime => Err(HandlerError::NotFound),
            Command::Start => match self.status {
                PayloadStatus::Empty | PayloadStatus::Loading => Err(HandlerError::InvalidRequest),
                PayloadStatus::Running => Ok(0),
                PayloadStatus::Stopped | PayloadStatus::Trapped => {
                    self.set_status(PayloadStatus::Running);
                    self.publish(keys::LOG, b"payload started");
                    Ok(0)
                }
            },
            Command::Stop => {
                if matches!(self.status, PayloadStatus::Running | PayloadStatus::Trapped) {
                    self.set_status(PayloadStatus::Stopped);
                }
                Ok(0)
            }
            Command::TrustInfo => self
                .trust
                .info(self.rekey_open)
                .encode(out)
                .ok_or(HandlerError::BufferTooSmall),
            Command::TrustClaim { key } => {
                if !is_valid_key(&key) {
                    return Err(HandlerError::InvalidRequest);
                }
                if self.trust.is_claimed() && !self.rekey_open {
                    return Err(HandlerError::Denied);
                }
                self.trust = TrustState::Claimed {
                    key,
                    min_version: Version(0),
                };
                self.rekey_open = false;
                Ok(0)
            }
            Command::Begin { .. } | Command::Chunk { .. } | Command::Commit { .. } => {
                let (next, effect) = self.loader.step(&cmd, self.capacity);
                self.loader = next;
                match effect {
                    Err(e) => {
                        if self.status == PayloadStatus::Loading {
                            self.set_status(self.resting());
                        }
                        Err(e)
                    }
                    Ok(Effect::Erase { .. }) => {
                        if let Command::Begin { crc, .. } = cmd {
                            self.declared_crc = Some(crc);
                        }
                        self.staging.clear();
                        self.set_status(PayloadStatus::Loading);
                        Ok(0)
                    }
                    Ok(Effect::Write { data, .. }) => {
                        self.staging.extend_from_slice(data);
                        Ok(0)
                    }
                    Ok(Effect::Activate { auth, .. }) => {
                        let staged = std::mem::take(&mut self.staging);
                        match self.accept(&staged, auth.as_ref()) {
                            Ok(next) => self.trust = next,
                            Err(e) => {
                                self.set_status(self.resting());
                                return Err(e);
                            }
                        }
                        self.installed = Some(staged);
                        self.set_status(PayloadStatus::Stopped);
                        self.publish(keys::LOG, b"payload installed");
                        Ok(0)
                    }
                    Ok(Effect::None) => Ok(0),
                }
            }
        }
    }

    fn on_request(&mut self, cobs: &mut [u8]) {
        let Ok(Frame::Request { id, key, payload }) = frame::decode_wire(cobs) else {
            return;
        };
        self.requests += 1;
        let n = self.requests;
        if self.script.drop_request.contains(&n) {
            return;
        }
        let mut out = [0u8; 128];
        let result = self.handle(key, payload, &mut out);
        let body = result.map(|len| &out[..len]);
        let bytes = self.emit(&Frame::Response { id, result: body });
        if self.script.drop_reply.contains(&n) {
            return;
        }
        if self.script.late_reply.contains(&n) {
            self.held = bytes;
            return;
        }
        let held = std::mem::take(&mut self.held);
        self.tx.extend(held);
        self.tx.extend(bytes);
    }

    fn on_frame(&mut self, cobs: &mut [u8]) {
        self.on_request(cobs);
        // A reboot the updater scheduled fires after the reply is out (or
        // lost).
        if self
            .fw
            .as_ref()
            .is_some_and(|fw| fw.sim.scheduled().is_some())
        {
            self.fw_reset();
            if self.script.vanish_on_reset {
                self.tx.clear();
                self.gone = true;
            }
        }
    }
}

impl Transport for FakeDevice {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.gone {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        self.raw_rx.extend_from_slice(bytes);
        for &b in bytes {
            if let Split::Frame(_) = self.splitter.push(b) {
                let mut run = self.splitter.frame().to_vec();
                self.on_frame(&mut run);
            }
        }
        Ok(())
    }

    fn read(&mut self, buf: &mut [u8], _timeout: Duration) -> io::Result<usize> {
        if self.gone {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let n = buf.len().min(self.tx.len());
        for (dst, src) in buf.iter_mut().zip(self.tx.drain(..n)) {
            *dst = src;
        }
        Ok(n)
    }
}
