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
//! version above `min_version`, else `Denied`. The check uses
//! `ed25519-dalek` directly, because mwctl builds `middleware` with no
//! features (the crate's own check needs `trust`).
//!
//! (`esp_base` itself is not used: it requires rustc 1.95 through its own
//! toolchain file, which the host workspace does not use.)
//!
//! A [`Script`] injects faults by request ordinal (1-based, counting every
//! request frame received): drop the request, drop its reply, or hold the
//! reply back until the next reply is sent. A read with nothing queued
//! returns `Ok(0)` at once, which the link treats as a timeout.

use crate::link::Transport;
use crate::sign::image_sha256;
use ed25519_dalek::{Signature, VerifyingKey};
use middleware::Key;
use middleware::loader::trust::{SignedCommit, TrustState, Version, signed_message};
use middleware::loader::{Command, DeviceInfo, Effect, Loader, PayloadStatus, keys};
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
}

pub struct FakeDevice {
    chip: &'static str,
    firmware: &'static str,
    capacity: u32,
    runtime: bool,
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
}

impl FakeDevice {
    pub fn new(chip: &'static str, firmware: &'static str, capacity: u32, runtime: bool) -> Self {
        FakeDevice {
            chip,
            firmware,
            capacity,
            runtime,
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
        let vk = VerifyingKey::from_bytes(&key.0).map_err(|_| HandlerError::Denied)?;
        let len = u32::try_from(image.len()).map_err(|_| HandlerError::Denied)?;
        let m = signed_message(len, auth.version, &image_sha256(image));
        vk.verify_strict(&m, &Signature::from_bytes(&auth.signature.0))
            .map_err(|_| HandlerError::Denied)?;
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
        let cmd = Command::parse(key, body)?.ok_or(HandlerError::NotFound)?;
        match cmd {
            Command::Info => DeviceInfo {
                abi_version: ABI_VERSION,
                capacity: self.capacity,
                status: self.status,
                chip: self.chip,
                firmware: self.firmware,
                node: Some(NODE),
            }
            .encode(out)
            .ok_or(HandlerError::BufferTooSmall),
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
                if VerifyingKey::from_bytes(&key.0).is_err() {
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

    fn on_frame(&mut self, cobs: &mut [u8]) {
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
}

impl Transport for FakeDevice {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
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
        let n = buf.len().min(self.tx.len());
        for (dst, src) in buf.iter_mut().zip(self.tx.drain(..n)) {
            *dst = src;
        }
        Ok(n)
    }
}
