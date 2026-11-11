//! Loader helpers for the browser: upload plans, `sys/info` decoding and a
//! device-side loader for fakes and tests.
//!
//! * [`plan`] turns an image into the ordered request sequence
//!   `sys/loader/begin`, `sys/loader/chunk` × n, `sys/loader/commit`, with
//!   bodies from [`Command::encode_body`] and the CRC-32 from [`Crc32`].
//! * [`Info`] is an owned [`DeviceInfo`].
//! * [`DeviceLoader`] runs the middleware's own [`Loader`] state machine, so
//!   a scripted fake device in the JS tests enforces exactly the ordering,
//!   length and CRC rules the firmware does. It also holds a simulated root
//!   of trust ([`TrustState`] and re-key [`Window`] on a JS-set clock) and
//!   answers `sys/trust/info`, `sys/trust/claim` and signed commits with
//!   the middleware's own [`TrustState::claim`] and
//!   [`TrustState::accept_commit`]; a refused commit is `Denied` and never
//!   reaches `Activate`, as on the device.
//! * [`signed_message`], [`sha256`], [`signed_commit_body`] and
//!   [`trust_state_name`] expose the signed-payload wire helpers.

use middleware::crc::Crc32;
use middleware::loader::sha256::ImageHash;
use middleware::loader::trust::{
    self, INFO_MAX, Signature, SignedCommit, TrustCode, TrustInfo, TrustState, Version, Window,
};
use middleware::loader::{CHUNK_MAX, Command, DeviceInfo, Effect, Loader, PayloadStatus};
use middleware::{HandlerError, Key};

/// One request of an upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// Key to send the request on.
    pub key: &'static str,
    /// Request body.
    pub body: Vec<u8>,
    /// Image bytes acknowledged once this step succeeds (for progress).
    pub done: u32,
}

/// Why an image cannot be uploaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanError {
    /// The image is empty.
    Empty,
    /// The image does not fit the `u32` length field.
    TooLarge,
}

impl PlanError {
    /// Short human-readable description.
    pub const fn describe(self) -> &'static str {
        match self {
            PlanError::Empty => "image is empty",
            PlanError::TooLarge => "image too large",
        }
    }
}

fn body(cmd: &Command<'_>) -> Vec<u8> {
    let mut out = vec![0u8; 8 + CHUNK_MAX];
    let n = cmd.encode_body(&mut out).unwrap_or(0);
    out.truncate(n);
    out
}

/// The CRC-32 the loader verifies at commit.
pub fn image_crc32(image: &[u8]) -> u32 {
    Crc32::checksum(image)
}

/// The full request sequence for installing `image`, chunks of at most
/// `chunk` bytes (clamped to `1..=CHUNK_MAX`), with an unsigned commit.
pub fn plan(image: &[u8], chunk: usize) -> Result<Vec<Step>, PlanError> {
    plan_with_commit(image, chunk, &[])
}

/// As [`plan`], with `commit` as the `sys/loader/commit` body (empty, or a
/// 68-byte [`signed_commit_body`]); the body is sent as given.
pub fn plan_with_commit(image: &[u8], chunk: usize, commit: &[u8]) -> Result<Vec<Step>, PlanError> {
    if image.is_empty() {
        return Err(PlanError::Empty);
    }
    let len = u32::try_from(image.len()).map_err(|_| PlanError::TooLarge)?;
    let chunk = chunk.clamp(1, CHUNK_MAX);
    let begin = Command::Begin {
        len,
        crc: image_crc32(image),
    };
    let mut steps = vec![Step {
        key: begin.key().as_str(),
        body: body(&begin),
        done: 0,
    }];
    let mut offset = 0u32;
    for data in image.chunks(chunk) {
        let cmd = Command::Chunk { offset, data };
        // `data.len() <= CHUNK_MAX` and the sum is bounded by `len`.
        offset += data.len() as u32;
        steps.push(Step {
            key: cmd.key().as_str(),
            body: body(&cmd),
            done: offset,
        });
    }
    steps.push(Step {
        key: Command::Commit { auth: None }.key().as_str(),
        body: commit.to_vec(),
        done: len,
    });
    Ok(steps)
}

/// SHA-256 of `data` (the digest a payload signature covers).
pub fn sha256(data: &[u8]) -> [u8; 32] {
    ImageHash::digest(data)
}

/// The 54-byte message an Ed25519 payload signature covers:
/// `b"mw-payload-v1\0" || len u32 LE || version u32 LE || sha256`.
/// `None` when `digest` is not 32 bytes.
pub fn signed_message(len: u32, version: u32, digest: &[u8]) -> Option<Vec<u8>> {
    let digest = <[u8; 32]>::try_from(digest).ok()?;
    Some(trust::signed_message(len, Version(version), &digest).to_vec())
}

/// The signed `sys/loader/commit` body `[version u32 LE][signature 64]`;
/// `None` when `signature` is not 64 bytes.
pub fn signed_commit_body(version: u32, signature: &[u8]) -> Option<Vec<u8>> {
    let signature = Signature(<[u8; 64]>::try_from(signature).ok()?);
    Some(
        SignedCommit {
            version: Version(version),
            signature,
        }
        .encode()
        .to_vec(),
    )
}

/// Name of a `sys/trust/info` state byte.
pub const fn trust_state_name(b: u8) -> Option<&'static str> {
    match TrustCode::from_byte(b) {
        Some(TrustCode::Unclaimed) => Some("unclaimed"),
        Some(TrustCode::Claimed) => Some("claimed"),
        Some(TrustCode::RekeyOpen) => Some("rekey"),
        None => None,
    }
}

/// Name of a payload status byte.
pub const fn status_name(b: u8) -> Option<&'static str> {
    match PayloadStatus::from_byte(b) {
        Some(PayloadStatus::Empty) => Some("empty"),
        Some(PayloadStatus::Stopped) => Some("stopped"),
        Some(PayloadStatus::Running) => Some("running"),
        Some(PayloadStatus::Trapped) => Some("trapped"),
        Some(PayloadStatus::Loading) => Some("loading"),
        None => None,
    }
}

/// Owned `sys/info` response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    /// ABI version implemented by the firmware.
    pub abi_version: u16,
    /// Largest payload image in bytes.
    pub capacity: u32,
    /// Payload status byte.
    pub status: u8,
    /// Chip name.
    pub chip: String,
    /// Firmware version.
    pub firmware: String,
    /// Node id (8 bytes), reported by middleware 0.5 firmware.
    pub node: Option<[u8; 8]>,
}

impl Info {
    /// Decodes a `sys/info` response body.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let d = DeviceInfo::decode(bytes)?;
        Some(Info {
            abi_version: d.abi_version,
            capacity: d.capacity,
            status: d.status as u8,
            chip: d.chip.to_owned(),
            firmware: d.firmware.to_owned(),
            node: d.node.map(|n| n.0),
        })
    }

    /// Encodes as a `sys/info` response body; `None` for an unknown status
    /// byte or over-long names.
    pub fn encode(&self) -> Option<Vec<u8>> {
        let info = DeviceInfo {
            abi_version: self.abi_version,
            capacity: self.capacity,
            status: PayloadStatus::from_byte(self.status)?,
            chip: &self.chip,
            firmware: &self.firmware,
            node: self.node.map(middleware::SenderId),
        };
        let mut out = vec![0u8; middleware::loader::MAX_INFO_LEN];
        let n = info.encode(&mut out)?;
        out.truncate(n);
        Some(out)
    }
}

/// What a [`DeviceLoader::step`] asks the device to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Not a system key: the application handles it.
    App,
    /// `sys/info`.
    Info,
    /// `sys/payload/start`.
    Start,
    /// `sys/payload/stop`.
    Stop,
    /// Erase storage for `len` bytes.
    Erase(u32),
    /// Write bytes at an offset.
    Write(u32, Vec<u8>),
    /// The image is complete, its CRC verified and (when claimed) its
    /// signature accepted: `(len, crc)`.
    Activate(u32, u32),
    /// `sys/trust/info`: the encoded [`TrustInfo`] response body.
    TrustInfo(Vec<u8>),
    /// `sys/trust/claim` accepted: the key is pinned.
    Claim,
}

/// A device-side loader with a storage capacity and a simulated root of
/// trust, for fakes and tests. It starts Unclaimed (every commit accepted)
/// with the re-key window closed and the clock at 0 ms.
///
/// [`DeviceLoader::step`] takes the loader by value and returns its
/// successor, matching [`Loader::step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceLoader {
    state: Loader,
    capacity: u32,
    trust: TrustState,
    window: Window,
    now_ms: u64,
}

impl DeviceLoader {
    /// An idle, unclaimed loader accepting images up to `capacity` bytes.
    pub const fn new(capacity: u32) -> Self {
        Self {
            state: Loader::Idle,
            capacity,
            trust: TrustState::Unclaimed,
            window: Window::Closed,
            now_ms: 0,
        }
    }

    /// The same loader with its clock (ms since boot) at `now_ms`; the
    /// re-key window expires against it.
    pub const fn at(self, now_ms: u64) -> Self {
        Self {
            now_ms,
            window: self.window.expire(now_ms),
            ..self
        }
    }

    /// Physical presence asserted now (the BOOT button held on a device):
    /// opens the re-key window for [`trust::REKEY_WINDOW_MS`].
    pub const fn assert_presence(self) -> Self {
        Self {
            window: Window::opened_at(self.now_ms),
            ..self
        }
    }

    /// The simulated persisted trust state.
    pub const fn trust(&self) -> TrustState {
        self.trust
    }

    /// Whether the re-key window is open now.
    pub const fn rekey_open(&self) -> bool {
        self.window.is_open(self.now_ms)
    }

    /// The `sys/trust/info` view now.
    pub const fn trust_info(&self) -> TrustInfo {
        self.trust.info(self.rekey_open())
    }

    /// Whether a transfer is in progress.
    pub const fn receiving(&self) -> bool {
        matches!(self.state, Loader::Receiving { .. })
    }

    /// Handles one request.
    pub fn step(self, key: &str, body: &[u8]) -> (Self, Result<Action, HandlerError>) {
        let Ok(key) = Key::new(key) else {
            return (self, Err(HandlerError::InvalidRequest));
        };
        let cmd = match Command::parse(key, body) {
            Ok(Some(c)) => c,
            Ok(None) => return (self, Ok(Action::App)),
            Err(e) => return (self, Err(e)),
        };
        match cmd {
            Command::TrustInfo => {
                let mut out = [0u8; INFO_MAX];
                let n = self.trust_info().encode(&mut out).unwrap_or(0);
                return (self, Ok(Action::TrustInfo(out[..n].to_vec())));
            }
            Command::TrustClaim { key } => {
                return match self.trust.claim(key, self.rekey_open()) {
                    Ok(trust) => (
                        Self {
                            trust,
                            window: Window::Closed,
                            ..self
                        },
                        Ok(Action::Claim),
                    ),
                    Err(e) => (self, Err(e)),
                };
            }
            _ => {}
        }
        let (state, effect) = self.state.step(&cmd, self.capacity);
        let next = Self { state, ..self };
        match effect {
            Err(e) => (next, Err(e)),
            Ok(Effect::Activate { image, auth }) => {
                match self.trust.accept_commit(&image, auth.as_ref()) {
                    Ok(trust) => (
                        Self { trust, ..next },
                        Ok(Action::Activate(image.len, image.crc)),
                    ),
                    Err(e) => (next, Err(e)),
                }
            }
            Ok(Effect::Erase { len }) => (next, Ok(Action::Erase(len))),
            Ok(Effect::Write { offset, data }) => (next, Ok(Action::Write(offset, data.to_vec()))),
            Ok(Effect::None) => {
                let action = match cmd {
                    Command::Start => Action::Start,
                    Command::Stop => Action::Stop,
                    _ => Action::Info,
                };
                (next, Ok(action))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_runs_through_the_device_loader() {
        let image: Vec<u8> = (0..1300u32).map(|i| (i * 7) as u8).collect();
        let steps = plan(&image, CHUNK_MAX).unwrap();
        assert_eq!(steps.len(), 1 + 3 + 1);
        assert_eq!(steps[0].key, "sys/loader/begin");
        assert_eq!(steps[1].key, "sys/loader/chunk");
        assert_eq!(steps[4].key, "sys/loader/commit");
        assert_eq!(
            steps.iter().map(|s| s.done).collect::<Vec<_>>(),
            [0, 512, 1024, 1300, 1300]
        );

        let mut dev = DeviceLoader::new(4096);
        let mut written = Vec::new();
        let mut last = None;
        for s in &steps {
            let (next, r) = dev.step(s.key, &s.body);
            dev = next;
            match r.unwrap() {
                Action::Write(off, data) => {
                    assert_eq!(off as usize, written.len());
                    written.extend(data);
                }
                a => last = Some(a),
            }
        }
        assert_eq!(written, image);
        assert_eq!(last, Some(Action::Activate(1300, image_crc32(&image))));
        assert!(!dev.receiving());
    }

    #[test]
    fn device_loader_rejects_out_of_order_and_bad_crc() {
        let image = [1u8; 20];
        let steps = plan(&image, 8).unwrap();
        let (dev, _) = DeviceLoader::new(64).step(steps[0].key, &steps[0].body);
        assert!(dev.receiving());
        let (dev2, r) = dev.step(steps[2].key, &steps[2].body);
        assert_eq!(r, Err(HandlerError::InvalidRequest));
        assert!(!dev2.receiving());

        let mut corrupt = steps.clone();
        corrupt[1].body[5] ^= 1;
        let mut d = DeviceLoader::new(64);
        let mut res = Ok(Action::Info);
        for s in &corrupt {
            let (n, r) = d.step(s.key, &s.body);
            d = n;
            res = r;
        }
        assert_eq!(res, Err(HandlerError::InvalidRequest));

        let (_, r) = DeviceLoader::new(8).step(steps[0].key, &steps[0].body);
        assert_eq!(r, Err(HandlerError::BufferTooSmall));
    }

    #[test]
    fn device_loader_passes_system_and_app_commands() {
        let d = DeviceLoader::new(8);
        assert_eq!(d.step("sys/info", &[]).1, Ok(Action::Info));
        assert_eq!(d.step("sys/payload/start", &[]).1, Ok(Action::Start));
        assert_eq!(d.step("sys/payload/stop", &[]).1, Ok(Action::Stop));
        assert_eq!(d.step("app/led", &[1]).1, Ok(Action::App));
        assert_eq!(d.step("sys/nope", &[]).1, Err(HandlerError::NotFound));
        assert_eq!(d.step("bad//", &[]).1, Err(HandlerError::InvalidRequest));
    }

    /// `web/test/fixtures/trust-vector.json` (TEST KEY ONLY).
    mod vector {
        pub const PUBLIC_KEY: &str =
            "03a107bff3ce10be1d70dd18e74bc09967e4d6309ba50d5f1ddc8664125531b8";
        pub const VERSION: u32 = 1_761_500_000;
        pub const SHA256: &str = "1e9bc38cbf860b9ec31918b065f9b52476c549a782e0e7990bed8ce3868d2371";
        pub const MESSAGE: &str = "6d772d7061796c6f61642d763100e8030000605bfe681e9bc38cbf860b9ec31918b065f9b52476c549a782e0e7990bed8ce3868d2371";
        pub const SIGNATURE: &str = "ca90e0aef84dfa5173aed3af1738933d2cebbd3f74012fe398847dad9db8b092fe91b9beaa7b59c6665f41b3abd352ddf890668cf994125438aa084c6a7b9f07";
        pub const COMMIT_BODY: &str = "605bfe68ca90e0aef84dfa5173aed3af1738933d2cebbd3f74012fe398847dad9db8b092fe91b9beaa7b59c6665f41b3abd352ddf890668cf994125438aa084c6a7b9f07";

        pub fn image() -> Vec<u8> {
            (0..1000usize).map(|i| ((i * 7 + 3) & 0xff) as u8).collect()
        }

        pub fn hex(s: &str) -> Vec<u8> {
            (0..s.len() / 2)
                .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
                .collect()
        }
    }

    /// Runs `steps` through `dev`, returning it and the last result.
    fn run(mut dev: DeviceLoader, steps: &[Step]) -> (DeviceLoader, Result<Action, HandlerError>) {
        let mut last = Ok(Action::Info);
        for s in steps {
            let (next, r) = dev.step(s.key, &s.body);
            dev = next;
            last = r;
            if last.is_err() {
                break;
            }
        }
        (dev, last)
    }

    #[test]
    fn wire_helpers_match_the_golden_vector() {
        let image = vector::image();
        assert_eq!(sha256(&image).to_vec(), vector::hex(vector::SHA256));
        let m = signed_message(1000, vector::VERSION, &sha256(&image)).unwrap();
        assert_eq!(m, vector::hex(vector::MESSAGE));
        assert_eq!(signed_message(1000, 1, &[0; 31]), None);
        let body = signed_commit_body(vector::VERSION, &vector::hex(vector::SIGNATURE)).unwrap();
        assert_eq!(body, vector::hex(vector::COMMIT_BODY));
        assert_eq!(signed_commit_body(1, &[0; 63]), None);
        let names: Vec<_> = (0..4).map(trust_state_name).collect();
        assert_eq!(
            names,
            [Some("unclaimed"), Some("claimed"), Some("rekey"), None]
        );
    }

    #[test]
    fn signed_upload_of_the_golden_vector_installs_and_replay_is_denied() {
        let image = vector::image();
        let key = vector::hex(vector::PUBLIC_KEY);
        let commit = vector::hex(vector::COMMIT_BODY);
        let dev = DeviceLoader::new(4096);
        let (dev, r) = dev.step("sys/trust/info", &[]);
        assert_eq!(r, Ok(Action::TrustInfo(vec![0, 0, 0, 0, 0])));
        let (dev, r) = dev.step("sys/trust/claim", &key);
        assert_eq!(r, Ok(Action::Claim));
        let (dev, r) = dev.step("sys/trust/info", &[]);
        let Ok(Action::TrustInfo(info)) = r else {
            panic!("{r:?}")
        };
        assert_eq!(info[0], 1);
        assert_eq!(&info[5..], key.as_slice());

        // Claimed: an unsigned upload is denied, nothing activates.
        let (dev, r) = run(dev, &plan(&image, 256).unwrap());
        assert_eq!(r, Err(HandlerError::Denied));
        assert!(!dev.receiving());

        let steps = plan_with_commit(&image, 256, &commit).unwrap();
        let (dev, r) = run(dev, &steps);
        assert_eq!(r, Ok(Action::Activate(1000, image_crc32(&image))));
        let TrustState::Claimed { min_version, .. } = dev.trust() else {
            panic!()
        };
        assert_eq!(min_version, Version(vector::VERSION));
        // The same signed commit again is a replay.
        let (_, r) = run(dev, &steps);
        assert_eq!(r, Err(HandlerError::Denied));
        // Another image under the same signature fails too.
        let mut other = image.clone();
        other[0] ^= 1;
        let (_, r) = run(
            DeviceLoader::new(4096).step("sys/trust/claim", &key).0,
            &plan_with_commit(&other, 256, &commit).unwrap(),
        );
        assert_eq!(r, Err(HandlerError::Denied));
    }

    #[test]
    fn unclaimed_accepts_unsigned_and_rekey_needs_presence() {
        let image = [9u8; 40];
        let (dev, r) = run(DeviceLoader::new(64), &plan(&image, 16).unwrap());
        assert_eq!(r, Ok(Action::Activate(40, image_crc32(&image))));
        let key = vector::hex(vector::PUBLIC_KEY);
        let (dev, _) = dev.step("sys/trust/claim", &key);
        assert!(dev.trust().is_claimed());
        // Claimed, no presence: a second claim is denied.
        let (dev, r) = dev.at(1_000).step("sys/trust/claim", &key);
        assert_eq!(r, Err(HandlerError::Denied));
        // Presence opens the window for 60 s; info reports state 2.
        let dev = dev.assert_presence();
        assert!(dev.rekey_open());
        assert_eq!(
            dev.step("sys/trust/info", &[])
                .1
                .map(|a| matches!(a, Action::TrustInfo(b) if b[0] == 2)),
            Ok(true)
        );
        assert!(!dev.at(61_000).rekey_open(), "expired after 60 s");
        let (dev, r) = dev.at(30_000).step("sys/trust/claim", &key);
        assert_eq!(r, Ok(Action::Claim));
        assert!(!dev.rekey_open(), "a claim closes the window");
        assert_eq!(
            dev.step("sys/trust/claim", &[0; 31]).1,
            Err(HandlerError::InvalidRequest)
        );
        assert_eq!(
            dev.step("sys/loader/commit", &[0; 5]).1,
            Err(HandlerError::InvalidRequest)
        );
    }

    #[test]
    fn plan_rejects_empty_and_clamps_chunk() {
        assert_eq!(plan(&[], 8), Err(PlanError::Empty));
        let steps = plan(&[0u8; 1100], 10_000).unwrap();
        assert_eq!(steps.len(), 1 + 3 + 1);
        assert_eq!(steps[1].body.len(), 4 + CHUNK_MAX);
        assert_eq!(PlanError::Empty.describe(), "image is empty");
    }

    #[test]
    fn info_round_trips_and_names_statuses() {
        let info = Info {
            abi_version: 1,
            capacity: 65536,
            status: 2,
            chip: "esp32c3".into(),
            firmware: "0.1.0".into(),
            node: None,
        };
        let bytes = info.encode().unwrap();
        assert_eq!(Info::decode(&bytes), Some(info.clone()));
        // A 0.5 response with the trailing node id decodes too.
        let with_node = Info {
            node: Some([1, 2, 3, 4, 5, 6, 0, 0]),
            ..info.clone()
        };
        let longer = with_node.encode().unwrap();
        assert_eq!(longer.len(), bytes.len() + 8);
        assert_eq!(Info::decode(&longer), Some(with_node));
        assert_eq!(Info::decode(&bytes[..3]), None);
        assert_eq!(Info { status: 9, ..info }.encode(), None);
        let names: Vec<_> = (0..6).map(status_name).collect();
        assert_eq!(
            names,
            [
                Some("empty"),
                Some("stopped"),
                Some("running"),
                Some("trapped"),
                Some("loading"),
                None
            ]
        );
    }
}
