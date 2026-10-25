//! Loader helpers for the browser: upload plans, `sys/info` decoding and a
//! device-side loader for fakes and tests.
//!
//! * [`plan`] turns an image into the ordered request sequence
//!   `sys/loader/begin`, `sys/loader/chunk` × n, `sys/loader/commit`, with
//!   bodies from [`Command::encode_body`] and the CRC-32 from [`Crc32`].
//! * [`Info`] is an owned [`DeviceInfo`].
//! * [`DeviceLoader`] runs the middleware's own [`Loader`] state machine, so
//!   a scripted fake device in the JS tests enforces exactly the ordering,
//!   length and CRC rules the firmware does.

use middleware::crc::Crc32;
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
/// `chunk` bytes (clamped to `1..=CHUNK_MAX`).
pub fn plan(image: &[u8], chunk: usize) -> Result<Vec<Step>, PlanError> {
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
        key: Command::Commit.key().as_str(),
        body: Vec::new(),
        done: len,
    });
    Ok(steps)
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
        };
        let mut out = vec![0u8; 8 + 2 * middleware::loader::MAX_NAME + 2];
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
    /// The image is complete and its CRC verified.
    Activate(u32, u32),
}

/// A device-side loader with a storage capacity, for fakes and tests.
///
/// [`DeviceLoader::step`] takes the loader by value and returns its
/// successor, matching [`Loader::step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceLoader {
    state: Loader,
    capacity: u32,
}

impl DeviceLoader {
    /// An idle loader accepting images up to `capacity` bytes.
    pub const fn new(capacity: u32) -> Self {
        Self {
            state: Loader::Idle,
            capacity,
        }
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
        let (state, effect) = self.state.step(&cmd, self.capacity);
        let next = Self { state, ..self };
        let action = effect.map(|e| match (e, cmd) {
            (Effect::Erase { len }, _) => Action::Erase(len),
            (Effect::Write { offset, data }, _) => Action::Write(offset, data.to_vec()),
            (Effect::Activate { len, crc }, _) => Action::Activate(len, crc),
            (Effect::None, Command::Start) => Action::Start,
            (Effect::None, Command::Stop) => Action::Stop,
            (Effect::None, _) => Action::Info,
        });
        (next, action)
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
        };
        let bytes = info.encode().unwrap();
        assert_eq!(Info::decode(&bytes), Some(info.clone()));
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
