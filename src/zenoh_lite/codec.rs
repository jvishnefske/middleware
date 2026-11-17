//! The Zenoh 1.x wire codec for the client subset: pure, `no_std`, no heap.
//!
//! Decoding borrows from the input batch (`&'a [u8]`, `&'a str`), so a
//! decoded message costs no copy; encoding writes into a caller-provided
//! slice through a [`Writer`] and fails with [`CodecError::NoSpace`]
//! instead of growing. Every read is bounded by the input length and every
//! length field is checked before it is used, so malformed input is an
//! error, never a panic.
//!
//! Byte-for-byte this is the encoding of `zenoh-codec` 1.10 (protocol
//! version [`VERSION`] = 0x09) for the same message; the tests in
//! [`transport`] and [`network`] compare against it.
//!
//! * Primitives (here): [`Reader`], [`Writer`], variable-length integers
//!   ([`zint_len`], [`Reader::zint`], [`Writer::zint`]), extensions
//!   ([`read_ext`], [`Ext`]), and the shared types [`Zid`], [`WhatAmI`],
//!   [`WireExpr`], [`Timestamp`], [`Encoding`], [`EntityGlobalId`].
//! * [`transport`]: InitSyn/Ack, OpenSyn/Ack, KeepAlive, Close, Frame, the
//!   batch iterator ([`transport::Batch`]) and the TCP length prefix.
//! * [`network`]: Push, Request, Response, ResponseFinal, Declare,
//!   Interest, and the bodies Put, Del, Query, Reply, Err.
//!
//! # Variable-length integers
//!
//! Little-endian base 128: seven bits per byte, the high bit set on every
//! byte but the last. A `u64` takes at most 9 bytes; the ninth carries 8
//! bits. Fields with a narrower type are bounded on decode
//! ([`Reader::zint_max`]): a larger value is [`CodecError::Overflow`].
//!
//! # Extensions
//!
//! A message with its Z flag set is followed by extensions, each a header
//! byte (`|Z|ENC|M|ID|`: more-follow flag, encoding unit/z64/zbuf, the
//! mandatory flag and a 4-bit id) and a body. Known extensions are decoded;
//! an unknown one is skipped when its M flag is clear and refused with
//! [`CodecError::Mandatory`] when set, as `zenoh-codec` does. An encoding
//! of `0b11` is [`CodecError::Invalid`].

pub mod network;
pub mod transport;
#[cfg(test)]
mod zref;

/// Zenoh protocol version spoken: 1.x (`zenoh` 1.10).
pub const VERSION: u8 = 0x09;

/// Why bytes could not be decoded or encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecError {
    /// The input ended inside a message.
    Truncated,
    /// An integer is larger than its field allows, or longer than 9 bytes.
    Overflow,
    /// A header, id, flag combination or value this protocol does not
    /// define (or this client refuses: fragments, `Join`).
    Invalid,
    /// An unknown extension marked mandatory.
    Mandatory,
    /// Text that must be UTF-8 is not.
    Utf8,
    /// The output buffer is full.
    NoSpace,
}

/// Header bits of a message id (the low five bits of the header byte).
pub(crate) const MID_MASK: u8 = 0x1f;

/// The message id of a header byte.
pub(crate) const fn mid(header: u8) -> u8 {
    header & MID_MASK
}

/// True if `flag` is set in `byte`.
pub(crate) const fn has(byte: u8, flag: u8) -> bool {
    byte & flag != 0
}

/// Bytes a variable-length integer takes.
pub const fn zint_len(x: u64) -> usize {
    let mut len = 1;
    let mut rest = x >> 7;
    while rest != 0 && len < 9 {
        len += 1;
        rest >>= 7;
    }
    len
}

/// A cursor over input bytes. Every read checks the remaining length.
#[derive(Debug, Clone, Copy)]
pub struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    /// A reader over `bytes`.
    pub const fn new(bytes: &'a [u8]) -> Self {
        Reader { rest: bytes }
    }

    /// Bytes not yet read.
    pub const fn remaining(&self) -> usize {
        self.rest.len()
    }

    /// True when everything has been read.
    pub const fn is_empty(&self) -> bool {
        self.rest.is_empty()
    }

    /// The unread bytes.
    pub const fn rest(&self) -> &'a [u8] {
        self.rest
    }

    /// The next byte without consuming it.
    pub fn peek(&self) -> Option<u8> {
        self.rest.first().copied()
    }

    /// One byte.
    pub fn u8(&mut self) -> Result<u8, CodecError> {
        let (&b, rest) = self.rest.split_first().ok_or(CodecError::Truncated)?;
        self.rest = rest;
        Ok(b)
    }

    /// The next `n` bytes.
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        let (head, rest) = self.rest.split_at_checked(n).ok_or(CodecError::Truncated)?;
        self.rest = rest;
        Ok(head)
    }

    /// The next `N` bytes as an array.
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], CodecError> {
        let (head, rest) = self
            .rest
            .split_first_chunk::<N>()
            .ok_or(CodecError::Truncated)?;
        self.rest = rest;
        Ok(*head)
    }

    /// A variable-length integer (at most 9 bytes).
    pub fn zint(&mut self) -> Result<u64, CodecError> {
        let mut value: u64 = 0;
        let mut shift = 0u32;
        // Bytes 1..=8 carry 7 bits each; the ninth carries 8.
        for _ in 0..8 {
            let b = self.u8()?;
            value |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
        }
        let b = self.u8()?;
        Ok(value | (u64::from(b) << 56))
    }

    /// A variable-length integer no larger than `max`.
    pub fn zint_max(&mut self, max: u64) -> Result<u64, CodecError> {
        let v = self.zint()?;
        if v > max {
            return Err(CodecError::Overflow);
        }
        Ok(v)
    }

    /// A variable-length integer that fits a `u32`.
    pub fn zu32(&mut self) -> Result<u32, CodecError> {
        u32::try_from(self.zint()?).map_err(|_| CodecError::Overflow)
    }

    /// A variable-length integer that fits a `u16`.
    pub fn zu16(&mut self) -> Result<u16, CodecError> {
        u16::try_from(self.zint()?).map_err(|_| CodecError::Overflow)
    }

    /// A length-prefixed byte string of at most `max` bytes.
    pub fn bytes_z(&mut self, max: u64) -> Result<&'a [u8], CodecError> {
        let len = self.zint_max(max)?;
        let len = usize::try_from(len).map_err(|_| CodecError::Overflow)?;
        self.take(len)
    }

    /// A length-prefixed UTF-8 string of at most `max` bytes.
    pub fn str_z(&mut self, max: u64) -> Result<&'a str, CodecError> {
        core::str::from_utf8(self.bytes_z(max)?).map_err(|_| CodecError::Utf8)
    }
}

/// A cursor writing into a caller-provided buffer; never grows it.
#[derive(Debug)]
pub struct Writer<'b> {
    buf: &'b mut [u8],
    len: usize,
}

impl<'b> Writer<'b> {
    /// A writer at the start of `buf`.
    pub fn new(buf: &'b mut [u8]) -> Self {
        Writer { buf, len: 0 }
    }

    /// Bytes written so far.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// True if nothing was written.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The written bytes.
    pub fn written(&self) -> &[u8] {
        self.buf.get(..self.len).unwrap_or(&[])
    }

    /// Mutable access to the written bytes (to patch a length prefix).
    pub fn written_mut(&mut self) -> &mut [u8] {
        self.buf.get_mut(..self.len).unwrap_or(&mut [])
    }

    /// One byte.
    pub fn u8(&mut self, b: u8) -> Result<(), CodecError> {
        let slot = self.buf.get_mut(self.len).ok_or(CodecError::NoSpace)?;
        *slot = b;
        self.len += 1;
        Ok(())
    }

    /// Raw bytes.
    pub fn bytes(&mut self, bytes: &[u8]) -> Result<(), CodecError> {
        let end = self
            .len
            .checked_add(bytes.len())
            .ok_or(CodecError::NoSpace)?;
        let slot = self.buf.get_mut(self.len..end).ok_or(CodecError::NoSpace)?;
        slot.copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }

    /// A variable-length integer.
    pub fn zint(&mut self, mut x: u64) -> Result<(), CodecError> {
        for _ in 0..8 {
            if x < 0x80 {
                return self.u8(x as u8);
            }
            self.u8((x as u8 & 0x7f) | 0x80)?;
            x >>= 7;
        }
        // Ninth byte: the remaining 8 bits.
        self.u8(x as u8)
    }

    /// A length-prefixed byte string.
    pub fn bytes_z(&mut self, bytes: &[u8]) -> Result<(), CodecError> {
        self.zint(bytes.len() as u64)?;
        self.bytes(bytes)
    }
}

/// Extension encodings (bits 5–6 of the extension header).
pub mod ext {
    /// No body.
    pub const ENC_UNIT: u8 = 0b00 << 5;
    /// A variable-length integer body.
    pub const ENC_Z64: u8 = 0b01 << 5;
    /// A length-prefixed byte body.
    pub const ENC_ZBUF: u8 = 0b10 << 5;
    /// Mask of the encoding bits.
    pub const ENC_MASK: u8 = 0b11 << 5;
    /// The mandatory flag.
    pub const FLAG_M: u8 = 1 << 4;
    /// More extensions follow.
    pub const FLAG_Z: u8 = 1 << 7;
}

/// The body of an extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtBody<'a> {
    /// No body.
    Unit,
    /// An integer.
    Z64(u64),
    /// Bytes.
    ZBuf(&'a [u8]),
}

/// One decoded extension: its header without the Z flag (id, mandatory
/// flag and encoding) and its body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ext<'a> {
    /// Header byte with the Z (more) flag cleared.
    pub id: u8,
    /// The body.
    pub body: ExtBody<'a>,
}

impl<'a> Ext<'a> {
    /// True if the M flag is set.
    pub const fn is_mandatory(&self) -> bool {
        has(self.id, ext::FLAG_M)
    }

    /// Refuses this extension if it is mandatory (for unknown extensions).
    pub const fn skip(&self) -> Result<(), CodecError> {
        if self.is_mandatory() {
            Err(CodecError::Mandatory)
        } else {
            Ok(())
        }
    }

    /// The integer body, or [`CodecError::Invalid`].
    pub const fn z64(&self) -> Result<u64, CodecError> {
        match self.body {
            ExtBody::Z64(v) => Ok(v),
            _ => Err(CodecError::Invalid),
        }
    }

    /// The byte body, or [`CodecError::Invalid`].
    pub const fn zbuf(&self) -> Result<&'a [u8], CodecError> {
        match self.body {
            ExtBody::ZBuf(b) => Ok(b),
            _ => Err(CodecError::Invalid),
        }
    }
}

/// Reads one extension; the flag says whether another follows.
pub fn read_ext<'a>(r: &mut Reader<'a>) -> Result<(Ext<'a>, bool), CodecError> {
    let header = r.u8()?;
    let body = match header & ext::ENC_MASK {
        ext::ENC_UNIT => ExtBody::Unit,
        ext::ENC_Z64 => ExtBody::Z64(r.zint()?),
        ext::ENC_ZBUF => ExtBody::ZBuf(r.bytes_z(u64::from(u32::MAX))?),
        _ => return Err(CodecError::Invalid),
    };
    Ok((
        Ext {
            id: header & !ext::FLAG_Z,
            body,
        },
        has(header, ext::FLAG_Z),
    ))
}

/// Skips every extension of a message whose Z flag is set and that knows
/// none, refusing mandatory ones.
pub fn skip_exts(r: &mut Reader<'_>, present: bool) -> Result<(), CodecError> {
    let mut more = present;
    // Each extension consumes at least its header byte, so this ends
    // within `r.remaining()` iterations.
    while more {
        let (e, m) = read_ext(r)?;
        e.skip()?;
        more = m;
    }
    Ok(())
}

/// Writes an extension header.
fn ext_header(w: &mut Writer<'_>, id: u8, more: bool) -> Result<(), CodecError> {
    w.u8(if more { id | ext::FLAG_Z } else { id })
}

/// Writes a unit extension.
pub fn write_ext_unit(w: &mut Writer<'_>, id: u8, more: bool) -> Result<(), CodecError> {
    ext_header(w, id, more)
}

/// Writes an integer extension.
pub fn write_ext_z64(w: &mut Writer<'_>, id: u8, value: u64, more: bool) -> Result<(), CodecError> {
    ext_header(w, id, more)?;
    w.zint(value)
}

/// Writes a byte extension.
pub fn write_ext_zbuf(
    w: &mut Writer<'_>,
    id: u8,
    body: &[u8],
    more: bool,
) -> Result<(), CodecError> {
    ext_header(w, id, more)?;
    w.bytes_z(body)
}

/// A Zenoh id: 1 to 16 bytes, little endian, not zero. Stored in 16 bytes;
/// the wire carries only the significant ones ([`Zid::size`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Zid([u8; 16]);

impl Zid {
    /// Largest size in bytes.
    pub const MAX_SIZE: usize = 16;

    /// An id from its 16 little-endian bytes; `None` if all are zero.
    pub const fn new(bytes: [u8; 16]) -> Option<Zid> {
        if u128::from_le_bytes(bytes) == 0 {
            None
        } else {
            Some(Zid(bytes))
        }
    }

    /// An id from its significant bytes (1 to 16, little endian).
    pub fn from_slice(bytes: &[u8]) -> Option<Zid> {
        let mut id = [0u8; 16];
        id.get_mut(..bytes.len())?.copy_from_slice(bytes);
        Zid::new(id)
    }

    /// The 16 little-endian bytes.
    pub const fn to_le_bytes(&self) -> [u8; 16] {
        self.0
    }

    /// Significant bytes: 16 minus the high zero bytes (at least 1).
    pub const fn size(&self) -> usize {
        16 - (u128::from_le_bytes(self.0).leading_zeros() as usize / 8)
    }

    /// The bytes the wire carries.
    pub fn as_wire(&self) -> &[u8] {
        self.0.get(..self.size()).unwrap_or(&self.0)
    }

    /// Reads `len` bytes as an id.
    pub(crate) fn read(r: &mut Reader<'_>, len: usize) -> Result<Zid, CodecError> {
        if len > Self::MAX_SIZE {
            return Err(CodecError::Invalid);
        }
        Zid::from_slice(r.take(len)?).ok_or(CodecError::Invalid)
    }
}

/// The role of a Zenoh node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhatAmI {
    /// A router.
    Router,
    /// A peer.
    Peer,
    /// A client (this crate's role).
    Client,
}

impl WhatAmI {
    pub(crate) const fn bits(self) -> u8 {
        match self {
            WhatAmI::Router => 0b00,
            WhatAmI::Peer => 0b01,
            WhatAmI::Client => 0b10,
        }
    }

    pub(crate) const fn from_bits(bits: u8) -> Result<WhatAmI, CodecError> {
        match bits & 0b11 {
            0b00 => Ok(WhatAmI::Router),
            0b01 => Ok(WhatAmI::Peer),
            0b10 => Ok(WhatAmI::Client),
            _ => Err(CodecError::Invalid),
        }
    }
}

/// A key expression on the wire: a numeric scope (0 = none, otherwise an
/// id from a `DeclareKeyExpr`) and a suffix. `sender_mapping` says whose
/// declarations the scope refers to (the sender's when true).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireExpr<'a> {
    /// Declared key expression id, 0 for none.
    pub scope: u16,
    /// Text appended to the scope's expression (or the whole expression).
    pub suffix: &'a str,
    /// The scope is one the sender declared (M flag).
    pub sender_mapping: bool,
}

impl<'a> WireExpr<'a> {
    /// A plain key expression: no scope, the whole text as suffix.
    pub const fn plain(expr: &'a str) -> Self {
        WireExpr {
            scope: 0,
            suffix: expr,
            sender_mapping: false,
        }
    }

    /// True if there is a suffix (the N flag).
    pub const fn has_suffix(&self) -> bool {
        !self.suffix.is_empty()
    }

    pub(crate) fn write(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
        w.zint(u64::from(self.scope))?;
        if self.has_suffix() {
            if self.suffix.len() > usize::from(u16::MAX) {
                return Err(CodecError::Overflow);
            }
            w.bytes_z(self.suffix.as_bytes())?;
        }
        Ok(())
    }

    pub(crate) fn read(
        r: &mut Reader<'a>,
        named: bool,
        sender_mapping: bool,
    ) -> Result<Self, CodecError> {
        let scope = r.zu16()?;
        let suffix = if named {
            r.str_z(u64::from(u16::MAX))?
        } else {
            ""
        };
        Ok(WireExpr {
            scope,
            suffix,
            sender_mapping,
        })
    }
}

/// A hybrid logical clock timestamp: NTP64 time and the clock's id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timestamp<'a> {
    /// NTP64 time.
    pub time: u64,
    /// The clock id, 1 to 16 bytes.
    pub id: &'a [u8],
}

impl<'a> Timestamp<'a> {
    pub(crate) fn len(&self) -> usize {
        zint_len(self.time) + zint_len(self.id.len() as u64) + self.id.len()
    }

    pub(crate) fn write(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
        w.zint(self.time)?;
        w.bytes_z(self.id)
    }

    pub(crate) fn read(r: &mut Reader<'a>) -> Result<Self, CodecError> {
        let time = r.zint()?;
        let id = r.bytes_z(Zid::MAX_SIZE as u64)?;
        if id.iter().all(|&b| b == 0) {
            return Err(CodecError::Invalid);
        }
        Ok(Timestamp { time, id })
    }
}

/// A payload encoding: a numeric id and an optional schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Encoding<'a> {
    /// The encoding id (0 is Zenoh's default, raw bytes).
    pub id: u16,
    /// Optional schema text bytes, at most 255.
    pub schema: Option<&'a [u8]>,
}

impl<'a> Encoding<'a> {
    /// The default (empty) encoding, which the wire omits.
    pub const EMPTY: Encoding<'static> = Encoding {
        id: 0,
        schema: None,
    };

    /// True for the default encoding.
    pub const fn is_empty(&self) -> bool {
        self.id == 0 && self.schema.is_none()
    }

    pub(crate) fn len(&self) -> usize {
        let mut len = zint_len(u64::from(self.id) << 1);
        if let Some(s) = self.schema {
            len += zint_len(s.len() as u64) + s.len();
        }
        len
    }

    pub(crate) fn write(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
        let mut id = u64::from(self.id) << 1;
        if self.schema.is_some() {
            id |= 1;
        }
        w.zint(id)?;
        if let Some(s) = self.schema {
            if s.len() > usize::from(u8::MAX) {
                return Err(CodecError::Overflow);
            }
            w.bytes_z(s)?;
        }
        Ok(())
    }

    pub(crate) fn read(r: &mut Reader<'a>) -> Result<Self, CodecError> {
        let raw = r.zint_max(u64::from(u32::MAX))?;
        // zenoh-codec keeps the low 16 bits of the id.
        let id = (raw >> 1) as u16;
        let schema = if raw & 1 != 0 {
            Some(r.bytes_z(u64::from(u8::MAX))?)
        } else {
            None
        };
        Ok(Encoding { id, schema })
    }
}

/// An entity of a node: its Zenoh id and an entity id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityGlobalId {
    /// The node.
    pub zid: Zid,
    /// The entity within it.
    pub eid: u32,
}

impl EntityGlobalId {
    pub(crate) fn len(&self) -> usize {
        1 + self.zid.size() + zint_len(u64::from(self.eid))
    }

    pub(crate) fn write(&self, w: &mut Writer<'_>) -> Result<(), CodecError> {
        // `size()` is 1..=16, so this is 0..=15 shifted into the high nibble.
        w.u8(((self.zid.size() as u8).wrapping_sub(1)) << 4)?;
        w.bytes(self.zid.as_wire())?;
        w.zint(u64::from(self.eid))
    }

    pub(crate) fn read(r: &mut Reader<'_>) -> Result<Self, CodecError> {
        let flags = r.u8()?;
        let zid = Zid::read(r, 1 + usize::from(flags >> 4))?;
        // zenoh-codec reads the entity id as a u64 and keeps 32 bits.
        let eid = r.zint()? as u32;
        Ok(EntityGlobalId { zid, eid })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zn1_zints_round_trip_at_every_length_boundary() {
        let mut values = [0u64; 20];
        for (i, v) in values.iter_mut().enumerate().take(10) {
            *v = (1u64 << (7 * i as u32).min(63)).wrapping_sub(1);
        }
        values[10] = u64::MAX;
        values[11] = 1 << 56;
        values[12] = (1 << 63) + 5;
        values[13] = 0x80;
        for v in values {
            let mut buf = [0u8; 9];
            let mut w = Writer::new(&mut buf);
            w.zint(v).expect("fits");
            assert_eq!(w.len(), zint_len(v), "{v:#x}");
            let mut r = Reader::new(w.written());
            assert_eq!(r.zint(), Ok(v));
            assert!(r.is_empty());
        }
        // Truncated and over-bound integers are refused.
        assert_eq!(Reader::new(&[0x80]).zint(), Err(CodecError::Truncated));
        assert_eq!(Reader::new(&[0x80, 0x02]).zu16(), Ok(256));
        assert_eq!(
            Reader::new(&[0x80, 0x80, 0x04]).zu16(),
            Err(CodecError::Overflow)
        );
        let mut tiny = [0u8; 1];
        assert_eq!(Writer::new(&mut tiny).zint(300), Err(CodecError::NoSpace));
    }

    #[test]
    fn zn1_unknown_extensions_skip_and_mandatory_ones_refuse() {
        // Unit id 5 (more), z64 id 6 = 300 (more), zbuf id 7 = "ab".
        let bytes = [0x85, 0xa6, 0xac, 0x02, 0x47, 0x02, b'a', b'b'];
        let mut r = Reader::new(&bytes);
        assert_eq!(skip_exts(&mut r, true), Ok(()));
        assert!(r.is_empty());
        // The same with the M flag on the second.
        let bytes = [0x85, 0xb6, 0xac, 0x02];
        assert_eq!(
            skip_exts(&mut Reader::new(&bytes), true),
            Err(CodecError::Mandatory)
        );
        // Encoding 0b11 is invalid; a zbuf past the end is truncated.
        assert_eq!(
            skip_exts(&mut Reader::new(&[0x61]), true),
            Err(CodecError::Invalid)
        );
        assert_eq!(
            skip_exts(&mut Reader::new(&[0x47, 0x05, 1]), true),
            Err(CodecError::Truncated)
        );
    }

    #[test]
    fn zn1_zids_carry_only_significant_bytes() {
        assert_eq!(Zid::new([0; 16]), None);
        let mut b = [0u8; 16];
        b[0] = 7;
        let z = Zid::new(b).expect("non zero");
        assert_eq!((z.size(), z.as_wire()), (1, &[7u8][..]));
        b[15] = 1;
        assert_eq!(Zid::new(b).map(|z| z.size()), Some(16));
        assert_eq!(Zid::from_slice(&[0, 0]), None);
        assert_eq!(Zid::from_slice(&[0; 17]), None);
    }
}
