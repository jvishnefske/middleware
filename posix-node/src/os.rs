//! The Linux A/B root (design.md, "Linux A/B root (OS)"): `sys/os/*`, the
//! update of the inactive slot of the KD240's card, and the trial boot.
//!
//! The protocol and the boot-state format are in `middleware::os_ab`. This
//! module is the shell: it owns the slots and the boot state through an
//! [`OsDevice`], keeps the record of the anti-rollback floor in the node's
//! state directory, and answers each request:
//!
//! * `begin` writes only the inactive slot (the one the running root is not
//!   on). A transfer lives in memory; a restart drops it, and the host
//!   begins again.
//! * `chunk` writes at the offset; a chunk already received is acknowledged.
//! * `commit` reads the written slot back and compares its SHA-256 with the
//!   declared one; on a claimed device it also needs the root-signed
//!   `Os` release cert for this chip with the same length, hash and a
//!   version above the floor. Then the slot is ready and the floor rises to
//!   its version ([`Record`]), saved before the reply.
//! * `reboot` writes a trial of the ready slot into the boot state (one try)
//!   and asks the host to reboot. U-Boot boots the trial once.
//! * `confirm`, from the trial, commits it: the boot state's active slot
//!   becomes the one running. The host calls it once `sys/os/info` shows
//!   the new slot running and the node answering.
//! * `abort` drops a transfer or a ready slot; from a trial it writes the
//!   old slot back as active and reboots into it.
//!
//! When the trial is never confirmed, U-Boot runs out of tries and boots the
//! committed slot again: the next start reports [`StateCode::RolledBack`].

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use middleware::Key;
use middleware::SenderId;
use middleware::loader::release::{CertKind, SignedCert, verify_cert};
use middleware::loader::trust::{ControlCounter, ControlOp, SignedControl, TrustState, Version};
use middleware::os_ab::bootstate::{self, BootState, COPY_LBA, SECTOR, Slot};
use middleware::os_ab::wire::{CHUNK_LEN, Command, OsInfo, StateCode};
use middleware::transport::HandlerError;
use sha2::{Digest, Sha256};

/// Bytes of the record file.
pub const RECORD_LEN: usize = 32;
/// Its magic.
pub const RECORD_MAGIC: &[u8; 4] = b"MWOS";
/// Its format byte.
pub const RECORD_FORMAT: u8 = 1;
/// The record's file name in the state directory.
pub const RECORD_FILE: &str = "os-record";

/// The two slots, the boot state and the reboot, as the root needs them.
/// The KD240's are the card's partitions and gap sectors; a host test uses
/// memory ([`MemDevice`]).
pub trait OsDevice: Send {
    /// Bytes a slot holds.
    fn capacity(&self) -> u64;
    /// Writes `data` at `offset` of `slot`.
    fn write(&mut self, slot: Slot, offset: u64, data: &[u8]) -> io::Result<()>;
    /// Reads `buf.len()` bytes at `offset` of `slot`.
    fn read(&mut self, slot: Slot, offset: u64, buf: &mut [u8]) -> io::Result<()>;
    /// Makes the slot's writes durable.
    fn sync(&mut self, slot: Slot) -> io::Result<()>;
    /// The two boot-state copies as stored.
    fn boot_copies(&mut self) -> io::Result<[[u8; SECTOR]; 2]>;
    /// Stores one boot-state copy, durably.
    fn write_boot_copy(&mut self, index: usize, sector: &[u8; SECTOR]) -> io::Result<()>;
    /// The slot this root booted from.
    fn running(&self) -> Slot;
    /// Asks for a reboot (the request is accepted; the reset follows).
    fn reboot(&mut self) -> io::Result<()>;
}

/// The control target of this node: the MAC-shaped six bytes `mwctl` signs
/// a control request for (`zlink::target`, the same derivation).
pub fn control_target(node: SenderId) -> [u8; 6] {
    let n = node.0;
    [0x02, n[3], n[4], n[5], n[6], n[7]]
}

/// The anti-rollback record: what the node has accepted, kept beside the
/// node's other state (not in the boot-state sector, which `boot.scr`
/// rewrites with its own keys only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    /// A new root's version must exceed this.
    pub floor: u32,
    /// The version of the root in each slot (0 when unknown).
    pub versions: [u32; 2],
    /// A slot committed and not yet made a trial.
    pub ready: Option<Slot>,
    /// The last signed control counter accepted over the network.
    pub control: u64,
}

impl Default for Record {
    fn default() -> Self {
        Record {
            floor: 0,
            versions: [0; 2],
            ready: None,
            control: 0,
        }
    }
}

impl Record {
    pub fn encode(&self) -> [u8; RECORD_LEN] {
        let mut b = [0u8; RECORD_LEN];
        b[0..4].copy_from_slice(RECORD_MAGIC);
        b[4] = RECORD_FORMAT;
        b[5] = self.ready.map_or(0xff, |s| s.index() as u8);
        b[8..12].copy_from_slice(&self.floor.to_le_bytes());
        b[12..16].copy_from_slice(&self.versions[0].to_le_bytes());
        b[16..20].copy_from_slice(&self.versions[1].to_le_bytes());
        b[20..28].copy_from_slice(&self.control.to_le_bytes());
        let crc = bootstate::crc32(&b[..28]);
        b[28..32].copy_from_slice(&crc.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Record, &'static str> {
        if b.len() != RECORD_LEN {
            return Err("length");
        }
        if &b[0..4] != RECORD_MAGIC || b[4] != RECORD_FORMAT {
            return Err("magic or format");
        }
        if bootstate::crc32(&b[..28]) != u32::from_le_bytes([b[28], b[29], b[30], b[31]]) {
            return Err("bad CRC");
        }
        let u = |at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
        let ready = match b[5] {
            0xff => None,
            i => Some(Slot::from_index(i).ok_or("ready slot")?),
        };
        let control = u64::from_le_bytes(b[20..28].try_into().map_err(|_| "control")?);
        Ok(Record {
            floor: u(8),
            versions: [u(12), u(16)],
            ready,
            control,
        })
    }
}

/// One update in flight (in memory).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Transfer {
    target: Slot,
    len: u32,
    version: Version,
    sha256: [u8; 32],
    got: Vec<bool>,
    count: usize,
}

impl Transfer {
    fn chunks(&self) -> usize {
        (self.len as usize).div_ceil(CHUNK_LEN)
    }
    fn chunk_len(&self, index: usize) -> usize {
        let start = index * CHUNK_LEN;
        (self.len as usize - start).min(CHUNK_LEN)
    }
}

/// The root's update state: the device, the record and a transfer.
pub struct OsUpdates {
    dev: Box<dyn OsDevice>,
    record: Record,
    record_path: PathBuf,
    /// The control target signed control requests name ([`control_target`]).
    target: [u8; 6],
    transfer: Option<Transfer>,
    /// Confirmed since this start (reported as `confirmed` until restart).
    confirmed: bool,
}

impl std::fmt::Debug for OsUpdates {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OsUpdates")
            .field("record", &self.record)
            .field("transfer", &self.transfer.as_ref().map(|t| t.target))
            .finish_non_exhaustive()
    }
}

impl OsUpdates {
    /// Opens the update state: the record in `record_path` (absent: the
    /// default; unreadable: an error, so the root refuses to update rather
    /// than forget its floor).
    pub fn open(dev: Box<dyn OsDevice>, record_path: PathBuf, node: SenderId) -> io::Result<Self> {
        let record = match std::fs::read(&record_path) {
            Ok(b) => Record::decode(&b).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {e}", record_path.display()),
                )
            })?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Record::default(),
            Err(e) => return Err(e),
        };
        Ok(OsUpdates {
            dev,
            record,
            record_path,
            target: control_target(node),
            transfer: None,
            confirmed: false,
        })
    }

    /// The record (for the node's log and tests).
    pub fn record(&self) -> Record {
        self.record
    }

    fn running(&self) -> Slot {
        self.dev.running()
    }

    fn copies(&mut self) -> Result<[[u8; SECTOR]; 2], HandlerError> {
        self.dev.boot_copies().map_err(|_| HandlerError::Internal)
    }

    /// The newest valid boot state, or `Unavailable` without one.
    fn boot(&mut self) -> Result<BootState, HandlerError> {
        let c = self.copies()?;
        bootstate::newest([&c[0], &c[1]])
            .map(|(_, s)| s)
            .ok_or(HandlerError::Unavailable)
    }

    fn write_boot(&mut self, next: BootState) -> Result<(), HandlerError> {
        let c = self.copies()?;
        let (index, sector) = bootstate::plan_write([&c[0], &c[1]], next);
        self.dev
            .write_boot_copy(index, &sector)
            .map_err(|_| HandlerError::Internal)
    }

    fn save_record(&mut self) -> Result<(), HandlerError> {
        let tmp = self.record_path.with_extension("tmp");
        let write = || -> io::Result<()> {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&self.record.encode())?;
            f.sync_all()?;
            std::fs::rename(&tmp, &self.record_path)
        };
        write().map_err(|_| HandlerError::Internal)
    }

    /// The state as `sys/os/info` reports it, from the record, the transfer
    /// and the boot state (`None`: no valid boot state).
    pub fn state(&mut self) -> StateCode {
        if self.transfer.is_some() {
            return StateCode::Receiving;
        }
        if self.record.ready.is_some() {
            return StateCode::Committed;
        }
        let running = self.running();
        match self.boot() {
            Err(_) => StateCode::Idle,
            Ok(b) => match b.try_slot {
                Some(_) if b.tries > 0 => StateCode::Committed,
                Some(t) if t == running => StateCode::Trial,
                Some(_) => StateCode::RolledBack,
                None if self.confirmed => StateCode::Confirmed,
                None => StateCode::Idle,
            },
        }
    }

    /// `sys/os/info`.
    pub fn info(&mut self) -> OsInfo {
        let running = self.running();
        let state = self.state();
        OsInfo {
            running,
            state,
            target: Some(running.other()),
            running_version: self.record.versions[running.index()],
            floor: self.record.floor,
            capacity: self.dev.capacity(),
        }
    }

    /// The floor a new root's version must exceed.
    fn version_floor(&self) -> u32 {
        self.record
            .floor
            .max(self.record.versions[self.running().index()])
    }

    /// Answers one `sys/os/*` request. `trust` is the node's root of trust
    /// (`None` when its record could not be read: the requests are refused);
    /// `chips` are the names a release cert may target (the chip, and the
    /// board when known); `network` is whether the request came over the
    /// network (a claimed node then needs control requests signed by its
    /// pinned key) rather than the local serial port.
    pub fn handle(
        &mut self,
        key: Key<'_>,
        body: &[u8],
        trust: Option<&TrustState>,
        chips: &[&str],
        network: bool,
        out: &mut [u8],
    ) -> Result<usize, HandlerError> {
        match Command::parse(key, body)? {
            None => Err(HandlerError::NotFound),
            Some(Command::Info) => self
                .info()
                .encode(out)
                .map(Some)
                .ok_or(HandlerError::BufferTooSmall),
            Some(Command::Begin {
                len,
                version,
                sha256,
            }) => self.begin(len, version, sha256, out),
            Some(Command::Chunk { offset, data }) => self.chunk(offset, data),
            Some(Command::Commit { cert }) => self.commit(cert, trust, chips),
            Some(Command::Reboot { control }) => {
                self.gate(ControlOp::OsReboot, control, trust, network)?;
                self.reboot()
            }
            Some(Command::Confirm { control }) => {
                self.gate(ControlOp::OsConfirm, control, trust, network)?;
                self.confirm()
            }
            Some(Command::Abort { control }) => {
                self.gate(ControlOp::OsAbort, control, trust, network)?;
                self.abort()
            }
        }
        .map(|n| n.unwrap_or(0))
    }

    /// A control request (reboot, confirm, abort). Over the network on a
    /// claimed node it must be signed by the pinned key for this node, with
    /// a counter above the last one accepted (saved before the action). On
    /// an unclaimed node, or over the local serial port, it needs no
    /// signature: trust on first use, and physical access.
    fn gate(
        &mut self,
        op: ControlOp,
        control: Option<SignedControl>,
        trust: Option<&TrustState>,
        network: bool,
    ) -> Result<(), HandlerError> {
        let trust = trust.ok_or(HandlerError::Unavailable)?;
        if !network || !matches!(trust, TrustState::Claimed { .. }) {
            return Ok(());
        }
        let auth = control.ok_or(HandlerError::Denied)?;
        let next =
            trust.accept_control(ControlCounter(self.record.control), op, &self.target, &auth)?;
        self.record.control = next.0;
        self.save_record()
    }

    fn begin(
        &mut self,
        len: u32,
        version: Version,
        sha256: [u8; 32],
        out: &mut [u8],
    ) -> Result<Option<usize>, HandlerError> {
        let target = self.running().other();
        if let Some(t) = &self.transfer {
            if t.target == target && t.len == len && t.version == version && t.sha256 == sha256 {
                return reply_slot(out, target).map(Some); // a resent begin resumes
            }
        }
        match self.state() {
            StateCode::Idle
            | StateCode::Receiving
            | StateCode::Confirmed
            | StateCode::RolledBack => {}
            _ => return Err(HandlerError::Unavailable), // a ready or a trial root: not now
        }
        if let Ok(b) = self.boot() {
            if b.active == target {
                return Err(HandlerError::Unavailable); // never write the committed slot
            }
        }
        if version.0 <= self.version_floor() {
            return Err(HandlerError::Denied);
        }
        if len == 0 {
            return Err(HandlerError::InvalidRequest);
        }
        if u64::from(len) > self.dev.capacity() {
            return Err(HandlerError::BufferTooSmall);
        }
        let chunks = (len as usize).div_ceil(CHUNK_LEN);
        self.transfer = Some(Transfer {
            target,
            len,
            version,
            sha256,
            got: vec![false; chunks],
            count: 0,
        });
        reply_slot(out, target).map(Some)
    }

    fn chunk(&mut self, offset: u32, data: &[u8]) -> Result<Option<usize>, HandlerError> {
        let Some(t) = self.transfer.as_mut() else {
            return Err(HandlerError::Unavailable); // no begin
        };
        let offset = offset as usize;
        if offset % CHUNK_LEN != 0 || offset >= t.len as usize {
            return Err(HandlerError::InvalidRequest);
        }
        let index = offset / CHUNK_LEN;
        if data.len() != t.chunk_len(index) {
            return Err(HandlerError::InvalidRequest);
        }
        if t.got[index] {
            return Ok(None); // already received: acknowledged, not written again
        }
        let target = t.target;
        self.dev
            .write(target, offset as u64, data)
            .map_err(|_| HandlerError::Internal)?;
        if let Some(t) = self.transfer.as_mut() {
            t.got[index] = true;
            t.count += 1;
        }
        Ok(None)
    }

    fn commit(
        &mut self,
        cert: Option<SignedCert<'_>>,
        trust: Option<&TrustState>,
        chips: &[&str],
    ) -> Result<Option<usize>, HandlerError> {
        let trust = trust.ok_or(HandlerError::Unavailable)?;
        let t = self.transfer.clone().ok_or(HandlerError::Unavailable)?;
        if t.count != t.chunks() {
            return Err(HandlerError::InvalidRequest); // not every chunk arrived
        }
        // Who may install: a claimed device needs the root's Os cert for
        // this image; an unclaimed one accepts any (trust on first use).
        if let TrustState::Claimed { key, .. } = trust {
            let sc = cert.ok_or(HandlerError::Denied)?;
            let c = &sc.cert;
            let for_us = c.kind == CertKind::Os
                && chips.iter().any(|chip| c.target == *chip)
                && c.len == t.len
                && c.sha256 == t.sha256
                && c.version == t.version
                && c.version.0 > self.version_floor();
            if !for_us || !verify_cert(key, &sc) {
                return Err(HandlerError::Denied);
            }
        }
        // The bytes on the slot are the ones the cert names.
        let digest = self
            .digest(t.target, t.len)
            .map_err(|_| HandlerError::Internal)?;
        if digest != t.sha256 {
            self.transfer = None; // known wrong: abandon the transfer
            return Err(HandlerError::InvalidRequest);
        }
        self.record.versions[t.target.index()] = t.version.0;
        self.record.floor = self.record.floor.max(t.version.0);
        self.record.ready = Some(t.target);
        self.save_record()?;
        self.transfer = None;
        Ok(None)
    }

    fn digest(&mut self, slot: Slot, len: u32) -> io::Result<[u8; 32]> {
        let mut h = Sha256::new();
        let mut buf = vec![0u8; CHUNK_LEN];
        let mut at = 0u64;
        while at < u64::from(len) {
            let n = (u64::from(len) - at).min(CHUNK_LEN as u64) as usize;
            self.dev.read(slot, at, &mut buf[..n])?;
            h.update(&buf[..n]);
            at += n as u64;
        }
        Ok(h.finalize().into())
    }

    fn reboot(&mut self) -> Result<Option<usize>, HandlerError> {
        let Some(target) = self.record.ready else {
            return Err(HandlerError::Unavailable);
        };
        let b = self.boot()?;
        self.write_boot(b.with_trial(target))?;
        self.record.ready = None;
        self.save_record()?;
        self.dev.reboot().map_err(|_| HandlerError::Internal)?;
        Ok(None)
    }

    fn confirm(&mut self) -> Result<Option<usize>, HandlerError> {
        if self.state() == StateCode::Confirmed {
            return Ok(None); // idempotent
        }
        if self.state() != StateCode::Trial {
            return Err(HandlerError::Unavailable);
        }
        let b = self.boot()?;
        self.write_boot(b.committed())?;
        self.confirmed = true;
        Ok(None)
    }

    fn abort(&mut self) -> Result<Option<usize>, HandlerError> {
        self.transfer = None;
        if self.record.ready.take().is_some() {
            self.save_record()?;
        }
        if self.state() == StateCode::Trial {
            // Back to the committed slot, now.
            let b = self.boot()?;
            self.write_boot(BootState {
                try_slot: None,
                tries: 0,
                ..b
            })?;
            self.dev.reboot().map_err(|_| HandlerError::Internal)?;
        }
        Ok(None)
    }
}

fn reply_slot(out: &mut [u8], slot: Slot) -> Result<usize, HandlerError> {
    let b = out.first_mut().ok_or(HandlerError::BufferTooSmall)?;
    *b = slot.index() as u8;
    Ok(1)
}

/// The card's two slots and boot state as files (the KD240's block
/// devices, or regular files in the simulation). The boot state is at
/// [`COPY_LBA`] within `state`; the running slot is `mw.slot=` on
/// `cmdline`; a reboot runs `reboot` (empty: none).
pub struct FileDevice {
    slots: [std::fs::File; 2],
    state: std::fs::File,
    running: Slot,
    reboot: Vec<String>,
}

impl FileDevice {
    pub fn open(
        slots: &[PathBuf; 2],
        state: &Path,
        cmdline: &Path,
        reboot: Vec<String>,
    ) -> io::Result<Self> {
        let open = |p: &Path| std::fs::OpenOptions::new().read(true).write(true).open(p);
        let line = std::fs::read_to_string(cmdline)?;
        let running = Slot::from_cmdline(&line).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "no mw.slot= on the kernel command line",
            )
        })?;
        Ok(FileDevice {
            slots: [open(&slots[0])?, open(&slots[1])?],
            state: open(state)?,
            running,
            reboot,
        })
    }

    fn slot(&mut self, slot: Slot) -> &mut std::fs::File {
        &mut self.slots[slot.index()]
    }
}

impl OsDevice for FileDevice {
    fn capacity(&self) -> u64 {
        // A block device reports its size at the end; a file its length.
        let mut f = &self.slots[0];
        f.seek(SeekFrom::End(0)).unwrap_or(0)
    }

    fn write(&mut self, slot: Slot, offset: u64, data: &[u8]) -> io::Result<()> {
        let f = self.slot(slot);
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(data)
    }

    fn read(&mut self, slot: Slot, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let f = self.slot(slot);
        f.seek(SeekFrom::Start(offset))?;
        f.read_exact(buf)
    }

    fn sync(&mut self, slot: Slot) -> io::Result<()> {
        self.slot(slot).sync_data()
    }

    fn boot_copies(&mut self) -> io::Result<[[u8; SECTOR]; 2]> {
        let mut out = [[0u8; SECTOR]; 2];
        for (i, lba) in COPY_LBA.iter().enumerate() {
            self.state.seek(SeekFrom::Start(lba * SECTOR as u64))?;
            self.state.read_exact(&mut out[i])?;
        }
        Ok(out)
    }

    fn write_boot_copy(&mut self, index: usize, sector: &[u8; SECTOR]) -> io::Result<()> {
        self.state
            .seek(SeekFrom::Start(COPY_LBA[index] * SECTOR as u64))?;
        self.state.write_all(sector)?;
        self.state.sync_data()
    }

    fn running(&self) -> Slot {
        self.running
    }

    fn reboot(&mut self) -> io::Result<()> {
        let Some((prog, args)) = self.reboot.split_first() else {
            return Ok(());
        };
        let status = std::process::Command::new(prog).args(args).status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!("{prog} exited with {status}")))
        }
    }
}

/// An in-memory card, for the tests: the slots, the copies, and a boot
/// that runs U-Boot's decision ([`bootstate::choose_boot`]).
#[derive(Clone)]
pub struct MemDevice(pub std::sync::Arc<std::sync::Mutex<MemCard>>);

pub struct MemCard {
    pub slots: [Vec<u8>; 2],
    pub copies: [[u8; SECTOR]; 2],
    pub running: Slot,
    pub reboots: u32,
}

impl MemDevice {
    /// A card of `capacity` bytes per slot, slot A committed and running.
    pub fn new(capacity: usize) -> Self {
        let mut copies = [[0u8; SECTOR]; 2];
        copies[0] = bootstate::encode(&BootState::initial());
        MemDevice(std::sync::Arc::new(std::sync::Mutex::new(MemCard {
            slots: [vec![0; capacity], vec![0; capacity]],
            copies,
            running: Slot::A,
            reboots: 0,
        })))
    }

    /// The card's power cycle: U-Boot picks the slot to boot and writes its
    /// try (`boot.scr`).
    pub fn boot(&self) {
        let mut c = self.0.lock().unwrap();
        let b = bootstate::choose_boot([&c.copies[0], &c.copies[1]]);
        if let Some(next) = b.wrote {
            let (i, sector) = bootstate::plan_write([&c.copies[0], &c.copies[1]], next);
            c.copies[i] = sector;
        }
        c.running = b.slot;
    }
}

impl OsDevice for MemDevice {
    fn capacity(&self) -> u64 {
        self.0.lock().unwrap().slots[0].len() as u64
    }
    fn write(&mut self, slot: Slot, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut c = self.0.lock().unwrap();
        let s = &mut c.slots[slot.index()];
        let end = offset as usize + data.len();
        s.get_mut(offset as usize..end)
            .ok_or_else(|| io::Error::other("past the slot"))?
            .copy_from_slice(data);
        Ok(())
    }
    fn read(&mut self, slot: Slot, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let c = self.0.lock().unwrap();
        let s = &c.slots[slot.index()];
        let src = s
            .get(offset as usize..offset as usize + buf.len())
            .ok_or_else(|| io::Error::other("past the slot"))?;
        buf.copy_from_slice(src);
        Ok(())
    }
    fn sync(&mut self, _: Slot) -> io::Result<()> {
        Ok(())
    }
    fn boot_copies(&mut self) -> io::Result<[[u8; SECTOR]; 2]> {
        Ok(self.0.lock().unwrap().copies)
    }
    fn write_boot_copy(&mut self, index: usize, sector: &[u8; SECTOR]) -> io::Result<()> {
        self.0.lock().unwrap().copies[index] = *sector;
        Ok(())
    }
    fn running(&self) -> Slot {
        self.0.lock().unwrap().running
    }
    fn reboot(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().reboots += 1;
        Ok(())
    }
}

/// The names a release cert for this node may target: its chip, and its
/// board when the device tree names one.
pub fn cert_targets(chip: &'static str, board: Option<&'static str>) -> Vec<&'static str> {
    std::iter::once(chip).chain(board).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::loader::release::{ANY_TARGET, COMMIT_BODY_MAX, CertKind, SoftwareCert};
    use middleware::loader::trust::{PublicKey, Signature, control_message};
    use middleware::os_ab::wire::{CHUNK_LEN, keys};
    use sha2::{Digest, Sha256};

    const CHIP: &str = crate::service::CHIP;
    const NODE: SenderId = SenderId([9, 8, 7, 6, 5, 4, 3, 2]);
    const SEED: [u8; 32] = [7; 32];
    const V1: u32 = 0x0001_0001;
    const V2: u32 = 0x0001_0002;

    /// A control request for `op` at `counter`, signed by `seed` over the
    /// control message for NODE's target.
    fn signed_control_by(seed: &[u8; 32], op: ControlOp, counter: u64) -> SignedControl {
        use ed25519_dalek::Signer;
        let msg = control_message(op, &control_target(NODE), ControlCounter(counter));
        let sig = ed25519_dalek::SigningKey::from_bytes(seed)
            .sign(&msg)
            .to_bytes();
        SignedControl {
            counter: ControlCounter(counter),
            signature: Signature(sig),
        }
    }

    fn root_key() -> PublicKey {
        PublicKey::Ed25519(
            ed25519_dalek::SigningKey::from_bytes(&SEED)
                .verifying_key()
                .to_bytes(),
        )
    }

    fn sha(data: &[u8]) -> [u8; 32] {
        Sha256::digest(data).into()
    }

    /// `[cert][sig]` for `cert`, signed by `seed` over the release message.
    fn signed(seed: &[u8; 32], cert: &SoftwareCert<'_>) -> Vec<u8> {
        use ed25519_dalek::Signer;
        let mut buf = [0u8; middleware::loader::release::RELEASE_MESSAGE_MAX];
        let msg = middleware::loader::release::release_message(cert, &mut buf).unwrap();
        let sig = ed25519_dalek::SigningKey::from_bytes(seed)
            .sign(msg)
            .to_bytes();
        let sc = SignedCert {
            cert: *cert,
            signature: Signature(sig),
        };
        let mut out = vec![0u8; COMMIT_BODY_MAX];
        let n = sc.encode(&mut out).unwrap();
        out.truncate(n);
        out
    }

    fn os_cert(len: u32, sha256: [u8; 32], version: u32) -> SoftwareCert<'static> {
        SoftwareCert {
            kind: CertKind::Os,
            target: CHIP,
            channel: "stable",
            version: Version(version),
            len,
            sha256,
        }
    }

    /// One request, answered: the reply bytes or the error.
    fn call(
        os: &mut OsUpdates,
        key: middleware::Key<'static>,
        body: &[u8],
        trust: Option<&TrustState>,
    ) -> Result<Vec<u8>, HandlerError> {
        let mut out = vec![0u8; 64];
        let n = os.handle(key, body, trust, &[CHIP], true, &mut out)?;
        Ok(out[..n].to_vec())
    }

    fn encoded(cmd: Command<'_>) -> Vec<u8> {
        let mut b = vec![0u8; 4 + CHUNK_LEN];
        let n = cmd.encode_body(&mut b).unwrap();
        b.truncate(n);
        b
    }

    fn begin(os: &mut OsUpdates, image: &[u8], version: u32) -> Result<u8, HandlerError> {
        let body = encoded(Command::Begin {
            len: image.len() as u32,
            version: Version(version),
            sha256: sha(image),
        });
        call(os, keys::BEGIN, &body, None).map(|r| r[0])
    }

    /// Sends `image` in chunks of `CHUNK_LEN`.
    fn send(os: &mut OsUpdates, image: &[u8]) -> Result<(), HandlerError> {
        for (i, c) in image.chunks(CHUNK_LEN).enumerate() {
            let body = encoded(Command::Chunk {
                offset: (i * CHUNK_LEN) as u32,
                data: c,
            });
            call(os, keys::CHUNK, &body, None)?;
        }
        Ok(())
    }

    fn image(n: usize, seed: u8) -> Vec<u8> {
        (0..n)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    fn info(os: &mut OsUpdates) -> OsInfo {
        let r = call(os, keys::INFO, &[], None).unwrap();
        OsInfo::decode(&r).unwrap()
    }

    /// Opens a fresh update state on the same card (a restart of the node).
    fn restart(card: &MemDevice, dir: &Path) -> OsUpdates {
        OsUpdates::open(Box::new(card.clone()), dir.join(RECORD_FILE), NODE).unwrap()
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mw-os-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_root_is_installed_tried_and_confirmed() {
        let dir = scratch("confirm");
        let card = MemDevice::new(4 * CHUNK_LEN);
        let mut os = restart(&card, &dir);
        let i = info(&mut os);
        assert_eq!(
            (i.running, i.state, i.target),
            (Slot::A, StateCode::Idle, Some(Slot::B))
        );

        let img = image(CHUNK_LEN + 100, 1); // two chunks
        assert_eq!(begin(&mut os, &img, V1), Ok(1), "the inactive slot B");
        send(&mut os, &img).unwrap();
        call(&mut os, keys::COMMIT, &[], None).unwrap_err(); // no trust: refused
        call(&mut os, keys::COMMIT, &[], Some(&TrustState::Unclaimed)).unwrap();
        assert_eq!(info(&mut os).state, StateCode::Committed);
        assert_eq!(os.record().ready, Some(Slot::B));
        assert_eq!(os.record().floor, V1);

        call(&mut os, keys::REBOOT, &[], Some(&TrustState::Unclaimed)).unwrap();
        assert_eq!(card.0.lock().unwrap().reboots, 1);
        card.boot(); // U-Boot tries B once
        let mut os = restart(&card, &dir);
        let i = info(&mut os);
        assert_eq!((i.running, i.state), (Slot::B, StateCode::Trial));

        call(&mut os, keys::CONFIRM, &[], Some(&TrustState::Unclaimed)).unwrap();
        assert_eq!(info(&mut os).state, StateCode::Confirmed);
        // After a restart the boot state says B is committed.
        card.boot();
        let mut os = restart(&card, &dir);
        let i = info(&mut os);
        assert_eq!(
            (i.running, i.state, i.running_version),
            (Slot::B, StateCode::Idle, V1)
        );
        // Nothing is ever written to the committed slot: the next update
        // goes to A.
        assert_eq!(begin(&mut os, &image(10, 2), V2), Ok(0));
    }

    #[test]
    fn a_trial_that_never_confirms_rolls_back() {
        let dir = scratch("rollback");
        let card = MemDevice::new(4 * CHUNK_LEN);
        let mut os = restart(&card, &dir);
        let img = image(1000, 3);
        begin(&mut os, &img, V1).unwrap();
        send(&mut os, &img).unwrap();
        call(&mut os, keys::COMMIT, &[], Some(&TrustState::Unclaimed)).unwrap();
        call(&mut os, keys::REBOOT, &[], Some(&TrustState::Unclaimed)).unwrap();
        card.boot(); // the trial of B
        card.boot(); // reset without a confirm: the count ran out, A boots
        let mut os = restart(&card, &dir);
        let i = info(&mut os);
        assert_eq!((i.running, i.state), (Slot::A, StateCode::RolledBack));
        // The floor keeps the rolled-back version out: V1 is not above it.
        assert_eq!(begin(&mut os, &img, V1), Err(HandlerError::Denied));
        // A newer root goes to B, the slot that failed.
        assert_eq!(begin(&mut os, &image(1000, 4), V2), Ok(1));
    }

    #[test]
    fn the_floor_and_the_running_version_refuse_older_roots() {
        let dir = scratch("floor");
        let card = MemDevice::new(4 * CHUNK_LEN);
        let mut os = restart(&card, &dir);
        assert_eq!(
            begin(&mut os, &image(10, 0), 0),
            Err(HandlerError::Denied),
            "version 0"
        );
        assert_eq!(
            begin(&mut os, &[], V1),
            Err(HandlerError::InvalidRequest),
            "empty image"
        );
        let big = vec![0u8; 4 * CHUNK_LEN + 1];
        assert_eq!(begin(&mut os, &big, V1), Err(HandlerError::BufferTooSmall));
        assert_eq!(begin(&mut os, &image(10, 0), V1), Ok(1));
    }

    #[test]
    fn a_wrong_hash_abandons_the_transfer() {
        let dir = scratch("hash");
        let card = MemDevice::new(4 * CHUNK_LEN);
        let mut os = restart(&card, &dir);
        let declared = image(500, 5);
        begin(&mut os, &declared, V1).unwrap();
        let other = image(500, 6);
        send(&mut os, &other).unwrap();
        assert_eq!(
            call(&mut os, keys::COMMIT, &[], Some(&TrustState::Unclaimed)),
            Err(HandlerError::InvalidRequest)
        );
        assert_eq!(info(&mut os).state, StateCode::Idle);
        assert_eq!(os.record().ready, None);
    }

    #[test]
    fn a_claimed_root_needs_the_root_signed_os_cert() {
        let dir = scratch("claimed");
        let card = MemDevice::new(4 * CHUNK_LEN);
        let mut os = restart(&card, &dir);
        let img = image(777, 8);
        begin(&mut os, &img, V1).unwrap();
        send(&mut os, &img).unwrap();
        let trust = TrustState::Claimed {
            key: root_key(),
            min_version: middleware::loader::trust::Version(0),
        };
        let good = os_cert(img.len() as u32, sha(&img), V1);
        let denied = Err(HandlerError::Denied);
        // No cert, a cert of another kind, another target, another key, a
        // different hash, a version at the floor: all refused.
        assert_eq!(call(&mut os, keys::COMMIT, &[], Some(&trust)), denied);
        let fw = SoftwareCert {
            kind: CertKind::Firmware,
            ..good
        };
        assert_eq!(
            call(&mut os, keys::COMMIT, &signed(&SEED, &fw), Some(&trust)),
            denied
        );
        let any = SoftwareCert {
            target: ANY_TARGET,
            ..good
        };
        assert_eq!(
            call(&mut os, keys::COMMIT, &signed(&SEED, &any), Some(&trust)),
            denied
        );
        let other = SoftwareCert {
            target: "rp2350",
            ..good
        };
        assert_eq!(
            call(&mut os, keys::COMMIT, &signed(&SEED, &other), Some(&trust)),
            denied
        );
        assert_eq!(
            call(
                &mut os,
                keys::COMMIT,
                &signed(&[9; 32], &good),
                Some(&trust)
            ),
            denied
        );
        let wrong_hash = SoftwareCert {
            sha256: [0; 32],
            ..good
        };
        assert_eq!(
            call(
                &mut os,
                keys::COMMIT,
                &signed(&SEED, &wrong_hash),
                Some(&trust)
            ),
            denied
        );
        // The good one, signed by the root.
        call(&mut os, keys::COMMIT, &signed(&SEED, &good), Some(&trust)).unwrap();
        assert_eq!(info(&mut os).state, StateCode::Committed);
    }

    #[test]
    fn chunks_and_begin_are_checked() {
        let dir = scratch("chunks");
        let card = MemDevice::new(4 * CHUNK_LEN);
        let mut os = restart(&card, &dir);
        let c = [1u8; 100];
        let body = encoded(Command::Chunk {
            offset: 0,
            data: &c,
        });
        assert_eq!(
            call(&mut os, keys::CHUNK, &body, None),
            Err(HandlerError::Unavailable),
            "no begin"
        );
        let img = image(CHUNK_LEN + 100, 9);
        begin(&mut os, &img, V1).unwrap();
        let misaligned = encoded(Command::Chunk {
            offset: 5,
            data: &img[..100],
        });
        assert_eq!(
            call(&mut os, keys::CHUNK, &misaligned, None),
            Err(HandlerError::InvalidRequest)
        );
        let short = encoded(Command::Chunk {
            offset: 0,
            data: &img[..99],
        });
        assert_eq!(
            call(&mut os, keys::CHUNK, &short, None),
            Err(HandlerError::InvalidRequest)
        );
        let first = encoded(Command::Chunk {
            offset: 0,
            data: &img[..CHUNK_LEN],
        });
        call(&mut os, keys::CHUNK, &first, None).unwrap();
        call(&mut os, keys::CHUNK, &first, None).unwrap(); // resent: acknowledged
        // A resent begin resumes; a different one restarts.
        assert_eq!(begin(&mut os, &img, V1), Ok(1));
        // Commit before the last chunk: not yet.
        assert_eq!(
            call(&mut os, keys::COMMIT, &[], Some(&TrustState::Unclaimed)),
            Err(HandlerError::InvalidRequest)
        );
        let last = encoded(Command::Chunk {
            offset: CHUNK_LEN as u32,
            data: &img[CHUNK_LEN..],
        });
        call(&mut os, keys::CHUNK, &last, None).unwrap();
        call(&mut os, keys::COMMIT, &[], Some(&TrustState::Unclaimed)).unwrap();
    }

    #[test]
    fn reboot_needs_a_committed_slot_and_abort_goes_back() {
        let dir = scratch("abort");
        let card = MemDevice::new(4 * CHUNK_LEN);
        let mut os = restart(&card, &dir);
        assert_eq!(
            call(&mut os, keys::REBOOT, &[], Some(&TrustState::Unclaimed)),
            Err(HandlerError::Unavailable)
        );
        let img = image(300, 2);
        begin(&mut os, &img, V1).unwrap();
        send(&mut os, &img).unwrap();
        call(&mut os, keys::COMMIT, &[], Some(&TrustState::Unclaimed)).unwrap();
        call(&mut os, keys::REBOOT, &[], Some(&TrustState::Unclaimed)).unwrap();
        card.boot();
        let mut os = restart(&card, &dir);
        assert_eq!(info(&mut os).state, StateCode::Trial);
        call(&mut os, keys::ABORT, &[], Some(&TrustState::Unclaimed)).unwrap();
        assert_eq!(
            card.0.lock().unwrap().reboots,
            2,
            "the abort reboots into A"
        );
        card.boot();
        let mut os = restart(&card, &dir);
        let i = info(&mut os);
        assert_eq!((i.running, i.state), (Slot::A, StateCode::Idle));
        call(&mut os, keys::ABORT, &[], Some(&TrustState::Unclaimed)).unwrap(); // idempotent
    }

    #[test]
    fn the_record_round_trips_and_a_corrupt_one_is_refused() {
        let r = Record {
            floor: V2,
            versions: [V1, V2],
            ready: Some(Slot::B),
            control: 9,
        };
        assert_eq!(Record::decode(&r.encode()), Ok(r));
        assert_eq!(
            Record::decode(&Record::default().encode()),
            Ok(Record::default())
        );
        let mut bad = r.encode();
        bad[9] ^= 1;
        assert!(Record::decode(&bad).is_err());
        let dir = scratch("record");
        std::fs::write(dir.join(RECORD_FILE), bad).unwrap();
        let card = MemDevice::new(CHUNK_LEN);
        assert!(
            OsUpdates::open(Box::new(card), dir.join(RECORD_FILE), NODE).is_err(),
            "never forget the floor"
        );
    }

    #[test]
    fn the_file_device_writes_the_boot_state_at_the_card_offsets() {
        // The card as files: two slots, the gap sectors, cmdline. The try
        // reaches the second copy's sector (LBA 6144 * 512), the one the
        // first copy at LBA 4096 does not hold.
        let dir = scratch("file");
        let slots = [dir.join("slot-a"), dir.join("slot-b")];
        let state = dir.join("card");
        let cmdline = dir.join("cmdline");
        std::fs::write(&cmdline, "init=/x mw.slot=a quiet\n").unwrap();
        for p in &slots {
            std::fs::write(p, vec![0u8; 3 * CHUNK_LEN]).unwrap();
        }
        let mut card = vec![0u8; (COPY_LBA[1] as usize + 1) * SECTOR];
        card[(COPY_LBA[0] as usize) * SECTOR..][..SECTOR]
            .copy_from_slice(&bootstate::encode(&BootState::initial()));
        std::fs::write(&state, &card).unwrap();

        let dev = FileDevice::open(&slots, &state, &cmdline, Vec::new()).unwrap();
        let mut os = OsUpdates::open(Box::new(dev), dir.join(RECORD_FILE), NODE).unwrap();
        assert_eq!(info(&mut os).running, Slot::A);
        let img = image(CHUNK_LEN + 5, 4);
        begin(&mut os, &img, V1).unwrap();
        send(&mut os, &img).unwrap();
        call(&mut os, keys::COMMIT, &[], Some(&TrustState::Unclaimed)).unwrap();
        call(&mut os, keys::REBOOT, &[], Some(&TrustState::Unclaimed)).unwrap(); // no reboot command: a simulation

        let bytes = std::fs::read(&state).unwrap();
        let copies = [
            &bytes[COPY_LBA[0] as usize * SECTOR..][..SECTOR],
            &bytes[COPY_LBA[1] as usize * SECTOR..][..SECTOR],
        ];
        let (_, s) = bootstate::newest(copies).unwrap();
        assert_eq!((s.try_slot, s.tries, s.active), (Some(Slot::B), 1, Slot::A));
        // The slot B file holds the image, byte for byte.
        let b = std::fs::read(&slots[1]).unwrap();
        assert_eq!(&b[..img.len()], &img[..]);
        // The floor was saved beside the state, and it is the new version.
        let saved = Record::decode(&std::fs::read(dir.join(RECORD_FILE)).unwrap()).unwrap();
        assert_eq!((saved.floor, saved.ready), (V1, None));
    }

    /// The node's side of a link: each request goes to `handle`, a refusal
    /// is the device's status, as the serial link reports it.
    struct NodeLink<'a> {
        os: &'a mut OsUpdates,
        trust: Option<TrustState>,
    }

    impl mwctl::link::Endpoint for NodeLink<'_> {
        fn request(
            &mut self,
            key: middleware::Key<'_>,
            body: &[u8],
        ) -> Result<Vec<u8>, mwctl::error::Error> {
            let mut out = vec![0u8; 4 + CHUNK_LEN];
            let trust = self.trust;
            match self
                .os
                .handle(key, body, trust.as_ref(), &[CHIP], true, &mut out)
            {
                Ok(n) => Ok(out[..n].to_vec()),
                Err(status) => Err(mwctl::error::Error::Device {
                    key: key.as_str().to_owned(),
                    status,
                }),
            }
        }
        fn take_events(&mut self) -> Vec<mwctl::link::Event> {
            Vec::new()
        }
        fn next_event(
            &mut self,
            _deadline: std::time::Instant,
        ) -> Result<Option<mwctl::link::Event>, mwctl::error::Error> {
            Ok(None)
        }
        fn chunk_size(&self) -> usize {
            CHUNK_LEN
        }
    }

    /// mwctl's `os update` and `os confirm` against the node's update
    /// state, over a link: the protocol as the host sees it, end to end
    /// (the cert signed by the pinned root, the trial, the confirm).
    #[test]
    fn mwctl_installs_a_root_on_the_node_and_confirms_the_trial() {
        use middleware::loader::release::SignedCert;
        use middleware::loader::trust::ControlOp;
        use mwctl::device::Device;

        let dir = scratch("mwctl");
        let card = MemDevice::new(4 * CHUNK_LEN);
        let trust = TrustState::Claimed {
            key: root_key(),
            min_version: Version(0),
        };
        let img = image(CHUNK_LEN + 9000, 11);
        let cert_bytes = signed(&SEED, &os_cert(img.len() as u32, sha(&img), V2));
        let cert = SignedCert::decode(&cert_bytes).unwrap();

        let mut os = restart(&card, &dir);
        let counter = std::cell::Cell::new(1u64);
        let sign = |op: ControlOp| -> Result<SignedControl, mwctl::error::Error> {
            let c = counter.get();
            counter.set(c + 1);
            Ok(signed_control_by(&SEED, op, c))
        };
        let signer = mwctl::fw::ControlSigner(&sign);
        {
            let mut dev = Device::new(NodeLink {
                os: &mut os,
                trust: Some(trust),
            });
            let target = dev
                .os_update(&img, Version(V2), Some(cert), |_| {})
                .expect("installed");
            assert_eq!(target, Slot::B);
            dev.os_reboot(Some(signer)).expect("the trial is requested");
        }
        card.boot();
        let mut os = restart(&card, &dir);
        assert_eq!(info(&mut os).state, StateCode::Trial);
        {
            let mut dev = Device::new(NodeLink {
                os: &mut os,
                trust: Some(trust),
            });
            dev.os_confirm(3, || {}, Some(signer)).expect("confirmed");
            let i = dev.os_info().expect("info");
            assert_eq!(
                (i.running, i.state, i.running_version),
                (Slot::B, StateCode::Confirmed, V2)
            );
        }
        // A root without the cert: the claimed node refuses the commit (the
        // image is sent, the commit is the step that fails).
        let mut os = restart(&card, &dir);
        let mut dev = Device::new(NodeLink {
            os: &mut os,
            trust: Some(trust),
        });
        let err = dev
            .os_update(&img, Version(V2 + 1), None, |_| {})
            .unwrap_err();
        match err {
            mwctl::error::Error::Upload { step: 3, cause, .. } => assert!(
                matches!(
                    *cause,
                    mwctl::error::Error::Device {
                        status: HandlerError::Denied,
                        ..
                    }
                ),
                "{cause:?}"
            ),
            other => panic!("{other:?}"),
        }
    }

    /// Over the network a claimed node takes a reboot, confirm or abort only
    /// signed by its pinned key, with a counter above the last one taken
    /// (persisted); over the local serial port it takes them unsigned.
    #[test]
    fn network_control_needs_the_pinned_key_and_a_new_counter() {
        let dir = scratch("control");
        let card = MemDevice::new(4 * CHUNK_LEN);
        let mut os = restart(&card, &dir);
        let img = image(300, 12);
        begin(&mut os, &img, V1).unwrap();
        send(&mut os, &img).unwrap();
        call(&mut os, keys::COMMIT, &[], Some(&TrustState::Unclaimed)).unwrap();
        let trust = TrustState::Claimed {
            key: root_key(),
            min_version: Version(0),
        };
        let denied = Err(HandlerError::Denied);
        let reboot = |os: &mut OsUpdates, c: Option<SignedControl>| {
            let body = c.map(|c| c.encode().to_vec()).unwrap_or_default();
            call(os, keys::REBOOT, &body, Some(&trust))
        };
        // Unsigned, and signed by another key: refused, nothing rebooted.
        assert_eq!(reboot(&mut os, None), denied);
        assert_eq!(
            reboot(
                &mut os,
                Some(signed_control_by(&[9; 32], ControlOp::OsReboot, 1))
            ),
            denied
        );
        assert_eq!(card.0.lock().unwrap().reboots, 0);
        // Signed by the pinned key for this op: taken, counter kept.
        assert_eq!(
            reboot(
                &mut os,
                Some(signed_control_by(&SEED, ControlOp::OsReboot, 1))
            ),
            Ok(vec![])
        );
        assert_eq!(card.0.lock().unwrap().reboots, 1);
        assert_eq!(os.record().control, 1);
        // The same request again is a replay; a signature for another op is
        // not this one either.
        assert_eq!(
            reboot(
                &mut os,
                Some(signed_control_by(&SEED, ControlOp::OsReboot, 1))
            ),
            denied
        );
        assert_eq!(
            reboot(
                &mut os,
                Some(signed_control_by(&SEED, ControlOp::OsAbort, 2))
            ),
            denied
        );
        // Over the serial port the board is in reach: no signature needed.
        let mut out = [0u8; 8];
        assert!(
            os.handle(keys::ABORT, &[], Some(&trust), &[CHIP], false, &mut out)
                .is_ok()
        );
        assert_eq!(os.record().control, 1, "serial takes no counter");
    }

    /// The node's control target is the one `mwctl` signs for (its zlink
    /// derivation), so a request signed for the node is for this node.
    #[test]
    fn control_target_is_the_one_mwctl_signs_for() {
        for id in [
            NODE,
            SenderId([1; 8]),
            SenderId([0xff, 0, 1, 2, 3, 4, 5, 6]),
        ] {
            assert_eq!(control_target(id), mwctl::zlink::target(id));
        }
    }
}
