//! Requests to a device over ESP-NOW, through the USB↔ESP-NOW bridge.
//!
//! [`AirLink`] is an [`Endpoint`], so every [`crate::device::Device`]
//! operation (`info`, `trust info`, `upload`, `start`, `stop`) runs over
//! the air unchanged. It speaks the bridge framing ([`crate::bridge`]) on
//! the serial [`Transport`] and the request/response frames of
//! [`middleware::rpc`] inside it:
//!
//! * Before the first request it sends `Query` and waits
//!   ([`AirTiming::hello`]) for the bridge's `Hello`, which names the
//!   bridge's MAC: responses must be addressed to it.
//! * Each request gets a fresh 16-bit id; it is sent to the target MAC as a
//!   bridge `Send`. Every retransmission reuses the id
//!   ([`middleware::rpc::Call`]), so the device answers a repeat from its
//!   cache instead of running `commit` or `start` twice. An attempt ends
//!   when its timeout passes ([`AirTiming::request`], or
//!   [`AirTiming::slow`] for `begin`, `commit` and `start`, which erase,
//!   write or load), or [`AirTiming::nack_wait`] after the bridge reports
//!   the unicast send unacknowledged (`Sent` failed) — the frame may still
//!   have arrived (only the acknowledgement lost), so a short wait for the
//!   answer comes first. After [`AirTiming::attempts`] sends the request
//!   fails with [`Error::Timeout`].
//! * A response is accepted only with the call's id, from the target, to
//!   the bridge ([`middleware::rpc::Call::answers`]); everything else heard
//!   (stale responses, ESP-NOW publications) is counted and skipped.
//! * Uploads use [`middleware::rpc::CHUNK_DATA`]-byte chunks, so every
//!   request fits one ESP-NOW frame.
//! * Bodies pass through unchanged: a signed `start`/`stop`
//!   ([`middleware::loader::trust::SignedControl`], 72 bytes) is built by
//!   the caller ([`crate::sign::sign_control`]) for this link's target.
//!
//! [`AirStats`] counts requests, sends, retransmissions and
//! unacknowledged sends for the throughput report.

use crate::bridge::{self, Mac, Msg};
use crate::error::Error;
use crate::link::{Endpoint, Event, Transport};
use middleware::Key;
use middleware::loader::keys;
use middleware::rpc::{self, Call, Message};
use middleware::serial::frame::{Split, Splitter};
use std::time::{Duration, Instant};

/// Timing of an [`AirLink`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AirTiming {
    /// Per-attempt timeout of an ordinary request.
    pub request: Duration,
    /// Per-attempt timeout of `begin` (erases), `commit` (writes and
    /// verifies) and `start` (loads the payload).
    pub slow: Duration,
    /// After an unacknowledged send, how long to wait for an answer before
    /// resending.
    pub nack_wait: Duration,
    /// Sends per request, including the first.
    pub attempts: u8,
    /// How long to wait for the bridge's `Hello`.
    pub hello: Duration,
}

impl Default for AirTiming {
    fn default() -> Self {
        AirTiming {
            request: Duration::from_millis(300),
            slow: Duration::from_millis(5000),
            nack_wait: Duration::from_millis(30),
            attempts: 10,
            hello: Duration::from_millis(2000),
        }
    }
}

impl AirTiming {
    /// The per-attempt timeout for a request on `key`.
    pub fn for_key(&self, key: Key<'_>) -> Duration {
        if key == keys::BEGIN || key == keys::COMMIT || key == keys::START {
            self.slow.max(self.request)
        } else {
            self.request
        }
    }
}

/// Counters of an [`AirLink`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AirStats {
    /// Requests made.
    pub requests: u32,
    /// Frames sent (requests plus retransmissions).
    pub sends: u32,
    /// Retransmissions after a timeout or an unacknowledged send.
    pub retransmissions: u32,
    /// Sends the bridge reported unacknowledged.
    pub nacks: u32,
    /// Frames heard that were not the awaited answer.
    pub skipped: u32,
}

const RX_CHUNK: usize = 512;

/// Longest delimited run kept: a boot banner glued to a maximal frame.
const MAX_RUN: usize = 4 * bridge::MAX_WIRE;

/// Host endpoint of a device reached over ESP-NOW through the bridge.
pub struct AirLink<T> {
    io: T,
    splitter: Splitter<MAX_RUN>,
    rx: [u8; RX_CHUNK],
    rx_pos: usize,
    rx_len: usize,
    target: Mac,
    bridge: Option<Mac>,
    next_id: u16,
    timing: AirTiming,
    stats: AirStats,
    events: Vec<Event>,
    synced: bool,
}

impl<T: Transport> AirLink<T> {
    /// An endpoint for the device with MAC `target`, through the bridge on
    /// `io`; the first request id is `first_id` (pick it at random or from
    /// the clock, so a restarted host does not repeat recent ids).
    pub fn new(io: T, target: Mac, timing: AirTiming, first_id: u16) -> Self {
        AirLink {
            io,
            splitter: Splitter::new(),
            rx: [0; RX_CHUNK],
            rx_pos: 0,
            rx_len: 0,
            target,
            bridge: None,
            next_id: first_id,
            timing,
            stats: AirStats::default(),
            events: Vec::new(),
            synced: false,
        }
    }

    /// The bridge transport.
    pub fn transport(&self) -> &T {
        &self.io
    }

    /// Counters so far.
    pub fn stats(&self) -> AirStats {
        self.stats
    }

    /// The bridge's MAC, once its `Hello` was seen.
    pub fn bridge_mac(&self) -> Option<Mac> {
        self.bridge
    }

    fn write(&mut self, msg: &Msg) -> Result<(), Error> {
        let wire = msg
            .wire()
            .map_err(|e| Error::Bridge(format!("cannot frame {msg:?}: {e:?}")))?;
        if !self.synced {
            self.io.write_all(&[0])?;
            self.synced = true;
        }
        self.io.write_all(&wire)?;
        Ok(())
    }

    /// Waits until `deadline` for the next bridge message; undecodable runs
    /// are queued as [`Event::Noise`]. A `Hello` updates the bridge's MAC.
    pub fn poll(&mut self, deadline: Instant) -> Result<Option<Msg>, Error> {
        loop {
            if self.rx_pos == self.rx_len {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Ok(None);
                }
                let n = self.io.read(&mut self.rx, left)?;
                if n == 0 {
                    return Ok(None);
                }
                self.rx_pos = 0;
                self.rx_len = n.min(RX_CHUNK);
            }
            // Bounded by the bytes already read.
            while self.rx_pos < self.rx_len {
                let byte = self.rx.get(self.rx_pos).copied().unwrap_or(0);
                self.rx_pos += 1;
                match self.splitter.push(byte) {
                    Split::Pending => {}
                    Split::Overflow => self.events.push(Event::Noise(Vec::new())),
                    Split::Frame(_) => {
                        let run = self.splitter.frame();
                        match bridge::decode_run(run) {
                            Some((skip, msg)) => {
                                if skip > 0 {
                                    let garbage = run.get(..skip).unwrap_or_default().to_vec();
                                    self.events.push(Event::Noise(garbage));
                                }
                                if let Msg::Hello { mac, .. } = msg {
                                    self.bridge = Some(mac);
                                }
                                return Ok(Some(msg));
                            }
                            None => self.events.push(Event::Noise(run.to_vec())),
                        }
                    }
                }
            }
        }
    }

    /// Asks the bridge who it is; returns its `Hello` and the `Stats` that
    /// follow (when they arrive in time).
    pub fn query(&mut self) -> Result<(Msg, Option<bridge::Stats>), Error> {
        self.write(&Msg::Query)?;
        let deadline = Instant::now() + self.timing.hello;
        let mut hello = None;
        // Each iteration consumes a message or ends at the deadline.
        while let Some(msg) = self.poll(deadline)? {
            match msg {
                Msg::Hello { .. } => hello = Some(msg),
                Msg::Stats(s) if hello.is_some() => {
                    return Ok((hello.unwrap_or(Msg::Query), Some(s)));
                }
                _ => {}
            }
        }
        hello.map(|h| (h, None)).ok_or_else(|| {
            Error::Bridge(format!(
                "no Hello within {} ms (is the bridge firmware flashed, at {} baud?)",
                self.timing.hello.as_millis(),
                bridge::BAUD
            ))
        })
    }

    fn bridge_mac_or_query(&mut self) -> Result<Mac, Error> {
        if let Some(m) = self.bridge {
            return Ok(m);
        }
        self.query()?;
        self.bridge
            .ok_or_else(|| Error::Bridge("Hello without a MAC".into()))
    }
}

impl<T: Transport> Endpoint for AirLink<T> {
    fn request(&mut self, key: Key<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
        let own = self.bridge_mac_or_query()?;
        let id = self.next_id;
        self.next_id = rpc::next_id(id);
        self.stats.requests = self.stats.requests.saturating_add(1);
        let mut frame = [0u8; rpc::MAX_FRAME];
        let n = Message::Request {
            id,
            to: self.target,
            key,
            body,
        }
        .encode(&mut frame)
        .map_err(|e| {
            Error::Bridge(format!(
                "{}: request does not fit one ESP-NOW frame: {e:?}",
                key.as_str()
            ))
        })?;
        let send = Msg::Send {
            dst: self.target,
            data: frame.get(..n).unwrap_or_default().to_vec(),
        };
        let timeout = self.timing.for_key(key);
        let mut call = Call::first(id, self.target, self.timing.attempts);
        loop {
            self.write(&send)?;
            self.stats.sends = self.stats.sends.saturating_add(1);
            let mut deadline = Instant::now() + timeout;
            // Each iteration consumes a message or ends at the deadline.
            while let Some(msg) = self.poll(deadline)? {
                match msg {
                    Msg::Recv { src, data } => {
                        let answer = Message::decode(&data)
                            .ok()
                            .and_then(|m| call.answers(Some(own), src, &m));
                        match answer {
                            Some(Ok(body)) => return Ok(body.to_vec()),
                            Some(Err(status)) => {
                                return Err(Error::Device {
                                    key: key.as_str().to_owned(),
                                    status,
                                });
                            }
                            None => self.stats.skipped = self.stats.skipped.saturating_add(1),
                        }
                    }
                    Msg::Sent { ok: false, dst } if dst == self.target => {
                        self.stats.nacks = self.stats.nacks.saturating_add(1);
                        deadline = deadline.min(Instant::now() + self.timing.nack_wait);
                    }
                    _ => {}
                }
            }
            call = call.retry().ok_or_else(|| Error::Timeout {
                key: key.as_str().to_owned(),
                attempts: u32::from(call.attempts()),
            })?;
            self.stats.retransmissions = self.stats.retransmissions.saturating_add(1);
        }
    }

    fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    /// Only noise runs are events over the air; the device's `sys/log` and
    /// status stay on its serial ports.
    fn next_event(&mut self, deadline: Instant) -> Result<Option<Event>, Error> {
        loop {
            if !self.events.is_empty() {
                return Ok(Some(self.events.remove(0)));
            }
            if self.poll(deadline)?.is_none() {
                return Ok(if self.events.is_empty() {
                    None
                } else {
                    Some(self.events.remove(0))
                });
            }
        }
    }

    fn chunk_size(&self) -> usize {
        rpc::CHUNK_DATA
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::tests::BRIDGE;
    use crate::device::{Device, UploadOptions};
    use crate::sign::sign_control;
    use crate::sign::tests::vector_seed;
    use middleware::crc::Crc32;
    use middleware::loader::trust::{ControlCounter, ControlOp};
    use middleware::loader::{Command, DeviceInfo, Effect, Loader, PayloadStatus};
    use middleware::rpc::{Admit, Replies};
    use middleware::transport::HandlerError;
    use std::collections::VecDeque;
    use std::io;

    const DEV: Mac = [0xe4, 0xb0, 0x63, 0x81, 0xd3, 0x24];

    /// What happens to the next `Send` the host makes.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Fate {
        Deliver,
        /// The request never reaches the device (and is not acknowledged).
        LoseRequest,
        /// The device runs it; its response is lost.
        LoseResponse,
        /// The device gets it but the acknowledgement is lost (`Sent`
        /// failed); its response arrives.
        LostAck,
    }

    /// The bridge plus an over-the-air device running the real loader and
    /// the real responder cache, answering synchronously. Like a claimed
    /// device it takes `start`/`stop` only with a control request signed
    /// by the test key for [`DEV`] and a counter above the last.
    struct FakeAir {
        control: u64,
        rx: VecDeque<u8>,
        script: VecDeque<Fate>,
        replies: Replies<8, 128>,
        loader: Loader,
        installed: Option<Vec<u8>>,
        image: Vec<u8>,
        status: PayloadStatus,
        runs: Vec<String>,
        hello: bool,
        splitter: Splitter<MAX_RUN>,
    }

    impl FakeAir {
        fn new(script: &[Fate]) -> Self {
            FakeAir {
                control: 0,
                rx: VecDeque::new(),
                script: script.iter().copied().collect(),
                replies: Replies::new(),
                loader: Loader::Idle,
                installed: None,
                image: Vec::new(),
                status: PayloadStatus::Empty,
                runs: Vec::new(),
                hello: true,
                splitter: Splitter::new(),
            }
        }

        fn emit(&mut self, msg: Msg) {
            self.rx.extend(msg.wire().unwrap());
        }

        fn handle(
            &mut self,
            key: Key<'_>,
            body: &[u8],
            out: &mut [u8],
        ) -> Result<usize, HandlerError> {
            self.runs.push(key.as_str().to_owned());
            let body = if key == keys::START || key == keys::STOP {
                self.check_control(key, body)?;
                &[][..]
            } else {
                body
            };
            let cmd = Command::parse(key, body)?.ok_or(HandlerError::NotFound)?;
            match cmd {
                Command::Info => DeviceInfo {
                    abi_version: 4,
                    capacity: 4096,
                    status: self.status,
                    chip: "esp32s3",
                    firmware: "0.1.0",
                }
                .encode(out)
                .ok_or(HandlerError::BufferTooSmall),
                Command::Start => {
                    self.status = PayloadStatus::Running;
                    Ok(0)
                }
                Command::Stop => {
                    self.status = PayloadStatus::Stopped;
                    Ok(0)
                }
                Command::TrustClaim { .. } => Err(HandlerError::Denied),
                _ => {
                    let (next, effect) = self.loader.step(&cmd, 4096);
                    self.loader = next;
                    match effect? {
                        Effect::Erase { .. } => self.image.clear(),
                        Effect::Write { data, .. } => self.image.extend_from_slice(data),
                        Effect::Activate { .. } => {
                            self.installed = Some(self.image.clone());
                            self.status = PayloadStatus::Stopped;
                        }
                        Effect::None => {}
                    }
                    Ok(0)
                }
            }
        }

        fn check_control(&mut self, key: Key<'_>, body: &[u8]) -> Result<(), HandlerError> {
            use ed25519_dalek::{Verifier, VerifyingKey};
            use middleware::loader::trust::{ControlOp, SignedControl, control_message};
            let auth = SignedControl::parse_body(body)?.ok_or(HandlerError::Denied)?;
            let op = if key == keys::START {
                ControlOp::Start
            } else {
                ControlOp::Stop
            };
            let vk = VerifyingKey::from_bytes(&vector_seed().public_key().0).unwrap();
            let m = control_message(op, &DEV, auth.counter);
            let sig = ed25519_dalek::Signature::from_bytes(&auth.signature.0);
            if auth.counter.0 <= self.control || vk.verify(&m, &sig).is_err() {
                return Err(HandlerError::Denied);
            }
            self.control = auth.counter.0;
            Ok(())
        }

        fn air(&mut self, dst: Mac, data: &[u8]) {
            let fate = self.script.pop_front().unwrap_or(Fate::Deliver);
            if fate == Fate::LoseRequest {
                self.emit(Msg::Sent { ok: false, dst });
                return;
            }
            self.emit(Msg::Sent {
                ok: fate != Fate::LostAck,
                dst,
            });
            let Ok(Message::Request { id, to, key, body }) = Message::decode(data) else {
                return;
            };
            if to != DEV || dst != DEV {
                return;
            }
            let (replies, admit) = self.replies.admit(BRIDGE, id, 0);
            self.replies = replies;
            let mut out = [0u8; 128];
            let result = match admit {
                Admit::Busy => return,
                Admit::Replay => {
                    let cached = self.replies.cached(BRIDGE, id).unwrap();
                    cached.map(|b| {
                        out[..b.len()].copy_from_slice(b);
                        b.len()
                    })
                }
                Admit::Execute => {
                    let r = self.handle(key, body, &mut out);
                    self.replies = self.replies.complete(BRIDGE, id, r.map(|n| &out[..n]));
                    r
                }
            };
            if fate == Fate::LoseResponse {
                return;
            }
            let mut frame = [0u8; rpc::MAX_FRAME];
            let n = Message::Response {
                id,
                to: BRIDGE,
                result: result.map(|n| &out[..n]),
            }
            .encode(&mut frame)
            .unwrap();
            self.emit(Msg::Recv {
                src: DEV,
                data: frame[..n].to_vec(),
            });
        }
    }

    impl Transport for FakeAir {
        fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
            for &b in bytes {
                if let Split::Frame(_) = self.splitter.push(b) {
                    match Msg::from_run(self.splitter.frame()) {
                        Ok(Msg::Query) if self.hello => {
                            self.emit(Msg::Hello {
                                mac: BRIDGE,
                                channel: 1,
                                version: 1,
                            });
                            self.emit(Msg::Stats(bridge::Stats::default()));
                        }
                        Ok(Msg::Send { dst, data }) => self.air(dst, &data),
                        _ => {}
                    }
                }
            }
            Ok(())
        }

        /// Answers are queued synchronously, so an empty queue is a timeout.
        fn read(&mut self, buf: &mut [u8], _timeout: Duration) -> io::Result<usize> {
            let n = buf.len().min(self.rx.len());
            for (dst, src) in buf.iter_mut().zip(self.rx.drain(..n)) {
                *dst = src;
            }
            Ok(n)
        }
    }

    fn device(fake: FakeAir) -> Device<AirLink<FakeAir>> {
        Device::new(AirLink::new(fake, DEV, AirTiming::default(), 500))
    }

    #[test]
    fn o9_info_over_the_air_learns_the_bridge_first() {
        let mut d = device(FakeAir::new(&[]));
        let i = d.info().unwrap();
        assert_eq!(
            (i.chip.as_str(), i.status),
            ("esp32s3", PayloadStatus::Empty)
        );
        assert_eq!(d.link().bridge_mac(), Some(BRIDGE));
        assert_eq!(
            d.link().stats(),
            AirStats {
                requests: 1,
                sends: 1,
                retransmissions: 0,
                nacks: 0,
                skipped: 0
            }
        );
        let mut silent = FakeAir::new(&[]);
        silent.hello = false;
        assert!(matches!(device(silent).info(), Err(Error::Bridge(_))));
    }

    #[test]
    fn o9_upload_over_the_air_survives_loss_and_never_runs_a_step_twice() {
        let img: Vec<u8> = (0..1000u32).map(|i| (i * 7 + 3) as u8).collect();
        let script = [
            Fate::LoseRequest,  // begin lost: resent
            Fate::Deliver,      // begin
            Fate::LoseResponse, // chunk 0 runs, its answer is lost: replayed
            Fate::Deliver,
            Fate::LostAck, // chunk 1 arrives, acknowledgement lost
            Fate::Deliver,
            Fate::Deliver,
            Fate::Deliver,
            Fate::LoseResponse, // commit runs once; the resend is replayed
            Fate::Deliver,
            Fate::LoseResponse, // start runs once
        ];
        let mut d = device(FakeAir::new(&script));
        let start = sign_control(&vector_seed(), ControlOp::Start, &DEV, ControlCounter(1));
        d.upload(
            &img,
            UploadOptions {
                start: true,
                start_auth: Some(start),
                ..UploadOptions::default()
            },
            |_| {},
        )
        .unwrap();
        let fake = d.link().transport();
        assert_eq!(fake.installed.as_deref(), Some(img.as_slice()));
        assert_eq!(fake.status, PayloadStatus::Running);
        let count = |k: &str| fake.runs.iter().filter(|r| *r == k).count();
        assert_eq!(count("sys/loader/begin"), 1);
        assert_eq!(
            count("sys/loader/chunk"),
            5,
            "1000 bytes in 217-byte chunks, each run once"
        );
        assert_eq!(count("sys/loader/commit"), 1);
        assert_eq!(count("sys/payload/start"), 1);
        let s = d.link().stats();
        assert_eq!(s.requests, 8);
        assert_eq!(s.retransmissions, 4);
        assert_eq!(s.sends, 12);
        assert_eq!(s.nacks, 2);
        assert_eq!(
            Crc32::checksum(&img),
            Crc32::checksum(fake.installed.as_ref().unwrap())
        );
    }

    #[test]
    fn o9_refusals_are_device_errors_and_attempts_are_bounded() {
        let mut d = device(FakeAir::new(&[]));
        let e = d
            .trust_claim(middleware::loader::trust::PublicKey([1; 32]))
            .unwrap_err();
        assert_eq!(e.status(), Some(HandlerError::Denied));
        assert!(e.to_string().contains("never"), "{e}");
        // Unsigned, replayed, for another device or the other command.
        let signed =
            |op, target: &Mac, n| Some(sign_control(&vector_seed(), op, target, ControlCounter(n)));
        let denied = |r: Result<(), Error>| r.unwrap_err().status() == Some(HandlerError::Denied);
        assert!(denied(d.stop()));
        assert!(denied(d.stop_signed(signed(ControlOp::Start, &DEV, 5))));
        assert!(denied(d.stop_signed(signed(ControlOp::Stop, &BRIDGE, 5))));
        d.stop_signed(signed(ControlOp::Stop, &DEV, 5)).unwrap();
        let e = d
            .start_signed(signed(ControlOp::Start, &DEV, 5))
            .unwrap_err();
        assert!(e.to_string().contains("counter"), "{e}");
        d.start_signed(signed(ControlOp::Start, &DEV, 6)).unwrap();
        assert_eq!(d.link().transport().status, PayloadStatus::Running);
        let mut d = device(FakeAir::new(&[Fate::LoseRequest; 10]));
        match d.stop() {
            Err(Error::Timeout { key, attempts }) => {
                assert_eq!((key.as_str(), attempts), ("sys/payload/stop", 10))
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(d.link().stats().nacks, 10);
        // Too large for one frame: refused before sending.
        let mut link = AirLink::new(FakeAir::new(&[]), DEV, AirTiming::default(), 1);
        assert!(matches!(
            link.request(keys::INFO, &[0; 300]),
            Err(Error::Bridge(_))
        ));
        assert_eq!(link.chunk_size(), 217);
        assert_eq!(
            AirTiming::default().for_key(keys::COMMIT),
            Duration::from_secs(5)
        );
        assert_eq!(
            AirTiming::default().for_key(keys::CHUNK),
            Duration::from_millis(300)
        );
    }
}
