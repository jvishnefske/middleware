//! Payload loader protocol: system keys, message bodies and the pure loader
//! state machine.
//!
//! A host (browser page, CLI) installs a WASM payload on a device with plain
//! middleware requests:
//!
//! | key                 | request body                    | response body      |
//! |---------------------|---------------------------------|--------------------|
//! | `sys/info`          | empty                           | [`DeviceInfo`]     |
//! | `sys/loader/begin`  | `[len u32 LE][crc32 u32 LE]`    | empty              |
//! | `sys/loader/chunk`  | `[offset u32 LE][≤CHUNK_MAX bytes]` | empty          |
//! | `sys/loader/commit` | empty                           | empty              |
//! | `sys/payload/start` | empty                           | empty              |
//! | `sys/payload/stop`  | empty                           | empty              |
//!
//! The device publishes logs on `sys/log` and payload state changes on
//! `sys/payload/status` ([`PayloadStatus`] as one byte). All integers are
//! little-endian.
//!
//! [`Loader`] is a value-typed state machine. [`Loader::step`] consumes the
//! state and a [`Command`] and returns the next state and an [`Effect`] for
//! the firmware shell to perform (erase, write, activate); it does no I/O.

use crate::crc::Crc32;
use crate::key::Key;
use crate::transport::HandlerError;

/// Largest chunk body accepted by `sys/loader/chunk`.
pub const CHUNK_MAX: usize = 512;

/// Well-known system keys.
pub mod keys {
    use crate::key::Key;

    /// Device description request.
    pub const INFO: Key<'static> = Key::from_static("sys/info");
    /// Starts a transfer.
    pub const BEGIN: Key<'static> = Key::from_static("sys/loader/begin");
    /// Carries one chunk.
    pub const CHUNK: Key<'static> = Key::from_static("sys/loader/chunk");
    /// Verifies and activates the transferred image.
    pub const COMMIT: Key<'static> = Key::from_static("sys/loader/commit");
    /// Starts the installed payload.
    pub const START: Key<'static> = Key::from_static("sys/payload/start");
    /// Stops the running payload.
    pub const STOP: Key<'static> = Key::from_static("sys/payload/stop");
    /// Device log lines (published by the device, UTF-8).
    pub const LOG: Key<'static> = Key::from_static("sys/log");
    /// Payload state changes (published by the device).
    pub const STATUS: Key<'static> = Key::from_static("sys/payload/status");
}

/// A parsed system request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command<'a> {
    /// `sys/info`.
    Info,
    /// `sys/loader/begin`.
    Begin {
        /// Total image length in bytes.
        len: u32,
        /// CRC-32 of the whole image.
        crc: u32,
    },
    /// `sys/loader/chunk`.
    Chunk {
        /// Byte offset of `data` within the image.
        offset: u32,
        /// Image bytes.
        data: &'a [u8],
    },
    /// `sys/loader/commit`.
    Commit,
    /// `sys/payload/start`.
    Start,
    /// `sys/payload/stop`.
    Stop,
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let b = bytes.get(at..at + 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

impl<'a> Command<'a> {
    /// Parses a request. `Ok(None)` means the key is not a system key and
    /// belongs to the application; a system key with a bad body is
    /// [`HandlerError::InvalidRequest`].
    pub fn parse(key: Key<'_>, body: &'a [u8]) -> Result<Option<Self>, HandlerError> {
        let bad = HandlerError::InvalidRequest;
        let empty = |c| {
            if body.is_empty() {
                Ok(Some(c))
            } else {
                Err(bad)
            }
        };
        match key.as_str() {
            k if k == keys::INFO.as_str() => empty(Command::Info),
            k if k == keys::COMMIT.as_str() => empty(Command::Commit),
            k if k == keys::START.as_str() => empty(Command::Start),
            k if k == keys::STOP.as_str() => empty(Command::Stop),
            k if k == keys::BEGIN.as_str() => {
                if body.len() != 8 {
                    return Err(bad);
                }
                let len = u32_at(body, 0).ok_or(bad)?;
                let crc = u32_at(body, 4).ok_or(bad)?;
                Ok(Some(Command::Begin { len, crc }))
            }
            k if k == keys::CHUNK.as_str() => {
                let offset = u32_at(body, 0).ok_or(bad)?;
                let data = body.get(4..).ok_or(bad)?;
                if data.is_empty() || data.len() > CHUNK_MAX {
                    return Err(bad);
                }
                Ok(Some(Command::Chunk { offset, data }))
            }
            k if k.starts_with("sys/") => Err(HandlerError::NotFound),
            _ => Ok(None),
        }
    }

    /// The key this command is sent on.
    pub const fn key(&self) -> Key<'static> {
        match self {
            Command::Info => keys::INFO,
            Command::Begin { .. } => keys::BEGIN,
            Command::Chunk { .. } => keys::CHUNK,
            Command::Commit => keys::COMMIT,
            Command::Start => keys::START,
            Command::Stop => keys::STOP,
        }
    }

    /// Writes the request body into `out`, returning its length. `None` when
    /// `out` is too small.
    pub fn encode_body(&self, out: &mut [u8]) -> Option<usize> {
        match self {
            Command::Begin { len, crc } => {
                out.get_mut(..4)?.copy_from_slice(&len.to_le_bytes());
                out.get_mut(4..8)?.copy_from_slice(&crc.to_le_bytes());
                Some(8)
            }
            Command::Chunk { offset, data } => {
                out.get_mut(..4)?.copy_from_slice(&offset.to_le_bytes());
                out.get_mut(4..4 + data.len())?.copy_from_slice(data);
                Some(4 + data.len())
            }
            _ => Some(0),
        }
    }
}

/// What the firmware shell must do after a [`Loader::step`].
///
/// `None` after a `Chunk` means the chunk was a resend of the last one
/// (its response was lost) and is already written: reply success only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect<'a> {
    /// Nothing; reply success.
    None,
    /// Prepare storage for an image of `len` bytes (stop the running payload,
    /// erase).
    Erase {
        /// Image length.
        len: u32,
    },
    /// Write `data` at `offset`.
    Write {
        /// Byte offset in the image.
        offset: u32,
        /// Bytes to write.
        data: &'a [u8],
    },
    /// The image is complete and verified; record it as installed.
    Activate {
        /// Image length.
        len: u32,
        /// Image CRC-32.
        crc: u32,
    },
}

/// Loader progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loader {
    /// No transfer in progress.
    Idle,
    /// Receiving an image.
    Receiving {
        /// Expected length.
        len: u32,
        /// Expected CRC-32.
        crc: u32,
        /// Bytes received so far (chunks must arrive in order).
        received: u32,
        /// CRC-32 of the bytes received so far.
        running: Crc32,
    },
}

impl Loader {
    /// Applies `cmd` given a storage `capacity` in bytes. On error the
    /// returned state is [`Loader::Idle`] for transfer commands (the transfer
    /// is abandoned) and unchanged otherwise. Non-loader commands
    /// (`Info`, `Start`, `Stop`) leave the state unchanged with
    /// [`Effect::None`]; the shell handles them.
    pub fn step<'a>(
        self,
        cmd: &Command<'a>,
        capacity: u32,
    ) -> (Loader, Result<Effect<'a>, HandlerError>) {
        match (*cmd, self) {
            (Command::Begin { len, crc }, _) => {
                if len == 0 || len > capacity {
                    (Loader::Idle, Err(HandlerError::BufferTooSmall))
                } else {
                    (
                        Loader::Receiving {
                            len,
                            crc,
                            received: 0,
                            running: Crc32::new(),
                        },
                        Ok(Effect::Erase { len }),
                    )
                }
            }
            (
                Command::Chunk { offset, data },
                Loader::Receiving {
                    len,
                    crc,
                    received,
                    running,
                },
            ) => {
                let n = data.len() as u32;
                if offset.checked_add(n) == Some(received) {
                    // A resend of the last chunk whose response was lost:
                    // already written, so acknowledge without changing state.
                    // Commit's CRC check still covers the stored bytes.
                    (self, Ok(Effect::None))
                } else if offset != received {
                    (Loader::Idle, Err(HandlerError::InvalidRequest))
                } else if received.checked_add(n).is_none_or(|end| end > len) {
                    (Loader::Idle, Err(HandlerError::BufferTooSmall))
                } else {
                    (
                        Loader::Receiving {
                            len,
                            crc,
                            received: received + n,
                            running: running.update(data),
                        },
                        Ok(Effect::Write { offset, data }),
                    )
                }
            }
            (
                Command::Commit,
                Loader::Receiving {
                    len,
                    crc,
                    received,
                    running,
                },
            ) => {
                if received == len && running.finish() == crc {
                    (Loader::Idle, Ok(Effect::Activate { len, crc }))
                } else {
                    (Loader::Idle, Err(HandlerError::InvalidRequest))
                }
            }
            (Command::Chunk { .. } | Command::Commit, Loader::Idle) => {
                (Loader::Idle, Err(HandlerError::InvalidRequest))
            }
            (Command::Info | Command::Start | Command::Stop, state) => (state, Ok(Effect::None)),
        }
    }
}

/// Payload lifecycle, published as one byte on `sys/payload/status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PayloadStatus {
    /// No payload installed.
    Empty = 0,
    /// Installed, not running.
    Stopped = 1,
    /// Running.
    Running = 2,
    /// Stopped by a trap or fuel exhaustion.
    Trapped = 3,
    /// Transfer in progress.
    Loading = 4,
}

impl PayloadStatus {
    /// Parses the status byte.
    pub const fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(PayloadStatus::Empty),
            1 => Some(PayloadStatus::Stopped),
            2 => Some(PayloadStatus::Running),
            3 => Some(PayloadStatus::Trapped),
            4 => Some(PayloadStatus::Loading),
            _ => None,
        }
    }
}

/// Longest chip or firmware name in [`DeviceInfo`].
pub const MAX_NAME: usize = 32;

/// `sys/info` response:
/// `[abi_version u16][capacity u32][status u8][chip_len u8][chip][fw_len u8][fw]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceInfo<'a> {
    /// [`crate::wasm_abi::ABI_VERSION`] implemented by the firmware.
    pub abi_version: u16,
    /// Largest payload image in bytes.
    pub capacity: u32,
    /// Current payload state.
    pub status: PayloadStatus,
    /// Chip name, e.g. `esp32c3`.
    pub chip: &'a str,
    /// Firmware version string.
    pub firmware: &'a str,
}

impl<'a> DeviceInfo<'a> {
    /// Encodes into `out`; `None` if `out` is too small or a name exceeds
    /// [`MAX_NAME`].
    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        let chip = self.chip.as_bytes();
        let fw = self.firmware.as_bytes();
        if chip.len() > MAX_NAME || fw.len() > MAX_NAME {
            return None;
        }
        let mut at = 0;
        let mut put = |bytes: &[u8]| -> Option<()> {
            out.get_mut(at..at + bytes.len())?.copy_from_slice(bytes);
            at += bytes.len();
            Some(())
        };
        put(&self.abi_version.to_le_bytes())?;
        put(&self.capacity.to_le_bytes())?;
        put(&[self.status as u8, chip.len() as u8])?;
        put(chip)?;
        put(&[fw.len() as u8])?;
        put(fw)?;
        Some(at)
    }

    /// Decodes a `sys/info` response.
    pub fn decode(bytes: &'a [u8]) -> Option<Self> {
        let abi_version = u16::from_le_bytes([*bytes.first()?, *bytes.get(1)?]);
        let capacity = u32_at(bytes, 2)?;
        let status = PayloadStatus::from_byte(*bytes.get(6)?)?;
        let chip_len = usize::from(*bytes.get(7)?);
        let chip = core::str::from_utf8(bytes.get(8..8 + chip_len)?).ok()?;
        let fw_at = 8 + chip_len;
        let fw_len = usize::from(*bytes.get(fw_at)?);
        let firmware = core::str::from_utf8(bytes.get(fw_at + 1..fw_at + 1 + fw_len)?).ok()?;
        Some(DeviceInfo {
            abi_version,
            capacity,
            status,
            chip,
            firmware,
        })
    }
}

/// Whether `key` is reserved for the system (`sys/...`).
pub fn is_system_key(key: Key<'_>) -> bool {
    key.segments().next() == Some("sys")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_transfer(image: &[u8], chunk: usize) -> Result<Effect<'static>, HandlerError> {
        let crc = Crc32::checksum(image);
        let (mut s, e) = Loader::Idle.step(
            &Command::Begin {
                len: image.len() as u32,
                crc,
            },
            4096,
        );
        assert_eq!(
            e,
            Ok(Effect::Erase {
                len: image.len() as u32
            })
        );
        for (i, part) in image.chunks(chunk).enumerate() {
            let cmd = Command::Chunk {
                offset: (i * chunk) as u32,
                data: part,
            };
            let (next, e) = s.step(&cmd, 4096);
            assert!(matches!(e, Ok(Effect::Write { .. })));
            s = next;
        }
        let (s, e) = s.step(&Command::Commit, 4096);
        assert_eq!(s, Loader::Idle);
        e.map(|e| match e {
            Effect::Activate { len, crc } => Effect::Activate { len, crc },
            _ => Effect::None,
        })
    }

    #[test]
    fn complete_transfer_activates() {
        let image = [0x5Au8; 1300];
        let crc = Crc32::checksum(&image);
        assert_eq!(
            run_transfer(&image, CHUNK_MAX),
            Ok(Effect::Activate { len: 1300, crc })
        );
    }

    #[test]
    fn out_of_order_oversized_and_corrupt_transfers_fail() {
        let (s, _) = Loader::Idle.step(&Command::Begin { len: 8, crc: 0 }, 64);
        let (s2, e) = s.step(
            &Command::Chunk {
                offset: 4,
                data: &[1],
            },
            64,
        );
        assert_eq!((s2, e), (Loader::Idle, Err(HandlerError::InvalidRequest)));

        let (s2, e) = s.step(
            &Command::Chunk {
                offset: 0,
                data: &[0; 9],
            },
            64,
        );
        assert_eq!((s2, e), (Loader::Idle, Err(HandlerError::BufferTooSmall)));

        let (s2, _) = s.step(
            &Command::Chunk {
                offset: 0,
                data: &[0; 8],
            },
            64,
        );
        assert_eq!(
            s2.step(&Command::Commit, 64).1,
            Err(HandlerError::InvalidRequest)
        );

        let (s2, _) = s.step(
            &Command::Chunk {
                offset: 0,
                data: &[0; 4],
            },
            64,
        );
        assert_eq!(
            s2.step(&Command::Commit, 64).1,
            Err(HandlerError::InvalidRequest)
        );

        assert_eq!(
            Loader::Idle.step(&Command::Begin { len: 65, crc: 0 }, 64).1,
            Err(HandlerError::BufferTooSmall)
        );
        assert_eq!(
            Loader::Idle.step(&Command::Commit, 64).1,
            Err(HandlerError::InvalidRequest)
        );
    }

    #[test]
    fn resent_last_chunk_is_acknowledged_without_rewriting() {
        let image = [7u8; 10];
        let crc = Crc32::checksum(&image);
        let (s, _) = Loader::Idle.step(&Command::Begin { len: 10, crc }, 64);
        let first = Command::Chunk {
            offset: 0,
            data: &image[..6],
        };
        let (s, _) = s.step(&first, 64);
        let (again, e) = s.step(&first, 64);
        assert_eq!((again, e), (s, Ok(Effect::None)));
        let (s, e) = again.step(
            &Command::Chunk {
                offset: 6,
                data: &image[6..],
            },
            64,
        );
        assert!(matches!(e, Ok(Effect::Write { offset: 6, .. })));
        assert_eq!(
            s.step(&Command::Commit, 64).1,
            Ok(Effect::Activate { len: 10, crc })
        );
    }

    #[test]
    fn commands_round_trip_through_parse() {
        let data = [1u8, 2, 3];
        for cmd in [
            Command::Info,
            Command::Begin {
                len: 10,
                crc: 0xDEAD_BEEF,
            },
            Command::Chunk {
                offset: 512,
                data: &data,
            },
            Command::Commit,
            Command::Start,
            Command::Stop,
        ] {
            let mut body = [0u8; 16];
            let n = cmd.encode_body(&mut body).unwrap();
            assert_eq!(Command::parse(cmd.key(), &body[..n]), Ok(Some(cmd)));
        }
    }

    #[test]
    fn parse_rejects_bad_bodies_and_passes_app_keys() {
        assert_eq!(
            Command::parse(keys::INFO, &[1]),
            Err(HandlerError::InvalidRequest)
        );
        assert_eq!(
            Command::parse(keys::BEGIN, &[0; 7]),
            Err(HandlerError::InvalidRequest)
        );
        assert_eq!(
            Command::parse(keys::CHUNK, &[0; 4]),
            Err(HandlerError::InvalidRequest)
        );
        assert_eq!(
            Command::parse(keys::CHUNK, &[0; 4 + CHUNK_MAX + 1]),
            Err(HandlerError::InvalidRequest)
        );
        assert_eq!(
            Command::parse(Key::from_static("sys/nope"), &[]),
            Err(HandlerError::NotFound)
        );
        assert_eq!(Command::parse(Key::from_static("app/x"), &[]), Ok(None));
        assert!(is_system_key(keys::LOG));
        assert!(!is_system_key(Key::from_static("system/x")));
    }

    #[test]
    fn device_info_round_trips() {
        let info = DeviceInfo {
            abi_version: 1,
            capacity: 65536,
            status: PayloadStatus::Running,
            chip: "esp32s3",
            firmware: "0.1.0",
        };
        let mut buf = [0u8; 64];
        let n = info.encode(&mut buf).unwrap();
        assert_eq!(DeviceInfo::decode(&buf[..n]), Some(info));
        assert_eq!(DeviceInfo::decode(&buf[..n - 1]), None);
        assert!(info.encode(&mut buf[..5]).is_none());
    }
}
