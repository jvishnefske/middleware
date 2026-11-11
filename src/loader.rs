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
//! | `sys/loader/commit` | empty, or `[version u32 LE][signature 64]` | empty   |
//! | `sys/payload/start` | empty                           | empty              |
//! | `sys/payload/stop`  | empty                           | empty              |
//! | `sys/trust/info`    | empty                           | [`trust::TrustInfo`] |
//! | `sys/trust/claim`   | `[public key 32]`               | empty              |
//!
//! [`Command::parse`] takes `sys/payload/start` and `stop` with empty
//! bodies only. Over a radio a claimed ESP base firmware device instead
//! requires a [`trust::SignedControl`] body (`[counter u64 LE][signature
//! 64]`), which its over-the-air policy verifies and strips before
//! dispatch (see [`trust`], "Signed control requests").
//!
//! The device publishes logs on `sys/log` and payload state changes on
//! `sys/payload/status` ([`PayloadStatus`] as one byte). All integers are
//! little-endian.
//!
//! [`Loader`] is a value-typed state machine. [`Loader::step`] consumes the
//! state and a [`Command`] and returns the next state and an [`Effect`] for
//! the firmware shell to perform (erase, write, activate); it does no I/O.
//!
//! # Signed commits
//!
//! The commit body and the `sys/trust/*` keys are parsed whatever the
//! features ([`trust`] holds the wire types and the rules). With the
//! `trust` feature the loader also hashes the image (SHA-256,
//! [`sha256::ImageHash`]) while chunks arrive, and [`Effect::Activate`]
//! carries the digest and the commit's [`trust::SignedCommit`] so the
//! shell can decide with [`trust::TrustState::accept_commit`] before it
//! activates anything. Without `trust` the loader behaves as in 0.3: a
//! signature in the commit body is parsed and passed on, never checked.

#[cfg(feature = "trust")]
pub mod sha256;
pub mod trust;

use crate::crc::Crc32;
use crate::key::Key;
use crate::transport::{HandlerError, SenderId};
use trust::{PublicKey, SignedCommit};

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
    /// Root-of-trust state request.
    pub const TRUST_INFO: Key<'static> = Key::from_static("sys/trust/info");
    /// Pins a payload signing key.
    pub const TRUST_CLAIM: Key<'static> = Key::from_static("sys/trust/claim");
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
    Commit {
        /// The signature, for a signed commit.
        auth: Option<SignedCommit>,
    },
    /// `sys/payload/start`.
    Start,
    /// `sys/payload/stop`.
    Stop,
    /// `sys/trust/info`.
    TrustInfo,
    /// `sys/trust/claim`.
    TrustClaim {
        /// The key to pin.
        key: PublicKey,
    },
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
            k if k == keys::COMMIT.as_str() => Ok(Some(Command::Commit {
                auth: SignedCommit::parse_body(body)?,
            })),
            k if k == keys::START.as_str() => empty(Command::Start),
            k if k == keys::STOP.as_str() => empty(Command::Stop),
            k if k == keys::TRUST_INFO.as_str() => empty(Command::TrustInfo),
            k if k == keys::TRUST_CLAIM.as_str() => Ok(Some(Command::TrustClaim {
                key: PublicKey::parse_claim(body)?,
            })),
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
            Command::Commit { .. } => keys::COMMIT,
            Command::Start => keys::START,
            Command::Stop => keys::STOP,
            Command::TrustInfo => keys::TRUST_INFO,
            Command::TrustClaim { .. } => keys::TRUST_CLAIM,
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
            Command::Commit { auth: Some(auth) } => {
                let body = auth.encode();
                out.get_mut(..body.len())?.copy_from_slice(&body);
                Some(body.len())
            }
            Command::TrustClaim { key } => {
                out.get_mut(..key.0.len())?.copy_from_slice(&key.0);
                Some(key.0.len())
            }
            Command::Info
            | Command::Commit { auth: None }
            | Command::Start
            | Command::Stop
            | Command::TrustInfo => Some(0),
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
    /// The image is complete and its CRC-32 matches; record it as
    /// installed. With the `trust` feature the shell must first pass
    /// `image` and `auth` to [`trust::TrustState::accept_commit`].
    Activate {
        /// What was received.
        image: ImageDigest,
        /// The commit's signature, if it was signed.
        auth: Option<SignedCommit>,
    },
}

/// A completed image as the loader received it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageDigest {
    /// Image length.
    pub len: u32,
    /// Image CRC-32.
    pub crc: u32,
    /// SHA-256 of the image (feature `trust`).
    #[cfg(feature = "trust")]
    pub sha256: [u8; 32],
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
        /// SHA-256 of the bytes received so far.
        #[cfg(feature = "trust")]
        hash: sha256::ImageHash,
    },
}

impl Loader {
    /// Applies `cmd` given a storage `capacity` in bytes. On error the
    /// returned state is [`Loader::Idle`] for transfer commands (the transfer
    /// is abandoned) and unchanged otherwise. Non-loader commands
    /// (`Info`, `Start`, `Stop`, `TrustInfo`, `TrustClaim`) leave the state
    /// unchanged with [`Effect::None`]; the shell handles them.
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
                            #[cfg(feature = "trust")]
                            hash: sha256::ImageHash::new(),
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
                    #[cfg(feature = "trust")]
                    hash,
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
                            #[cfg(feature = "trust")]
                            hash: hash.update(data),
                        },
                        Ok(Effect::Write { offset, data }),
                    )
                }
            }
            (
                Command::Commit { auth },
                Loader::Receiving {
                    len,
                    crc,
                    received,
                    running,
                    #[cfg(feature = "trust")]
                    hash,
                },
            ) => {
                if received == len && running.finish() == crc {
                    let image = ImageDigest {
                        len,
                        crc,
                        #[cfg(feature = "trust")]
                        sha256: hash.finish(),
                    };
                    (Loader::Idle, Ok(Effect::Activate { image, auth }))
                } else {
                    (Loader::Idle, Err(HandlerError::InvalidRequest))
                }
            }
            (Command::Chunk { .. } | Command::Commit { .. }, Loader::Idle) => {
                (Loader::Idle, Err(HandlerError::InvalidRequest))
            }
            (
                Command::Info
                | Command::Start
                | Command::Stop
                | Command::TrustInfo
                | Command::TrustClaim { .. },
                state,
            ) => (state, Ok(Effect::None)),
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

/// Bytes of the optional node id at the end of a `sys/info` response.
pub const NODE_ID_LEN: usize = 8;

/// Largest `sys/info` response [`DeviceInfo::encode`] writes.
pub const MAX_INFO_LEN: usize = 2 + 4 + 1 + 1 + MAX_NAME + 1 + MAX_NAME + NODE_ID_LEN;

/// `sys/info` response:
/// `[abi_version u16][capacity u32][status u8][chip_len u8][chip][fw_len u8][fw]`,
/// then, since middleware 0.5, optionally `[node id 8]`: the device's
/// [`SenderId`] (its MAC zero-padded, [`SenderId::from_mac`]), the id it
/// carries as a sender on its radios.
///
/// Decoders read the fields they know and ignore whatever follows, so a
/// response with the node id decodes as before on a 0.4 host, and later
/// fields may be appended after it. A response that stops after the
/// firmware name has no node id (`node: None`, a pre-0.5 device).
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
    /// The device's node id, if it reports one.
    pub node: Option<SenderId>,
}

impl<'a> DeviceInfo<'a> {
    /// Encodes into `out`; `None` if `out` is too small or a name exceeds
    /// [`MAX_NAME`]. At most [`MAX_INFO_LEN`] bytes.
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
        if let Some(node) = self.node {
            put(&node.0)?;
        }
        Some(at)
    }

    /// Decodes a `sys/info` response, ignoring bytes after the fields it
    /// knows (see the type docs).
    pub fn decode(bytes: &'a [u8]) -> Option<Self> {
        let abi_version = u16::from_le_bytes([*bytes.first()?, *bytes.get(1)?]);
        let capacity = u32_at(bytes, 2)?;
        let status = PayloadStatus::from_byte(*bytes.get(6)?)?;
        let chip_len = usize::from(*bytes.get(7)?);
        let chip = core::str::from_utf8(bytes.get(8..8 + chip_len)?).ok()?;
        let fw_at = 8 + chip_len;
        let fw_len = usize::from(*bytes.get(fw_at)?);
        let fw_end = fw_at + 1 + fw_len;
        let firmware = core::str::from_utf8(bytes.get(fw_at + 1..fw_end)?).ok()?;
        let node = bytes
            .get(fw_end..fw_end + NODE_ID_LEN)
            .and_then(|b| <[u8; NODE_ID_LEN]>::try_from(b).ok())
            .map(SenderId);
        Some(DeviceInfo {
            abi_version,
            capacity,
            status,
            chip,
            firmware,
            node,
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
        let (s, e) = s.step(&COMMIT, 4096);
        assert_eq!(s, Loader::Idle);
        e.map(|e| match e {
            Effect::Activate { image, auth } => Effect::Activate { image, auth },
            _ => Effect::None,
        })
    }

    const COMMIT: Command<'static> = Command::Commit { auth: None };

    fn digest(image: &[u8]) -> ImageDigest {
        ImageDigest {
            len: image.len() as u32,
            crc: Crc32::checksum(image),
            #[cfg(feature = "trust")]
            sha256: sha256::ImageHash::digest(image),
        }
    }

    fn activated(e: Result<Effect<'_>, HandlerError>) -> Option<ImageDigest> {
        match e {
            Ok(Effect::Activate { image, .. }) => Some(image),
            _ => None,
        }
    }

    #[test]
    fn complete_transfer_activates() {
        let image = [0x5Au8; 1300];
        assert_eq!(
            run_transfer(&image, CHUNK_MAX),
            Ok(Effect::Activate {
                image: digest(&image),
                auth: None
            })
        );
    }

    /// The digest (SHA-256 with `trust`) covers the streamed bytes, and the
    /// commit's signature is passed through untouched.
    #[test]
    fn commit_passes_digest_and_signature_through() {
        let image: [u8; 1000] = core::array::from_fn(|i| (i * 7 + 3) as u8);
        let auth = trust::SignedCommit {
            version: trust::Version(9),
            signature: trust::Signature([5; 64]),
        };
        let (mut s, _) = Loader::Idle.step(
            &Command::Begin {
                len: 1000,
                crc: Crc32::checksum(&image),
            },
            4096,
        );
        for (i, part) in image.chunks(97).enumerate() {
            s = s
                .step(
                    &Command::Chunk {
                        offset: (i * 97) as u32,
                        data: part,
                    },
                    4096,
                )
                .0;
        }
        let (_, e) = s.step(&Command::Commit { auth: Some(auth) }, 4096);
        assert_eq!(
            e,
            Ok(Effect::Activate {
                image: digest(&image),
                auth: Some(auth)
            })
        );
        #[cfg(feature = "trust")]
        assert_eq!(
            activated(e).map(|d| d.sha256),
            Some(trust_vector_sha()),
            "golden vector image hash"
        );
    }

    #[cfg(feature = "trust")]
    fn trust_vector_sha() -> [u8; 32] {
        let h = "1e9bc38cbf860b9ec31918b065f9b52476c549a782e0e7990bed8ce3868d2371";
        core::array::from_fn(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap())
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
        assert_eq!(s2.step(&COMMIT, 64).1, Err(HandlerError::InvalidRequest));

        let (s2, _) = s.step(
            &Command::Chunk {
                offset: 0,
                data: &[0; 4],
            },
            64,
        );
        assert_eq!(s2.step(&COMMIT, 64).1, Err(HandlerError::InvalidRequest));

        assert_eq!(
            Loader::Idle.step(&Command::Begin { len: 65, crc: 0 }, 64).1,
            Err(HandlerError::BufferTooSmall)
        );
        assert_eq!(
            Loader::Idle.step(&COMMIT, 64).1,
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
            activated(s.step(&COMMIT, 64).1).map(|d| (d.len, d.crc)),
            Some((10, crc))
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
            COMMIT,
            Command::Commit {
                auth: Some(trust::SignedCommit {
                    version: trust::Version(0x0102_0304),
                    signature: trust::Signature([0xAB; 64]),
                }),
            },
            Command::Start,
            Command::Stop,
            Command::TrustInfo,
            Command::TrustClaim {
                key: trust::PublicKey([3; 32]),
            },
        ] {
            let mut body = [0u8; 80];
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
            Command::parse(keys::COMMIT, &[0; 67]),
            Err(HandlerError::InvalidRequest)
        );
        assert_eq!(
            Command::parse(keys::COMMIT, &[0; 1]),
            Err(HandlerError::InvalidRequest)
        );
        assert_eq!(
            Command::parse(keys::TRUST_INFO, &[0]),
            Err(HandlerError::InvalidRequest)
        );
        assert_eq!(
            Command::parse(keys::TRUST_CLAIM, &[0; 31]),
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
            node: None,
        };
        let mut buf = [0u8; 64];
        let n = info.encode(&mut buf).unwrap();
        assert_eq!(DeviceInfo::decode(&buf[..n]), Some(info));
        assert_eq!(DeviceInfo::decode(&buf[..n - 1]), None);
        assert!(info.encode(&mut buf[..5]).is_none());
    }

    #[test]
    fn s10_device_info_carries_a_trailing_node_id() {
        let node = SenderId::from_mac([0x24, 0x6F, 0x28, 1, 2, 3]);
        let old = DeviceInfo {
            abi_version: 4,
            capacity: 1 << 20,
            status: PayloadStatus::Stopped,
            chip: "esp32",
            firmware: "0.1.0",
            node: None,
        };
        let new = DeviceInfo {
            node: Some(node),
            ..old
        };
        let mut a = [0u8; MAX_INFO_LEN];
        let mut b = [0u8; MAX_INFO_LEN];
        let n_old = old.encode(&mut a).unwrap();
        let n_new = new.encode(&mut b).unwrap();
        // The node id is appended: the old fields are unchanged.
        assert_eq!(n_new, n_old + NODE_ID_LEN);
        assert_eq!(&b[..n_old], &a[..n_old]);
        assert_eq!(&b[n_old..n_new], &node.0);
        assert_eq!(DeviceInfo::decode(&b[..n_new]), Some(new));
        // A 0.4 response (no node id) decodes with none; a cut node id is
        // ignored, never misread.
        assert_eq!(DeviceInfo::decode(&a[..n_old]), Some(old));
        assert_eq!(DeviceInfo::decode(&b[..n_new - 1]), Some(old));
        // Later fields after the node id are ignored.
        let mut longer = [9u8; MAX_INFO_LEN + 3];
        longer[..n_new].copy_from_slice(&b[..n_new]);
        assert_eq!(DeviceInfo::decode(&longer[..n_new + 3]), Some(new));
        // The largest response fits MAX_INFO_LEN.
        let names = "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn";
        assert_eq!(names.len(), MAX_NAME);
        let max = DeviceInfo {
            chip: names,
            firmware: names,
            ..new
        };
        assert_eq!(max.encode(&mut b), Some(MAX_INFO_LEN));
    }
}
