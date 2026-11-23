//! `sys/os/*` request and response bodies. The layout is `sys/fw/*`'s
//! (`fw_ota::wire`), with its own key prefix and its own chunk size:
//! a Linux root is sent over Zenoh, so a chunk is 64 KiB, not the serial
//! frame's 512 bytes (see design.md, "Linux A/B root (OS)").
//!
//! [`Command::parse`] never panics on hostile input; [`Command::encode_body`]
//! is its inverse, used by hosts. All integers are little-endian.
//!
//! | key              | request body                                     | response body |
//! |------------------|--------------------------------------------------|---------------|
//! | `sys/os/info`    | empty                                            | [`OsInfo`] (20 bytes) |
//! | `sys/os/begin`   | `[len u32][version u32][sha256 32]`              | `[slot u8]` (0 A, 1 B) |
//! | `sys/os/chunk`   | `[offset u32][1..=CHUNK_LEN bytes]`              | empty |
//! | `sys/os/commit`  | empty, or `[cert][signature 64]`                 | empty |
//! | `sys/os/reboot`  | empty, or a [`SignedControl`] (80 bytes)         | empty |
//! | `sys/os/confirm` | empty, or a [`SignedControl`]                    | empty |
//! | `sys/os/abort`   | empty, or a [`SignedControl`]                    | empty |
//!
//! Reboot, confirm and abort are control requests: on a claimed node that
//! arrives over the network they must be signed by the pinned key
//! (`ControlOp::OsReboot|OsConfirm|OsAbort`, the node's counter). Over the
//! local serial port the device is physically in reach and takes them
//! unsigned.

use crate::key::Key;
use crate::loader::release::SignedCert;
use crate::loader::trust::{SignedControl, Version};
use crate::transport::HandlerError;

use super::bootstate::Slot;

/// Longest chunk of `sys/os/chunk`: a 64 KiB data run.
pub const CHUNK_LEN: usize = 64 * 1024;
/// `sys/os/begin`'s body.
pub const BEGIN_LEN: usize = 4 + 4 + 32;
/// `sys/os/info`'s body.
pub const INFO_LEN: usize = 20;
/// `slot` in [`OsInfo`] when there is no inactive slot (no boot state).
pub const NO_SLOT: u8 = 0xff;

/// The keys this module serves.
pub mod keys {
    use crate::key::Key;

    pub const INFO: Key<'static> = Key::from_static("sys/os/info");
    pub const BEGIN: Key<'static> = Key::from_static("sys/os/begin");
    pub const CHUNK: Key<'static> = Key::from_static("sys/os/chunk");
    pub const COMMIT: Key<'static> = Key::from_static("sys/os/commit");
    pub const REBOOT: Key<'static> = Key::from_static("sys/os/reboot");
    pub const CONFIRM: Key<'static> = Key::from_static("sys/os/confirm");
    pub const ABORT: Key<'static> = Key::from_static("sys/os/abort");
    /// Every key, in code order.
    pub const ALL: [Key<'static>; 7] = [INFO, BEGIN, CHUNK, COMMIT, REBOOT, CONFIRM, ABORT];
}

/// Where the update is, as `sys/os/info` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum StateCode {
    /// Nothing in progress.
    Idle = 0,
    /// Chunks arriving into the inactive slot.
    Receiving = 1,
    /// Verified and recorded; `reboot` starts the trial.
    Committed = 2,
    /// Booted the new slot on trial; `confirm` makes it permanent.
    Trial = 3,
    /// The running slot is committed and confirmed.
    Confirmed = 4,
    /// A trial did not confirm: the previous slot booted again.
    RolledBack = 5,
}

impl StateCode {
    pub const fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(StateCode::Idle),
            1 => Some(StateCode::Receiving),
            2 => Some(StateCode::Committed),
            3 => Some(StateCode::Trial),
            4 => Some(StateCode::Confirmed),
            5 => Some(StateCode::RolledBack),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            StateCode::Idle => "idle",
            StateCode::Receiving => "receiving",
            StateCode::Committed => "committed",
            StateCode::Trial => "trial",
            StateCode::Confirmed => "confirmed",
            StateCode::RolledBack => "rolled back",
        }
    }
}

/// The `sys/os/info` response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OsInfo {
    /// The slot the root booted from (0 A, 1 B).
    pub running: Slot,
    pub state: StateCode,
    /// The slot an update writes (the inactive one), or [`NO_SLOT`].
    pub target: Option<Slot>,
    /// The running root's version (`major << 16 | minor`; 0 when unknown).
    pub running_version: u32,
    /// The anti-rollback floor: a new root's version must exceed it.
    pub floor: u32,
    /// Bytes a slot holds.
    pub capacity: u64,
}

impl OsInfo {
    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        let out = out.get_mut(..INFO_LEN)?;
        out[0] = self.running.index() as u8;
        out[1] = self.state as u8;
        out[2] = self.target.map(|s| s.index() as u8).unwrap_or(NO_SLOT);
        out[3] = 0;
        out[4..8].copy_from_slice(&self.running_version.to_le_bytes());
        out[8..12].copy_from_slice(&self.floor.to_le_bytes());
        out[12..20].copy_from_slice(&self.capacity.to_le_bytes());
        Some(INFO_LEN)
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != INFO_LEN || b[3] != 0 {
            return None;
        }
        Some(OsInfo {
            running: Slot::from_index(b[0])?,
            state: StateCode::from_byte(b[1])?,
            target: match b[2] {
                NO_SLOT => None,
                s => Some(Slot::from_index(s)?),
            },
            running_version: u32_at(b, 4)?,
            floor: u32_at(b, 8)?,
            capacity: u64::from_le_bytes(b[12..20].try_into().ok()?),
        })
    }
}

/// A request, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command<'a> {
    Info,
    Begin {
        len: u32,
        version: Version,
        sha256: [u8; 32],
    },
    Chunk {
        offset: u32,
        data: &'a [u8],
    },
    /// `None` when the body is empty (an unclaimed device needs no cert).
    Commit {
        cert: Option<SignedCert<'a>>,
    },
    /// `control` is the signed form, `None` when the body is empty.
    Reboot {
        control: Option<SignedControl>,
    },
    Confirm {
        control: Option<SignedControl>,
    },
    Abort {
        control: Option<SignedControl>,
    },
}

impl<'a> Command<'a> {
    /// Parses a request. `Ok(None)` when `key` is not under `sys/os/`; an
    /// unknown `sys/os/` key is `NotFound`, a bad body `InvalidRequest`.
    pub fn parse(key: Key<'_>, body: &'a [u8]) -> Result<Option<Self>, HandlerError> {
        let bad = HandlerError::InvalidRequest;
        let Some(op) = key.as_str().strip_prefix("sys/os/") else {
            return Ok(None);
        };
        let empty = |c| if body.is_empty() { Ok(Some(c)) } else { Err(bad) };
        match op {
            "info" => empty(Command::Info),
            "reboot" => Ok(Some(Command::Reboot {
                control: SignedControl::parse_body(body)?,
            })),
            "confirm" => Ok(Some(Command::Confirm {
                control: SignedControl::parse_body(body)?,
            })),
            "abort" => Ok(Some(Command::Abort {
                control: SignedControl::parse_body(body)?,
            })),
            "begin" => {
                if body.len() != BEGIN_LEN {
                    return Err(bad);
                }
                let len = u32_at(body, 0).ok_or(bad)?;
                let version = Version(u32_at(body, 4).ok_or(bad)?);
                let sha256 = body.get(8..40).and_then(|s| <[u8; 32]>::try_from(s).ok()).ok_or(bad)?;
                Ok(Some(Command::Begin { len, version, sha256 }))
            }
            "chunk" => {
                let offset = u32_at(body, 0).ok_or(bad)?;
                let data = body.get(4..).ok_or(bad)?;
                if data.is_empty() || data.len() > CHUNK_LEN {
                    return Err(bad);
                }
                Ok(Some(Command::Chunk { offset, data }))
            }
            "commit" => match body.len() {
                0 => Ok(Some(Command::Commit { cert: None })),
                _ => {
                    let c = SignedCert::decode(body).ok_or(bad)?;
                    Ok(Some(Command::Commit { cert: Some(c) }))
                }
            },
            _ => Err(HandlerError::NotFound),
        }
    }

    /// The key this command is sent on.
    pub const fn key(&self) -> Key<'static> {
        match self {
            Command::Info => keys::INFO,
            Command::Begin { .. } => keys::BEGIN,
            Command::Chunk { .. } => keys::CHUNK,
            Command::Commit { .. } => keys::COMMIT,
            Command::Reboot { .. } => keys::REBOOT,
            Command::Confirm { .. } => keys::CONFIRM,
            Command::Abort { .. } => keys::ABORT,
        }
    }

    /// Writes the body into `out`; the length, or `None` if it does not fit.
    pub fn encode_body(&self, out: &mut [u8]) -> Option<usize> {
        match *self {
            Command::Begin { len, version, sha256 } => {
                let out = out.get_mut(..BEGIN_LEN)?;
                out[0..4].copy_from_slice(&len.to_le_bytes());
                out[4..8].copy_from_slice(&version.0.to_le_bytes());
                out[8..40].copy_from_slice(&sha256);
                Some(BEGIN_LEN)
            }
            Command::Chunk { offset, data } => {
                let n = 4 + data.len();
                let out = out.get_mut(..n)?;
                out[..4].copy_from_slice(&offset.to_le_bytes());
                out[4..].copy_from_slice(data);
                Some(n)
            }
            Command::Commit { cert: Some(c) } => c.encode(out),
            Command::Reboot { control } | Command::Confirm { control } | Command::Abort { control } => {
                match control {
                    None => Some(0),
                    Some(c) => {
                        let b = c.encode();
                        out.get_mut(..b.len())?.copy_from_slice(&b);
                        Some(b.len())
                    }
                }
            }
            Command::Commit { cert: None } | Command::Info => Some(0),
        }
    }
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::release::{CertKind, SoftwareCert};
    use crate::loader::trust::{ControlCounter, Signature};

    fn cert() -> SignedCert<'static> {
        SignedCert {
            cert: SoftwareCert {
                kind: CertKind::Os,
                target: "linux-aarch64",
                channel: "stable",
                version: Version(0x0001_0002),
                len: 1000,
                sha256: [7; 32],
            },
            signature: Signature([3; 64]),
        }
    }

    #[test]
    fn every_command_round_trips() {
        let mut buf = [0u8; 4 + CHUNK_LEN];
        let data = [9u8; 100];
        let cmds = [
            Command::Info,
            Command::Begin { len: 1000, version: Version(0x0001_0002), sha256: [7; 32] },
            Command::Chunk { offset: 65536, data: &data },
            Command::Commit { cert: None },
            Command::Commit { cert: Some(cert()) },
            Command::Reboot { control: None },
            Command::Confirm { control: None },
            Command::Abort { control: None },
            Command::Reboot {
                control: Some(SignedControl {
                    counter: ControlCounter(7),
                    signature: Signature([4; 64]),
                }),
            },
        ];
        for cmd in cmds {
            let n = cmd.encode_body(&mut buf).unwrap();
            assert_eq!(Command::parse(cmd.key(), &buf[..n]), Ok(Some(cmd)), "{:?}", cmd.key());
        }
    }

    #[test]
    fn the_begin_body_matches_fw() {
        // The layout sys/fw/begin uses: [len][version][sha256], little-endian.
        let mut b = [0u8; BEGIN_LEN];
        let n = Command::Begin { len: 0x0102_0304, version: Version(5), sha256: [1; 32] }
            .encode_body(&mut b)
            .unwrap();
        assert_eq!(&b[..8], &[4, 3, 2, 1, 5, 0, 0, 0]);
        assert_eq!(n, BEGIN_LEN);
    }

    #[test]
    fn other_prefixes_are_not_ours_and_bad_bodies_are_refused() {
        assert_eq!(Command::parse(Key::from_static("sys/fw/info"), &[]), Ok(None));
        assert_eq!(Command::parse(Key::from_static("sys/os/flash"), &[]), Err(HandlerError::NotFound));
        assert_eq!(Command::parse(Key::from_static("sys/os/info"), &[0]), Err(HandlerError::InvalidRequest));
        assert_eq!(Command::parse(Key::from_static("sys/os/begin"), &[0; 39]), Err(HandlerError::InvalidRequest));
        assert_eq!(Command::parse(Key::from_static("sys/os/chunk"), &[0; 4]), Err(HandlerError::InvalidRequest));
        let too_big = vec![0u8; 4 + CHUNK_LEN + 1];
        assert_eq!(Command::parse(Key::from_static("sys/os/chunk"), &too_big), Err(HandlerError::InvalidRequest));
        assert_eq!(Command::parse(Key::from_static("sys/os/commit"), &[1; 10]), Err(HandlerError::InvalidRequest));
    }

    #[test]
    fn info_round_trips_and_is_20_bytes() {
        let i = OsInfo {
            running: Slot::B,
            state: StateCode::Trial,
            target: Some(Slot::A),
            running_version: 0x0001_0002,
            floor: 0x0001_0002,
            capacity: 3 << 30,
        };
        let mut b = [0u8; INFO_LEN];
        assert_eq!(i.encode(&mut b), Some(INFO_LEN));
        assert_eq!(OsInfo::decode(&b), Some(i));
        let none = OsInfo { target: None, ..i };
        none.encode(&mut b).unwrap();
        assert_eq!(b[2], NO_SLOT);
        assert_eq!(OsInfo::decode(&b).unwrap().target, None);
        assert_eq!(OsInfo::decode(&[0; 19]), None);
    }
}
