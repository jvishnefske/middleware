//! Transport messages of the client subset and the batch they travel in.
//!
//! | message | id | flags |
//! |---------|----|-------|
//! | [`InitSyn`] / [`InitAck`] | `0x01` | A (ack), S (size params), Z |
//! | [`OpenSyn`] / [`OpenAck`] | `0x02` | A (ack), T (lease in s), Z |
//! | [`Close`] | `0x03` | S (whole session), Z |
//! | KeepAlive | `0x04` | Z |
//! | [`FrameHeader`] | `0x05` | R (reliable), Z |
//!
//! A **batch** is a sequence of transport messages; a Frame is followed by
//! network messages until the next byte is not a network message id
//! (network ids `0x19..=0x1f` never collide with transport ids). On TCP each
//! batch is preceded by its length, 2 bytes little endian
//! ([`LEN_PREFIX`]). [`Batch`] walks one batch item by item with bounded
//! work. Fragments are reported ([`Item::Fragment`]) and end the batch
//! (this client never reassembles); a transport OAM is skipped; `Join`
//! (multicast) is [`CodecError::Invalid`].
//!
//! Extensions this client does not use (shared memory, authentication,
//! multilink, low latency, compression, region name, remote bound) are
//! skipped on decode and never sent.

use super::network::{self, NetworkMessage};
use super::{
    CodecError, Reader, WhatAmI, Writer, Zid, has, mid, read_ext, skip_exts, write_ext_unit,
    write_ext_z64,
};

/// Transport message ids.
pub mod id {
    /// Operations and management.
    pub const OAM: u8 = 0x00;
    /// InitSyn / InitAck.
    pub const INIT: u8 = 0x01;
    /// OpenSyn / OpenAck.
    pub const OPEN: u8 = 0x02;
    /// Close.
    pub const CLOSE: u8 = 0x03;
    /// KeepAlive.
    pub const KEEP_ALIVE: u8 = 0x04;
    /// Frame.
    pub const FRAME: u8 = 0x05;
    /// Fragment.
    pub const FRAGMENT: u8 = 0x06;
    /// Join (multicast only).
    pub const JOIN: u8 = 0x07;
}

const FLAG_A: u8 = 1 << 5;
const FLAG_S: u8 = 1 << 6;
const FLAG_T: u8 = 1 << 6;
const FLAG_R: u8 = 1 << 5;
const FLAG_CLOSE_S: u8 = 1 << 5;
const FLAG_Z: u8 = 1 << 7;

const EXT_QOS: u8 = 0x01;
const EXT_PATCH: u8 = 0x27;
const EXT_FRAME_QOS: u8 = 0x31;

/// Bytes of the TCP length prefix before every batch.
pub const LEN_PREFIX: usize = 2;
/// Batch size Zenoh assumes when the S flag is absent.
pub const DEFAULT_BATCH: u16 = u16::MAX;
/// Close reasons.
pub mod reason {
    /// Generic.
    pub const GENERIC: u8 = 0x00;
    /// Unsupported.
    pub const UNSUPPORTED: u8 = 0x01;
    /// Invalid message.
    pub const INVALID: u8 = 0x02;
    /// Lease expired.
    pub const EXPIRED: u8 = 0x05;
}

/// Sequence number and request id resolutions, two bits each
/// (`|x|x|x|x|rid|fsn|`: 0 = 8 bits, 1 = 16, 2 = 32, 3 = 64).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution(pub u8);

impl Resolution {
    /// Zenoh's default: 32-bit frame sequence numbers and request ids.
    pub const DEFAULT: Resolution = Resolution(0b1010);

    /// Bits of the frame sequence number field (0..=3).
    pub const fn frame_sn(&self) -> u8 {
        self.0 & 0b11
    }

    /// Bits of the request id field (0..=3).
    pub const fn request_id(&self) -> u8 {
        (self.0 >> 2) & 0b11
    }

    /// The largest frame sequence number plus one, as a mask: Zenoh keeps
    /// one bit spare per encoded byte (8 bits: `0x7f`, 16: `0x3fff`, 32:
    /// `0x0fff_ffff`, 64: `0xffff_ffff` in this 32-bit implementation).
    pub const fn sn_mask(&self) -> u32 {
        match self.frame_sn() {
            0 => 0x7f,
            1 => 0x3fff,
            2 => 0x0fff_ffff,
            _ => u32::MAX,
        }
    }

    /// Mask of request ids.
    pub const fn rid_mask(&self) -> u32 {
        match self.request_id() {
            0 => 0xff,
            1 => 0xffff,
            _ => u32::MAX,
        }
    }
}

/// The first handshake message, client to router.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitSyn {
    /// Protocol version ([`super::VERSION`]).
    pub version: u8,
    /// The sender's role.
    pub whatami: WhatAmI,
    /// The sender's id.
    pub zid: Zid,
    /// Proposed resolutions.
    pub resolution: Resolution,
    /// Largest batch the sender accepts.
    pub batch_size: u16,
    /// QoS extension (unit) present.
    pub qos: bool,
    /// Patch extension value (0 = absent).
    pub patch: u8,
}

/// The router's answer to [`InitSyn`], with the cookie to echo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitAck<'a> {
    /// Protocol version.
    pub version: u8,
    /// The router's role.
    pub whatami: WhatAmI,
    /// The router's id.
    pub zid: Zid,
    /// Agreed resolutions.
    pub resolution: Resolution,
    /// Agreed batch size.
    pub batch_size: u16,
    /// Opaque state to send back in [`OpenSyn`].
    pub cookie: &'a [u8],
    /// QoS extension (unit) present.
    pub qos: bool,
    /// Patch extension value (0 = absent).
    pub patch: u8,
}

/// The second handshake message, client to router.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenSyn<'a> {
    /// The sender's lease in milliseconds.
    pub lease_ms: u64,
    /// The first sequence number the sender will use.
    pub initial_sn: u32,
    /// The cookie from [`InitAck`].
    pub cookie: &'a [u8],
    /// QoS extension (unit) present.
    pub qos: bool,
}

/// The router's answer to [`OpenSyn`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenAck {
    /// The router's lease in milliseconds.
    pub lease_ms: u64,
    /// The first sequence number the router will use.
    pub initial_sn: u32,
    /// QoS extension (unit) present.
    pub qos: bool,
}

/// Closes the link (or the whole session).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Close {
    /// One of [`reason`].
    pub reason: u8,
    /// Close the session, not just this link.
    pub session: bool,
}

/// The header of a Frame; network messages follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// Reliable channel (else best effort).
    pub reliable: bool,
    /// Sequence number in that channel.
    pub sn: u32,
    /// QoS extension byte (priority); [`FrameHeader::DEFAULT_QOS`] omits it.
    pub qos: u8,
}

impl FrameHeader {
    /// The default frame QoS (priority `Data`), not sent.
    pub const DEFAULT_QOS: u8 = 5;
}

/// A fragment header; its payload is the rest of the batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FragmentHeader {
    /// Reliable channel.
    pub reliable: bool,
    /// More fragments follow.
    pub more: bool,
    /// Sequence number.
    pub sn: u32,
}

/// A transport message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportMessage<'a> {
    /// See [`InitSyn`].
    InitSyn(InitSyn),
    /// See [`InitAck`].
    InitAck(InitAck<'a>),
    /// See [`OpenSyn`].
    OpenSyn(OpenSyn<'a>),
    /// See [`OpenAck`].
    OpenAck(OpenAck),
    /// See [`Close`].
    Close(Close),
    /// Keeps the lease alive.
    KeepAlive,
    /// See [`FrameHeader`].
    Frame(FrameHeader),
}

fn write_size_params(
    w: &mut Writer<'_>,
    resolution: Resolution,
    batch: u16,
) -> Result<(), CodecError> {
    w.u8(resolution.0)?;
    w.bytes(&batch.to_le_bytes())
}

fn init_header(ack: bool, resolution: Resolution, batch: u16, exts: bool) -> u8 {
    let mut h = id::INIT;
    if ack {
        h |= FLAG_A;
    }
    if resolution != Resolution::DEFAULT || batch != DEFAULT_BATCH {
        h |= FLAG_S;
    }
    if exts {
        h |= FLAG_Z;
    }
    h
}

fn write_role_and_zid(w: &mut Writer<'_>, whatami: WhatAmI, zid: Zid) -> Result<(), CodecError> {
    w.u8(((zid.size() as u8).wrapping_sub(1) << 4) | whatami.bits())?;
    w.bytes(zid.as_wire())
}

fn write_init_exts(w: &mut Writer<'_>, qos: bool, patch: u8) -> Result<(), CodecError> {
    if qos {
        write_ext_unit(w, EXT_QOS, patch != 0)?;
    }
    if patch != 0 {
        write_ext_z64(w, EXT_PATCH, u64::from(patch), false)?;
    }
    Ok(())
}

/// The fields InitSyn and InitAck share, read after the header.
struct InitCommon {
    version: u8,
    whatami: WhatAmI,
    zid: Zid,
    resolution: Resolution,
    batch_size: u16,
}

fn read_init_common(r: &mut Reader<'_>, header: u8) -> Result<InitCommon, CodecError> {
    let version = r.u8()?;
    let flags = r.u8()?;
    let whatami = WhatAmI::from_bits(flags)?;
    let zid = Zid::read(r, 1 + usize::from(flags >> 4))?;
    let (resolution, batch_size) = if has(header, FLAG_S) {
        let res = Resolution(r.u8()? & 0b0011_1111);
        (res, u16::from_le_bytes(r.array::<2>()?))
    } else {
        (Resolution::DEFAULT, DEFAULT_BATCH)
    };
    Ok(InitCommon {
        version,
        whatami,
        zid,
        resolution,
        batch_size,
    })
}

/// Reads Init/Open extensions, keeping QoS (unit, id 1) and patch (z64,
/// id 7 for Init); every other non-mandatory one is skipped.
fn read_handshake_exts(
    r: &mut Reader<'_>,
    present: bool,
    init: bool,
) -> Result<(bool, u8), CodecError> {
    let (mut qos, mut patch) = (false, 0u8);
    let mut more = present;
    while more {
        let (e, m) = read_ext(r)?;
        match e.id {
            EXT_QOS => qos = true,
            EXT_PATCH if init => patch = e.z64()? as u8,
            _ => e.skip()?,
        }
        more = m;
    }
    Ok((qos, patch))
}

impl InitSyn {
    /// Writes the message.
    pub fn write(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
        let exts = self.qos || self.patch != 0;
        let h = init_header(false, self.resolution, self.batch_size, exts);
        w.u8(h)?;
        w.u8(self.version)?;
        write_role_and_zid(w, self.whatami, self.zid)?;
        if has(h, FLAG_S) {
            write_size_params(w, self.resolution, self.batch_size)?;
        }
        write_init_exts(w, self.qos, self.patch)
    }

    fn read(r: &mut Reader<'_>, header: u8) -> Result<Self, CodecError> {
        let c = read_init_common(r, header)?;
        let (qos, patch) = read_handshake_exts(r, has(header, FLAG_Z), true)?;
        Ok(InitSyn {
            version: c.version,
            whatami: c.whatami,
            zid: c.zid,
            resolution: c.resolution,
            batch_size: c.batch_size,
            qos,
            patch,
        })
    }
}

impl<'a> InitAck<'a> {
    /// Writes the message.
    pub fn write(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
        let exts = self.qos || self.patch != 0;
        let h = init_header(true, self.resolution, self.batch_size, exts);
        w.u8(h)?;
        w.u8(self.version)?;
        write_role_and_zid(w, self.whatami, self.zid)?;
        if has(h, FLAG_S) {
            write_size_params(w, self.resolution, self.batch_size)?;
        }
        if self.cookie.len() > usize::from(u16::MAX) {
            return Err(CodecError::Overflow);
        }
        w.bytes_z(self.cookie)?;
        write_init_exts(w, self.qos, self.patch)
    }

    fn read(r: &mut Reader<'a>, header: u8) -> Result<Self, CodecError> {
        let c = read_init_common(r, header)?;
        let cookie = r.bytes_z(u64::from(u16::MAX))?;
        let (qos, patch) = read_handshake_exts(r, has(header, FLAG_Z), true)?;
        Ok(InitAck {
            version: c.version,
            whatami: c.whatami,
            zid: c.zid,
            resolution: c.resolution,
            batch_size: c.batch_size,
            cookie,
            qos,
            patch,
        })
    }
}

fn write_lease(w: &mut Writer<'_>, lease_ms: u64, seconds: bool) -> Result<(), CodecError> {
    if seconds {
        w.zint(lease_ms / 1000)
    } else {
        w.zint(lease_ms)
    }
}

fn read_lease(r: &mut Reader<'_>, header: u8) -> Result<u64, CodecError> {
    let v = r.zint()?;
    if has(header, FLAG_T) {
        v.checked_mul(1000).ok_or(CodecError::Overflow)
    } else {
        Ok(v)
    }
}

impl<'a> OpenSyn<'a> {
    /// Writes the message (lease in seconds when it is a whole number of
    /// seconds, as Zenoh does).
    pub fn write(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
        let seconds = self.lease_ms.is_multiple_of(1000);
        let mut h = id::OPEN;
        if seconds {
            h |= FLAG_T;
        }
        if self.qos {
            h |= FLAG_Z;
        }
        w.u8(h)?;
        write_lease(w, self.lease_ms, seconds)?;
        w.zint(u64::from(self.initial_sn))?;
        w.bytes_z(self.cookie)?;
        if self.qos {
            write_ext_unit(w, EXT_QOS, false)?;
        }
        Ok(())
    }

    fn read(r: &mut Reader<'a>, header: u8) -> Result<Self, CodecError> {
        let lease_ms = read_lease(r, header)?;
        // zenoh-codec keeps the low 32 bits of the sequence number.
        let initial_sn = r.zint()? as u32;
        let cookie = r.bytes_z(u64::from(u16::MAX))?;
        let (qos, _) = read_handshake_exts(r, has(header, FLAG_Z), false)?;
        Ok(OpenSyn {
            lease_ms,
            initial_sn,
            cookie,
            qos,
        })
    }
}

impl OpenAck {
    /// Writes the message.
    pub fn write(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
        let seconds = self.lease_ms.is_multiple_of(1000);
        let mut h = id::OPEN | FLAG_A;
        if seconds {
            h |= FLAG_T;
        }
        if self.qos {
            h |= FLAG_Z;
        }
        w.u8(h)?;
        write_lease(w, self.lease_ms, seconds)?;
        w.zint(u64::from(self.initial_sn))?;
        if self.qos {
            write_ext_unit(w, EXT_QOS, false)?;
        }
        Ok(())
    }

    fn read(r: &mut Reader<'_>, header: u8) -> Result<Self, CodecError> {
        let lease_ms = read_lease(r, header)?;
        let initial_sn = r.zint()? as u32;
        let (qos, _) = read_handshake_exts(r, has(header, FLAG_Z), false)?;
        Ok(OpenAck {
            lease_ms,
            initial_sn,
            qos,
        })
    }
}

impl Close {
    /// Writes the message.
    pub fn write(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
        w.u8(if self.session {
            id::CLOSE | FLAG_CLOSE_S
        } else {
            id::CLOSE
        })?;
        w.u8(self.reason)
    }

    fn read(r: &mut Reader<'_>, header: u8) -> Result<Self, CodecError> {
        let reason = r.u8()?;
        skip_exts(r, has(header, FLAG_Z))?;
        Ok(Close {
            reason,
            session: has(header, FLAG_CLOSE_S),
        })
    }
}

/// Writes a KeepAlive.
pub fn write_keep_alive(w: &mut Writer<'_>) -> Result<(), CodecError> {
    w.u8(id::KEEP_ALIVE)
}

impl FrameHeader {
    /// Writes the header; network messages follow.
    pub fn write(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
        let mut h = id::FRAME;
        if self.reliable {
            h |= FLAG_R;
        }
        let qos = self.qos != Self::DEFAULT_QOS;
        if qos {
            h |= FLAG_Z;
        }
        w.u8(h)?;
        w.zint(u64::from(self.sn))?;
        if qos {
            write_ext_z64(w, EXT_FRAME_QOS, u64::from(self.qos), false)?;
        }
        Ok(())
    }

    fn read(r: &mut Reader<'_>, header: u8) -> Result<Self, CodecError> {
        let sn = r.zint()? as u32;
        let mut qos = Self::DEFAULT_QOS;
        let mut more = has(header, FLAG_Z);
        while more {
            let (e, m) = read_ext(r)?;
            match e.id {
                EXT_FRAME_QOS => qos = e.z64()? as u8,
                _ => e.skip()?,
            }
            more = m;
        }
        Ok(FrameHeader {
            reliable: has(header, FLAG_R),
            sn,
            qos,
        })
    }
}

impl<'a> TransportMessage<'a> {
    /// Writes the message.
    pub fn write(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
        match self {
            TransportMessage::InitSyn(m) => m.write(w),
            TransportMessage::InitAck(m) => m.write(w),
            TransportMessage::OpenSyn(m) => m.write(w),
            TransportMessage::OpenAck(m) => m.write(w),
            TransportMessage::Close(m) => m.write(w),
            TransportMessage::KeepAlive => write_keep_alive(w),
            TransportMessage::Frame(m) => m.write(w),
        }
    }
}

/// One step through a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item<'a> {
    /// A transport message (a Frame's network messages follow as
    /// [`Item::Network`]).
    Transport(TransportMessage<'a>),
    /// A network message inside the current frame.
    Network(NetworkMessage<'a>),
    /// A fragment; this client does not reassemble, and the fragment's
    /// payload (the rest of the batch) is skipped.
    Fragment(FragmentHeader),
    /// A transport OAM message, skipped.
    Oam,
}

/// Where a [`Batch`] walk stands; `Copy`, so a caller can try a step and
/// commit it only when the item was consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pos: usize,
    in_frame: bool,
    failed: bool,
}

impl Cursor {
    /// The start of a batch.
    pub const START: Cursor = Cursor {
        pos: 0,
        in_frame: false,
        failed: false,
    };

    /// The byte offset reached in the batch.
    pub const fn pos(&self) -> usize {
        self.pos
    }

    /// True once the batch is fully walked (or stopped on an error).
    pub const fn done(&self, batch: &[u8]) -> bool {
        self.failed || self.pos >= batch.len()
    }
}

/// Walks one batch (without its length prefix).
#[derive(Debug, Clone, Copy)]
pub struct Batch<'a> {
    bytes: &'a [u8],
    cursor: Cursor,
}

impl<'a> Batch<'a> {
    /// A walk over `bytes` from the start.
    pub const fn new(bytes: &'a [u8]) -> Self {
        Batch {
            bytes,
            cursor: Cursor::START,
        }
    }

    /// A walk over `bytes` from `cursor`.
    pub const fn resume(bytes: &'a [u8], cursor: Cursor) -> Self {
        Batch { bytes, cursor }
    }

    /// The position after the items read so far.
    pub const fn cursor(&self) -> Cursor {
        self.cursor
    }

    fn step(&mut self) -> Result<Item<'a>, CodecError> {
        let rest = self
            .bytes
            .get(self.cursor.pos..)
            .ok_or(CodecError::Truncated)?;
        let mut r = Reader::new(rest);
        let header = r.u8()?;
        let item = if network::is_network_id(mid(header)) {
            if !self.cursor.in_frame {
                return Err(CodecError::Invalid);
            }
            Item::Network(NetworkMessage::read(&mut r, header)?)
        } else {
            self.cursor.in_frame = false;
            match mid(header) {
                id::INIT if has(header, FLAG_A) => {
                    Item::Transport(TransportMessage::InitAck(InitAck::read(&mut r, header)?))
                }
                id::INIT => {
                    Item::Transport(TransportMessage::InitSyn(InitSyn::read(&mut r, header)?))
                }
                id::OPEN if has(header, FLAG_A) => {
                    Item::Transport(TransportMessage::OpenAck(OpenAck::read(&mut r, header)?))
                }
                id::OPEN => {
                    Item::Transport(TransportMessage::OpenSyn(OpenSyn::read(&mut r, header)?))
                }
                id::CLOSE => Item::Transport(TransportMessage::Close(Close::read(&mut r, header)?)),
                id::KEEP_ALIVE => {
                    skip_exts(&mut r, has(header, FLAG_Z))?;
                    Item::Transport(TransportMessage::KeepAlive)
                }
                id::FRAME => {
                    let f = FrameHeader::read(&mut r, header)?;
                    self.cursor.in_frame = true;
                    Item::Transport(TransportMessage::Frame(f))
                }
                id::FRAGMENT => {
                    let sn = r.zint()? as u32;
                    skip_exts(&mut r, has(header, FLAG_Z))?;
                    // The payload is the rest of the batch.
                    let rest_len = r.remaining();
                    r.take(rest_len)?;
                    Item::Fragment(FragmentHeader {
                        reliable: has(header, FLAG_R),
                        more: has(header, 1 << 6),
                        sn,
                    })
                }
                id::OAM => {
                    let _id = r.zint_max(u64::from(u16::MAX))?;
                    skip_exts(&mut r, has(header, FLAG_Z))?;
                    skip_oam_body(&mut r, header)?;
                    Item::Oam
                }
                _ => return Err(CodecError::Invalid),
            }
        };
        let used = rest.len() - r.remaining();
        self.cursor.pos = self.cursor.pos.saturating_add(used);
        Ok(item)
    }
}

/// Skips an OAM body, whose encoding is in header bits 5–6.
pub(crate) fn skip_oam_body(r: &mut Reader<'_>, header: u8) -> Result<(), CodecError> {
    match header & super::ext::ENC_MASK {
        super::ext::ENC_UNIT => Ok(()),
        super::ext::ENC_Z64 => r.zint().map(|_| ()),
        super::ext::ENC_ZBUF => r.bytes_z(u64::from(u32::MAX)).map(|_| ()),
        _ => Err(CodecError::Invalid),
    }
}

impl<'a> Iterator for Batch<'a> {
    type Item = Result<Item<'a>, CodecError>;

    /// The next item; after an error, `None`. Every item consumes at least
    /// one byte, so a batch of `n` bytes yields at most `n` items.
    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor.done(self.bytes) {
            return None;
        }
        let item = self.step();
        if item.is_err() {
            self.cursor.failed = true;
        }
        Some(item)
    }
}

/// Writes a TCP batch: reserves the 2-byte length, lets `body` write the
/// messages, then patches the length in. Fails if the batch exceeds
/// `max_batch` bytes or the buffer.
pub fn write_batch<F>(out: &mut [u8], max_batch: u16, body: F) -> Result<usize, CodecError>
where
    F: FnOnce(&mut Writer<'_>) -> Result<(), CodecError>,
{
    let mut w = Writer::new(out);
    w.bytes(&[0, 0])?;
    body(&mut w)?;
    let len = w.len() - LEN_PREFIX;
    let len16 = u16::try_from(len).map_err(|_| CodecError::NoSpace)?;
    if len16 > max_batch {
        return Err(CodecError::NoSpace);
    }
    let prefix = w
        .written_mut()
        .get_mut(..LEN_PREFIX)
        .ok_or(CodecError::NoSpace)?;
    prefix.copy_from_slice(&len16.to_le_bytes());
    Ok(w.len())
}

#[cfg(test)]
mod tests;
