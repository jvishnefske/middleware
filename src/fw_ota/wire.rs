//! `sys/fw/*` request and response bodies, and the signed message.
//!
//! [`Command::parse`] decodes a request from its key and body and never
//! panics on hostile input; [`Command::encode_body`] is its inverse, used by
//! hosts. [`FwInfo`] is the `sys/fw/info` response. [`signed_message`] is
//! what a firmware commit signature covers. All integers are
//! little-endian; the table is in the [`super`] module docs.

use super::{CHUNK_LEN, Slot};
use crate::key::Key;
use crate::loader::trust::{SIGNATURE_LEN, Signature, Version};
use crate::transport::{HandlerError, SenderId};

/// The `sys/fw/*` keys.
pub mod keys {
    use crate::key::Key;

    /// Update state, slots and versions.
    pub const INFO: Key<'static> = Key::from_static("sys/fw/info");
    /// Starts a transfer into the inactive slot.
    pub const BEGIN: Key<'static> = Key::from_static("sys/fw/begin");
    /// Carries one chunk.
    pub const CHUNK: Key<'static> = Key::from_static("sys/fw/chunk");
    /// Verifies, authorizes and installs the transferred image.
    pub const COMMIT: Key<'static> = Key::from_static("sys/fw/commit");
    /// Flash-update reboot into the committed image.
    pub const REBOOT: Key<'static> = Key::from_static("sys/fw/reboot");
    /// Explicit buy of the trial image.
    pub const CONFIRM: Key<'static> = Key::from_static("sys/fw/confirm");
    /// Abandons a transfer or a committed update, or rejects a trial.
    pub const ABORT: Key<'static> = Key::from_static("sys/fw/abort");

    /// Every key, in protocol order.
    pub const ALL: [Key<'static>; 7] = [INFO, BEGIN, CHUNK, COMMIT, REBOOT, CONFIRM, ABORT];
}

/// Length of a `sys/fw/begin` body.
pub const BEGIN_LEN: usize = 4 + 4 + 32;
/// Domain-separation tag of [`signed_message`] (distinct from the payload
/// tag `mw-payload-v1` and the control tag `mw-control-v1`).
pub const MESSAGE_TAG: &[u8; 15] = b"mw-firmware-v1\0";
/// Length of [`signed_message`].
pub const MESSAGE_LEN: usize = MESSAGE_TAG.len() + 4 + 4 + 1 + 8 + 32;
/// Length of `sys/fw/info` as [`FwInfo::encode`] writes it.
pub const INFO_LEN: usize = 36;
/// [`FwInfo::target`] when there is none.
pub const NO_SLOT: u8 = 0xff;

/// The message a firmware commit signature covers (64 bytes):
/// `MESSAGE_TAG || len u32 || version u32 || slot u8 || node id 8 || sha256 32`.
pub fn signed_message(
    len: u32,
    version: Version,
    slot: Slot,
    node: &SenderId,
    sha256: &[u8; 32],
) -> [u8; MESSAGE_LEN] {
    let mut m = [0u8; MESSAGE_LEN];
    let (tag, rest) = m.split_at_mut(MESSAGE_TAG.len());
    tag.copy_from_slice(MESSAGE_TAG);
    let (l, rest) = rest.split_at_mut(4);
    l.copy_from_slice(&len.to_le_bytes());
    let (v, rest) = rest.split_at_mut(4);
    v.copy_from_slice(&version.0.to_le_bytes());
    let (s, rest) = rest.split_at_mut(1);
    s[0] = slot as u8;
    let (n, digest) = rest.split_at_mut(8);
    n.copy_from_slice(&node.0);
    digest.copy_from_slice(sha256);
    m
}

/// A parsed `sys/fw/*` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command<'a> {
    /// `sys/fw/info`: empty body.
    Info,
    /// `sys/fw/begin`: `[len u32][version u32][sha256 32]`.
    Begin {
        /// Image length in bytes.
        len: u32,
        /// Image version (`major << 16 | minor` of its IMAGE_DEF).
        version: Version,
        /// SHA-256 of the image.
        sha256: [u8; 32],
    },
    /// `sys/fw/chunk`: `[offset u32][1..=CHUNK_LEN bytes]`.
    Chunk {
        /// Byte offset in the image.
        offset: u32,
        /// Image bytes.
        data: &'a [u8],
    },
    /// `sys/fw/commit`: empty, or `[signature 64]`.
    Commit {
        /// Signature over [`signed_message`].
        signature: Option<Signature>,
    },
    /// `sys/fw/reboot`: empty body.
    Reboot,
    /// `sys/fw/confirm`: empty body.
    Confirm,
    /// `sys/fw/abort`: empty body.
    Abort,
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let b = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

impl<'a> Command<'a> {
    /// Parses a request. `Ok(None)` when `key` is not under `sys/fw/` (the
    /// payload loader or the application handles it); an unknown
    /// `sys/fw/` key is `NotFound`, a bad body `InvalidRequest`.
    pub fn parse(key: Key<'_>, body: &'a [u8]) -> Result<Option<Self>, HandlerError> {
        let bad = HandlerError::InvalidRequest;
        let k = key.as_str();
        let Some(op) = k.strip_prefix("sys/fw/") else {
            return Ok(None);
        };
        let empty = |c| {
            if body.is_empty() {
                Ok(Some(c))
            } else {
                Err(bad)
            }
        };
        match op {
            "info" => empty(Command::Info),
            "reboot" => empty(Command::Reboot),
            "confirm" => empty(Command::Confirm),
            "abort" => empty(Command::Abort),
            "begin" => {
                if body.len() != BEGIN_LEN {
                    return Err(bad);
                }
                let len = u32_at(body, 0).ok_or(bad)?;
                let version = Version(u32_at(body, 4).ok_or(bad)?);
                let sha256 = body
                    .get(8..40)
                    .and_then(|s| <[u8; 32]>::try_from(s).ok())
                    .ok_or(bad)?;
                Ok(Some(Command::Begin {
                    len,
                    version,
                    sha256,
                }))
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
                0 => Ok(Some(Command::Commit { signature: None })),
                SIGNATURE_LEN => {
                    let s = <[u8; SIGNATURE_LEN]>::try_from(body).map_err(|_| bad)?;
                    Ok(Some(Command::Commit {
                        signature: Some(Signature(s)),
                    }))
                }
                _ => Err(bad),
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
            Command::Reboot => keys::REBOOT,
            Command::Confirm => keys::CONFIRM,
            Command::Abort => keys::ABORT,
        }
    }

    /// Writes the request body into `out`, returning its length; `None`
    /// when `out` is too small.
    pub fn encode_body(&self, out: &mut [u8]) -> Option<usize> {
        match self {
            Command::Begin {
                len,
                version,
                sha256,
            } => {
                out.get_mut(..4)?.copy_from_slice(&len.to_le_bytes());
                out.get_mut(4..8)?.copy_from_slice(&version.0.to_le_bytes());
                out.get_mut(8..40)?.copy_from_slice(sha256);
                Some(BEGIN_LEN)
            }
            Command::Chunk { offset, data } => {
                out.get_mut(..4)?.copy_from_slice(&offset.to_le_bytes());
                out.get_mut(4..4 + data.len())?.copy_from_slice(data);
                Some(4 + data.len())
            }
            Command::Commit { signature: Some(s) } => {
                out.get_mut(..SIGNATURE_LEN)?.copy_from_slice(&s.0);
                Some(SIGNATURE_LEN)
            }
            Command::Info
            | Command::Commit { signature: None }
            | Command::Reboot
            | Command::Confirm
            | Command::Abort => Some(0),
        }
    }
}

/// The `state` byte of `sys/fw/info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StateCode {
    /// No update in progress.
    Idle = 0,
    /// Receiving chunks.
    Receiving = 1,
    /// The image is verified and authorized; installing it failed and may be
    /// retried with another commit.
    Verified = 2,
    /// Installed in the inactive slot, awaiting the flash-update reboot.
    Committed = 3,
    /// Running the new image on trial, awaiting confirm.
    Trial = 4,
    /// The new image was bought and is permanent.
    Confirmed = 5,
    /// The new image did not stick (trial not confirmed); the previous one
    /// runs.
    RolledBack = 6,
}

impl StateCode {
    /// Parses the state byte.
    pub const fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(StateCode::Idle),
            1 => Some(StateCode::Receiving),
            2 => Some(StateCode::Verified),
            3 => Some(StateCode::Committed),
            4 => Some(StateCode::Trial),
            5 => Some(StateCode::Confirmed),
            6 => Some(StateCode::RolledBack),
            _ => None,
        }
    }

    /// Lower-case name, as `mwctl` prints it.
    pub const fn name(self) -> &'static str {
        match self {
            StateCode::Idle => "idle",
            StateCode::Receiving => "receiving",
            StateCode::Verified => "verified",
            StateCode::Committed => "committed",
            StateCode::Trial => "trial",
            StateCode::Confirmed => "confirmed",
            StateCode::RolledBack => "rolled back",
        }
    }
}

/// What a slot holds, as its IMAGE_DEF says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SlotInfo {
    /// `major << 16 | minor` of a valid RP2350 executable IMAGE_DEF, `None`
    /// when the slot holds no image.
    pub version: Option<Version>,
    /// The image is still flagged try-before-you-buy (not bought).
    pub tbyb: bool,
}

/// `sys/fw/info` response (36 bytes):
/// `[state u8][running slot u8][flags u8][version A u32][version B u32]`
/// `[min_version u32][node id 8][target slot u8, 0xff none][target version u32]`
/// `[target len u32][received u32]`. Flags: bit 0 slot A holds an image,
/// bit 1 A is TBYB, bit 2 B holds an image, bit 3 B is TBYB, bit 4 the
/// self-test passed (a trial may be confirmed). A slot's version is 0 when
/// it holds no image. Decoders ignore trailing bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FwInfo {
    /// Update state.
    pub state: StateCode,
    /// The slot the running firmware booted from.
    pub running: Slot,
    /// Slots A and B.
    pub slots: [SlotInfo; 2],
    /// The anti-rollback floor: a new image's version must exceed it (and
    /// the running image's).
    pub min_version: Version,
    /// The device node id the commit signature must name.
    pub node: SenderId,
    /// The slot of the update in progress (receiving, committed, on trial,
    /// confirmed or rolled back).
    pub target: Option<Slot>,
    /// That update's version (0 when unknown).
    pub version: Version,
    /// That update's length (0 when unknown).
    pub len: u32,
    /// Bytes received so far (while receiving; `len` once complete).
    pub received: u32,
    /// The firmware's self-test passed.
    pub self_test_ok: bool,
}

impl FwInfo {
    /// Encodes into `out`; `None` if it is shorter than [`INFO_LEN`].
    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        let out = out.get_mut(..INFO_LEN)?;
        let flag = |s: &SlotInfo, valid: u8, tbyb: u8| {
            (if s.version.is_some() { valid } else { 0 }) | if s.tbyb { tbyb } else { 0 }
        };
        out[0] = self.state as u8;
        out[1] = self.running as u8;
        out[2] = flag(&self.slots[0], 1, 2)
            | flag(&self.slots[1], 4, 8)
            | if self.self_test_ok { 16 } else { 0 };
        let v = |s: &SlotInfo| s.version.map_or(0, |v| v.0);
        out[3..7].copy_from_slice(&v(&self.slots[0]).to_le_bytes());
        out[7..11].copy_from_slice(&v(&self.slots[1]).to_le_bytes());
        out[11..15].copy_from_slice(&self.min_version.0.to_le_bytes());
        out[15..23].copy_from_slice(&self.node.0);
        out[23] = self.target.map_or(NO_SLOT, |s| s as u8);
        out[24..28].copy_from_slice(&self.version.0.to_le_bytes());
        out[28..32].copy_from_slice(&self.len.to_le_bytes());
        out[32..36].copy_from_slice(&self.received.to_le_bytes());
        Some(INFO_LEN)
    }

    /// Decodes a `sys/fw/info` response, ignoring trailing bytes.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let b = bytes.get(..INFO_LEN)?;
        let flags = b[2];
        if flags & !0x1f != 0 {
            return None;
        }
        let slot = |valid: u8, tbyb: u8, at: usize| -> Option<SlotInfo> {
            Some(SlotInfo {
                version: (flags & valid != 0).then_some(Version(u32_at(b, at)?)),
                tbyb: flags & tbyb != 0,
            })
        };
        let target = match b[23] {
            NO_SLOT => None,
            s => Some(Slot::from_byte(s)?),
        };
        Some(FwInfo {
            state: StateCode::from_byte(b[0])?,
            running: Slot::from_byte(b[1])?,
            slots: [slot(1, 2, 3)?, slot(4, 8, 7)?],
            min_version: Version(u32_at(b, 11)?),
            node: SenderId(<[u8; 8]>::try_from(b.get(15..23)?).ok()?),
            target,
            version: Version(u32_at(b, 24)?),
            len: u32_at(b, 28)?,
            received: u32_at(b, 32)?,
            self_test_ok: flags & 16 != 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn y1_commands_round_trip_through_parse() {
        let data = [9u8; CHUNK_LEN];
        for cmd in [
            Command::Info,
            Command::Begin {
                len: 0x0012_3456,
                version: Version(0x0002_0003),
                sha256: [7; 32],
            },
            Command::Chunk {
                offset: 4096,
                data: &data,
            },
            Command::Chunk {
                offset: 0,
                data: &data[..1],
            },
            Command::Commit { signature: None },
            Command::Commit {
                signature: Some(Signature([0xAB; 64])),
            },
            Command::Reboot,
            Command::Confirm,
            Command::Abort,
        ] {
            let mut body = [0u8; 4 + CHUNK_LEN];
            let n = cmd.encode_body(&mut body).unwrap();
            assert_eq!(Command::parse(cmd.key(), &body[..n]), Ok(Some(cmd)));
        }
        assert_eq!(Command::Info.encode_body(&mut []), Some(0));
        assert_eq!(
            Command::Commit {
                signature: Some(Signature([0; 64]))
            }
            .encode_body(&mut [0; 63]),
            None
        );
        let keys: [&str; 7] = core::array::from_fn(|i| keys::ALL[i].as_str());
        assert_eq!(
            keys,
            [
                "sys/fw/info",
                "sys/fw/begin",
                "sys/fw/chunk",
                "sys/fw/commit",
                "sys/fw/reboot",
                "sys/fw/confirm",
                "sys/fw/abort"
            ]
        );
    }

    /// Every body length other than the specified ones is refused, unknown
    /// `sys/fw/` keys are `NotFound`, and other keys are not ours.
    #[test]
    fn y1_parse_refuses_bad_bodies_without_panicking() {
        let junk = [0x5Au8; 4 + CHUNK_LEN + 2];
        for key in keys::ALL {
            for len in 0..junk.len() {
                let r = Command::parse(key, &junk[..len]);
                let ok = match key.as_str() {
                    "sys/fw/begin" => len == BEGIN_LEN,
                    "sys/fw/chunk" => (5..=4 + CHUNK_LEN).contains(&len),
                    "sys/fw/commit" => len == 0 || len == SIGNATURE_LEN,
                    _ => len == 0,
                };
                if ok {
                    assert!(matches!(r, Ok(Some(_))), "{key:?} {len}");
                } else {
                    assert_eq!(r, Err(HandlerError::InvalidRequest), "{key:?} {len}");
                }
            }
        }
        assert_eq!(
            Command::parse(Key::from_static("sys/fw/flash"), &[]),
            Err(HandlerError::NotFound)
        );
        assert_eq!(
            Command::parse(Key::from_static("sys/loader/begin"), &[]),
            Ok(None)
        );
        assert_eq!(
            Command::parse(Key::from_static("app/fw/info"), &[]),
            Ok(None)
        );
        assert_eq!(Command::parse(Key::from_static("sys/fwx"), &[]), Ok(None));
    }

    #[test]
    fn y4_signed_message_layout() {
        let node = SenderId([1, 2, 3, 4, 5, 6, 7, 8]);
        let m = signed_message(
            0x0102_0304,
            Version(0x0005_0006),
            Slot::B,
            &node,
            &[0xEE; 32],
        );
        assert_eq!(MESSAGE_LEN, 64);
        assert_eq!(&m[..15], b"mw-firmware-v1\0");
        assert_eq!(&m[15..19], &[4, 3, 2, 1]);
        assert_eq!(&m[19..23], &[6, 0, 5, 0]);
        assert_eq!(m[23], 1);
        assert_eq!(&m[24..32], &node.0);
        assert_eq!(&m[32..], &[0xEE; 32]);
        // The tags of the three signed messages differ, so no signature is
        // valid for two of them.
        assert_ne!(&m[..13], &crate::loader::trust::MESSAGE_TAG[..13]);
        assert_ne!(&m[..13], &crate::loader::trust::CONTROL_TAG[..13]);
    }

    #[test]
    fn y1_info_round_trips() {
        let info = FwInfo {
            state: StateCode::Trial,
            running: Slot::B,
            slots: [
                SlotInfo {
                    version: Some(Version(0x0001_0000)),
                    tbyb: false,
                },
                SlotInfo {
                    version: Some(Version(0x0001_0001)),
                    tbyb: true,
                },
            ],
            min_version: Version(0x0001_0001),
            node: SenderId([9; 8]),
            target: Some(Slot::B),
            version: Version(0x0001_0001),
            len: 70_000,
            received: 70_000,
            self_test_ok: true,
        };
        let mut buf = [0u8; INFO_LEN + 4];
        assert_eq!(info.encode(&mut buf), Some(INFO_LEN));
        assert_eq!(FwInfo::decode(&buf), Some(info), "trailing bytes ignored");
        assert_eq!(FwInfo::decode(&buf[..INFO_LEN - 1]), None);
        assert!(info.encode(&mut [0; INFO_LEN - 1]).is_none());
        let empty = FwInfo {
            state: StateCode::Idle,
            slots: [SlotInfo::default(); 2],
            target: None,
            self_test_ok: false,
            ..info
        };
        empty.encode(&mut buf).unwrap();
        assert_eq!(FwInfo::decode(&buf), Some(empty));
        for bad in [(0usize, 7u8), (1, 2), (2, 0x20), (23, 2)] {
            let mut b = buf;
            b[bad.0] = bad.1;
            assert_eq!(FwInfo::decode(&b), None, "{bad:?}");
        }
        for s in 0..=6u8 {
            assert_eq!(StateCode::from_byte(s).map(|c| c as u8), Some(s));
            assert!(!StateCode::from_byte(s).unwrap().name().is_empty());
        }
    }
}
