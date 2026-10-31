//! The wire format: one SMBus Block Write per fragment, and the message
//! bodies that fragments carry. Pure: encoding writes into caller buffers,
//! decoding borrows its input, and neither panics on any input.
//!
//! # One block
//!
//! Every fragment is an SMBus **Block Write with PEC** (SMBus 2.0 §5.5.7,
//! §7.4) from the sending master to the destination's address:
//!
//! ```text
//! S | addr<<1|W | A | command | A | count | A | data[0..count] | A | PEC | A | P
//! ```
//!
//! What the master writes after the address byte (what [`encode_block`]
//! produces and [`decode_block`] reads) is `[command, count, data.., PEC]`:
//!
//! | byte        | meaning                                                  |
//! |-------------|----------------------------------------------------------|
//! | `command`   | `0xC0 \| kind`: `0xC1` publish, `0xC2` request, `0xC3` reply, `0xC4` error ([`Kind`]); any other command is foreign traffic |
//! | `count`     | data bytes that follow, [`FRAGMENT_HEADER`]`..=B`         |
//! | `data[0]`   | the sender's own 7-bit address (I2C does not tell a target who the master is) |
//! | `data[1]`   | message id: the sender's publication sequence for a publish, the correlation id for a request and for the reply or error that answers it |
//! | `data[2]`   | bit 7: last fragment; bits 6..0: fragment index (0..=127) |
//! | `data[3..]` | this fragment's slice of the message body                 |
//! | `PEC`       | CRC-8/SMBUS ([`crc8`](crate::crc::crc8)) over the address byte (`addr << 1`, R/W̄ = 0, so `0x00` for the general call), `command`, `count` and `data` |
//!
//! `B`, the largest `count`, is 32 for SMBus 2.0 ([`SMBUS2_BLOCK`]) and up
//! to 255 for SMBus 3.x ([`MAX_BLOCK`]); a fragment carries `B - 3` body
//! bytes. A message is at most [`MAX_FRAGMENTS`] fragments, so its body is
//! at most [`max_body`]`(B)` bytes (3 712 for `B = 32`); the node's own
//! `M` bounds it further.
//!
//! # Message bodies
//!
//! The fragments of a message, in index order, concatenate to its body:
//!
//! | kind      | body                                                       |
//! |-----------|------------------------------------------------------------|
//! | publish   | `[key_len][key][payload]`                                  |
//! | request   | `[deadline_ms u32 LE][key_len][key][request]`; `0xFFFF_FFFF` means no deadline |
//! | reply     | `[response]`                                               |
//! | error     | `[code]`, a [`HandlerError::code`]                         |

use super::address::Address;
use crate::crc::crc8_update;
use crate::key::Key;
use crate::transport::HandlerError;

/// Bytes of every fragment's header inside the block data: sender address,
/// message id and fragment index.
pub const FRAGMENT_HEADER: usize = 3;

/// Bytes a block adds around its data on the wire after the address byte:
/// command, count and PEC.
pub const TRANSACTION_OVERHEAD: usize = 3;

/// The SMBus 2.0 block size: at most 32 data bytes per block.
pub const SMBUS2_BLOCK: usize = 32;

/// The SMBus 3.x block size limit: 255 data bytes.
pub const MAX_BLOCK: usize = 255;

/// The smallest block size that carries a body byte.
pub const MIN_BLOCK: usize = FRAGMENT_HEADER + 1;

/// The longest transaction (after the address byte) of any block size.
pub const MAX_TRANSACTION: usize = MAX_BLOCK + TRANSACTION_OVERHEAD;

/// Fragments a message may have (a 7-bit index).
pub const MAX_FRAGMENTS: usize = 128;

/// Header bytes a request body has before its key: the deadline and the
/// key length.
pub const REQUEST_HEAD: usize = 5;

/// Header bytes a publish body has before its key: the key length.
pub const PUBLISH_HEAD: usize = 1;

const MAX_HEAD: usize = REQUEST_HEAD;
const COMMAND_PREFIX: u8 = 0xC0;
const LAST: u8 = 0x80;
const NO_DEADLINE: u32 = u32::MAX;

/// Body bytes one fragment carries with block size `block`.
pub const fn fragment_capacity(block: usize) -> usize {
    block.saturating_sub(FRAGMENT_HEADER)
}

/// The largest message body with block size `block`.
pub const fn max_body(block: usize) -> usize {
    MAX_FRAGMENTS * fragment_capacity(block)
}

/// The kind of a message, carried in the block's command byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A publication, broadcast.
    Publish,
    /// A request, unicast to the responder.
    Request,
    /// A successful reply, unicast to the requester.
    Reply,
    /// A handler error answering a request, unicast to the requester.
    Error,
}

impl Kind {
    /// The SMBus command byte of this kind.
    pub const fn command(self) -> u8 {
        COMMAND_PREFIX
            | match self {
                Kind::Publish => 1,
                Kind::Request => 2,
                Kind::Reply => 3,
                Kind::Error => 4,
            }
    }

    /// The kind a command byte names, if it is one of this protocol's.
    pub const fn from_command(command: u8) -> Option<Kind> {
        match command {
            0xC1 => Some(Kind::Publish),
            0xC2 => Some(Kind::Request),
            0xC3 => Some(Kind::Reply),
            0xC4 => Some(Kind::Error),
            _ => None,
        }
    }
}

/// One decoded block: a fragment of a message. Borrows the transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fragment<'a> {
    /// The message kind.
    pub kind: Kind,
    /// The sender's own address.
    pub source: Address,
    /// Message id (see the module docs).
    pub id: u8,
    /// Position of this fragment in the message, 0..=127.
    pub index: u8,
    /// True on the message's last fragment.
    pub last: bool,
    /// This fragment's slice of the message body.
    pub body: &'a [u8],
}

/// Why a block or a message body could not be encoded or decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// The block size is outside [`MIN_BLOCK`]`..=`[`MAX_BLOCK`].
    BlockSize,
    /// The transaction is shorter than a block with a fragment header.
    Short,
    /// The count byte is out of range or disagrees with the length.
    Count,
    /// The Packet Error Code does not match.
    Pec,
    /// The command byte is not one of this protocol's.
    Foreign(u8),
    /// The sender address byte is not a node address.
    Source,
    /// The fragment index exceeds 127, or the message needs more than
    /// [`MAX_FRAGMENTS`] fragments.
    Index,
    /// The output buffer or the block is too small, or a body exceeds a
    /// bound.
    TooLarge,
    /// A message body is shorter than its header or its lengths disagree.
    Malformed,
    /// The key bytes are not a valid [`Key`].
    InvalidKey,
}

/// Writes the transaction bytes `[command, count, data.., PEC]` of `frag`,
/// addressed to `to`, into `out` and returns their length. The fragment
/// body must fit the block size `block`.
pub fn encode_block(
    to: Address,
    frag: &Fragment<'_>,
    block: usize,
    out: &mut [u8],
) -> Result<usize, WireError> {
    if !(MIN_BLOCK..=MAX_BLOCK).contains(&block) {
        return Err(WireError::BlockSize);
    }
    if usize::from(frag.index) >= MAX_FRAGMENTS {
        return Err(WireError::Index);
    }
    let count = FRAGMENT_HEADER + frag.body.len();
    if count > block {
        return Err(WireError::TooLarge);
    }
    let len = count + TRANSACTION_OVERHEAD;
    let out = out.get_mut(..len).ok_or(WireError::TooLarge)?;
    let (head, rest) = out.split_at_mut(2 + FRAGMENT_HEADER);
    head.copy_from_slice(&[
        frag.kind.command(),
        // `count <= block <= 255`.
        count as u8,
        frag.source.get(),
        frag.id,
        frag.index | if frag.last { LAST } else { 0 },
    ]);
    let (body, pec) = rest.split_at_mut(frag.body.len());
    body.copy_from_slice(frag.body);
    let crc = crc8_update(crc8_update(0, &[to.write_byte()]), head);
    let crc = crc8_update(crc, body);
    if let Some(p) = pec.first_mut() {
        *p = crc;
    }
    Ok(len)
}

/// Decodes the transaction bytes `[command, count, data.., PEC]` a target
/// received at address `to`, for block size `block`. Checks, in order, the
/// length and count, the PEC, the command and the sender address.
pub fn decode_block(to: Address, bytes: &[u8], block: usize) -> Result<Fragment<'_>, WireError> {
    if !(MIN_BLOCK..=MAX_BLOCK).contains(&block) {
        return Err(WireError::BlockSize);
    }
    let [command, count, rest @ ..] = bytes else {
        return Err(WireError::Short);
    };
    let count = usize::from(*count);
    if count < FRAGMENT_HEADER || count > block || rest.len() != count + 1 {
        return Err(if rest.len() < FRAGMENT_HEADER + 1 {
            WireError::Short
        } else {
            WireError::Count
        });
    }
    let (covered, pec) = bytes.split_at(bytes.len() - 1);
    let crc = crc8_update(crc8_update(0, &[to.write_byte()]), covered);
    if pec != [crc] {
        return Err(WireError::Pec);
    }
    let kind = Kind::from_command(*command).ok_or(WireError::Foreign(*command))?;
    let [source, id, index, body @ ..] = rest.get(..count).ok_or(WireError::Count)? else {
        return Err(WireError::Short);
    };
    let source = Address::node(*source).map_err(|_| WireError::Source)?;
    Ok(Fragment {
        kind,
        source,
        id: *id,
        index: *index & !LAST,
        last: *index & LAST != 0,
        body,
    })
}

/// A whole message, as sent and as reassembled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message<'a> {
    /// A publication on `key`.
    Publish {
        /// Topic.
        key: Key<'a>,
        /// Published bytes.
        payload: &'a [u8],
    },
    /// A request to the handler for `key`.
    Request {
        /// The caller's budget in milliseconds when it sent the request
        /// (`Some(u32::MAX)` is carried as `u32::MAX - 1`).
        deadline_ms: Option<u32>,
        /// Target key.
        key: Key<'a>,
        /// Request bytes.
        body: &'a [u8],
    },
    /// The handler's response.
    Reply {
        /// Response bytes.
        body: &'a [u8],
    },
    /// The handler's error.
    Error {
        /// What failed.
        error: HandlerError,
    },
}

impl Message<'_> {
    /// The message's kind.
    pub const fn kind(&self) -> Kind {
        match self {
            Message::Publish { .. } => Kind::Publish,
            Message::Request { .. } => Kind::Request,
            Message::Reply { .. } => Kind::Reply,
            Message::Error { .. } => Kind::Error,
        }
    }
}

/// The body of a message to send, as up to three borrowed parts (a small
/// header, the key and the rest), so fragments are cut from the caller's
/// buffers without copying the message together first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Body<'a> {
    head: [u8; MAX_HEAD],
    head_len: usize,
    key: &'a [u8],
    tail: &'a [u8],
}

impl<'a> Body<'a> {
    /// The body of `msg` (see the module docs for the layouts).
    pub fn of(msg: &Message<'a>) -> Body<'a> {
        let mut head = [0u8; MAX_HEAD];
        let (head_len, key, tail): (usize, &'a [u8], &'a [u8]) = match *msg {
            Message::Publish { key, payload } => {
                head[0] = key_len(key);
                (PUBLISH_HEAD, key.as_bytes(), payload)
            }
            Message::Request {
                deadline_ms,
                key,
                body,
            } => {
                let d = deadline_ms.map_or(NO_DEADLINE, |d| d.min(NO_DEADLINE - 1));
                let [a, b, c, e] = d.to_le_bytes();
                head = [a, b, c, e, key_len(key)];
                (REQUEST_HEAD, key.as_bytes(), body)
            }
            Message::Reply { body } => (0, &[], body),
            Message::Error { error } => {
                head[0] = error.code();
                (1, &[], &[])
            }
        };
        Body {
            head,
            head_len,
            key,
            tail,
        }
    }

    /// Total body length.
    pub const fn len(&self) -> usize {
        self.head_len + self.key.len() + self.tail.len()
    }

    /// True for an empty body (an empty reply).
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copies body bytes from offset `start` into `out` (as many as fit or
    /// remain) and returns how many were copied.
    pub fn copy_from(&self, start: usize, out: &mut [u8]) -> usize {
        let head = self.head.get(..self.head_len).unwrap_or_default();
        let mut skip = start;
        let mut written = 0;
        for part in [head, self.key, self.tail] {
            if skip >= part.len() {
                skip -= part.len();
                continue;
            }
            let src = part.get(skip..).unwrap_or_default();
            skip = 0;
            let Some(dst) = out.get_mut(written..) else {
                break;
            };
            let n = src.len().min(dst.len());
            if let (Some(d), Some(s)) = (dst.get_mut(..n), src.get(..n)) {
                d.copy_from_slice(s);
            }
            written += n;
        }
        written
    }
}

fn key_len(key: Key<'_>) -> u8 {
    // A valid key is at most MAX_KEY_LEN (128) bytes.
    u8::try_from(key.as_bytes().len()).unwrap_or(u8::MAX)
}

/// Fragments a body of `body_len` bytes needs with block size `block`: at
/// least one (an empty reply is one empty fragment).
pub const fn fragment_count(body_len: usize, block: usize) -> Result<usize, WireError> {
    if block < MIN_BLOCK || block > MAX_BLOCK {
        return Err(WireError::BlockSize);
    }
    let n = body_len.div_ceil(fragment_capacity(block));
    let n = if n == 0 { 1 } else { n };
    if n > MAX_FRAGMENTS {
        Err(WireError::Index)
    } else {
        Ok(n)
    }
}

/// Parses a reassembled message body of `kind`.
pub fn decode_message(kind: Kind, body: &[u8]) -> Result<Message<'_>, WireError> {
    match kind {
        Kind::Publish => {
            let (key, payload) = keyed(body)?;
            Ok(Message::Publish { key, payload })
        }
        Kind::Request => {
            let [a, b, c, d, rest @ ..] = body else {
                return Err(WireError::Malformed);
            };
            let deadline = u32::from_le_bytes([*a, *b, *c, *d]);
            let (key, body) = keyed(rest)?;
            Ok(Message::Request {
                deadline_ms: (deadline != NO_DEADLINE).then_some(deadline),
                key,
                body,
            })
        }
        Kind::Reply => Ok(Message::Reply { body }),
        Kind::Error => match body {
            [code] => Ok(Message::Error {
                error: HandlerError::from_code(*code),
            }),
            _ => Err(WireError::Malformed),
        },
    }
}

/// Splits `[key_len][key][rest]`.
fn keyed(bytes: &[u8]) -> Result<(Key<'_>, &[u8]), WireError> {
    let [len, rest @ ..] = bytes else {
        return Err(WireError::Malformed);
    };
    let (key, rest) = rest
        .split_at_checked(usize::from(*len))
        .ok_or(WireError::Malformed)?;
    let key = core::str::from_utf8(key).map_err(|_| WireError::InvalidKey)?;
    let key = Key::new(key).map_err(|_| WireError::InvalidKey)?;
    Ok((key, rest))
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::crc::crc8;

    const K: Key<'static> = Key::from_static("sensor/temp");

    fn addr(raw: u8) -> Address {
        Address::node(raw).unwrap()
    }

    fn frag(body: &[u8]) -> Fragment<'_> {
        Fragment {
            kind: Kind::Publish,
            source: addr(0x2A),
            id: 7,
            index: 3,
            last: true,
            body,
        }
    }

    #[test]
    fn m2_block_layout_is_smbus_block_write_with_pec() {
        let to = addr(0x31);
        let mut out = [0u8; MAX_TRANSACTION];
        let n = encode_block(to, &frag(&[0xAA, 0xBB]), SMBUS2_BLOCK, &mut out).unwrap();
        assert_eq!(&out[..n - 1], &[0xC1, 5, 0x2A, 7, 0x83, 0xAA, 0xBB]);
        // The PEC covers the address byte (0x31 << 1 = 0x62) and every
        // byte after it.
        assert_eq!(
            out[n - 1],
            crc8(&[0x62, 0xC1, 5, 0x2A, 7, 0x83, 0xAA, 0xBB])
        );
        // So the CRC over address byte + whole transaction is zero.
        let mut whole = [0u8; 16];
        whole[0] = 0x62;
        whole[1..=n].copy_from_slice(&out[..n]);
        assert_eq!(crc8(&whole[..=n]), 0);
        // To the general call address the address byte is 0x00.
        let n = encode_block(Address::GENERAL_CALL, &frag(&[]), 32, &mut out).unwrap();
        assert_eq!(out[n - 1], crc8(&[0x00, 0xC1, 3, 0x2A, 7, 0x83]));
    }

    #[test]
    fn m2_blocks_round_trip_for_every_kind_and_size() {
        let mut out = [0u8; MAX_TRANSACTION];
        let body = [0x5Au8; MAX_BLOCK];
        for kind in [Kind::Publish, Kind::Request, Kind::Reply, Kind::Error] {
            assert_eq!(Kind::from_command(kind.command()), Some(kind));
            for block in [MIN_BLOCK, SMBUS2_BLOCK, MAX_BLOCK] {
                for len in [0, 1, fragment_capacity(block)] {
                    let f = Fragment {
                        kind,
                        source: addr(0x10),
                        id: 0xFE,
                        index: 127,
                        last: len == 0,
                        body: &body[..len],
                    };
                    let to = addr(0x55);
                    let n = encode_block(to, &f, block, &mut out).unwrap();
                    assert_eq!(n, len + FRAGMENT_HEADER + TRANSACTION_OVERHEAD);
                    assert_eq!(decode_block(to, &out[..n], block), Ok(f));
                }
            }
        }
    }

    #[test]
    fn m2_bad_blocks_are_rejected() {
        let to = addr(0x31);
        let mut out = [0u8; MAX_TRANSACTION];
        let n = encode_block(to, &frag(b"abc"), 32, &mut out).unwrap();
        let good = out;
        // Any single bit flip anywhere fails (PEC, or count first).
        for i in 0..n {
            for bit in 0..8 {
                let mut bad = good;
                bad[i] ^= 1 << bit;
                assert!(
                    decode_block(to, &bad[..n], 32).is_err(),
                    "byte {i} bit {bit}"
                );
            }
        }
        let mut bad = good;
        bad[n - 1] ^= 1;
        assert_eq!(decode_block(to, &bad[..n], 32), Err(WireError::Pec));
        // Addressed to someone else: the PEC covers the address.
        assert_eq!(
            decode_block(addr(0x32), &good[..n], 32),
            Err(WireError::Pec)
        );
        assert_eq!(decode_block(to, &good[..n - 1], 32), Err(WireError::Count));
        assert_eq!(decode_block(to, &good[..4], 32), Err(WireError::Short));
        assert_eq!(decode_block(to, &[], 32), Err(WireError::Short));
        // A count over the block size.
        assert_eq!(decode_block(to, &good[..n], 5), Err(WireError::Count));
        assert_eq!(decode_block(to, &good[..n], 3), Err(WireError::BlockSize));
        assert_eq!(decode_block(to, &good[..n], 256), Err(WireError::BlockSize));
    }

    /// Re-PECs `bytes` (a transaction to `to`) after an edit.
    fn seal(to: Address, bytes: &mut [u8]) {
        let n = bytes.len();
        bytes[n - 1] = crc8_update(crc8(&[to.write_byte()]), &bytes[..n - 1]);
    }

    #[test]
    fn m2_foreign_commands_and_bad_sources_are_rejected_after_the_pec() {
        let to = addr(0x31);
        let mut out = [0u8; MAX_TRANSACTION];
        let n = encode_block(to, &frag(b"x"), 32, &mut out).unwrap();
        let mut foreign = out;
        foreign[0] = 0x10;
        seal(to, &mut foreign[..n]);
        assert_eq!(
            decode_block(to, &foreign[..n], 32),
            Err(WireError::Foreign(0x10))
        );
        let mut reserved = out;
        reserved[2] = 0x08;
        seal(to, &mut reserved[..n]);
        assert_eq!(decode_block(to, &reserved[..n], 32), Err(WireError::Source));
    }

    #[test]
    fn m2_encode_refuses_what_does_not_fit() {
        let mut out = [0u8; MAX_TRANSACTION];
        let big = [0u8; 30];
        assert_eq!(
            encode_block(addr(0x31), &frag(&big), 32, &mut out),
            Err(WireError::TooLarge)
        );
        assert_eq!(
            encode_block(addr(0x31), &frag(b"ab"), 32, &mut out[..7]),
            Err(WireError::TooLarge)
        );
        let f = Fragment {
            index: 128,
            ..frag(b"")
        };
        assert_eq!(
            encode_block(addr(0x31), &f, 32, &mut out),
            Err(WireError::Index)
        );
        assert_eq!(
            encode_block(addr(0x31), &frag(b""), 2, &mut out),
            Err(WireError::BlockSize)
        );
    }

    /// Joins a body's fragments as the receiver would.
    fn cut(body: &Body<'_>, block: usize) -> std::vec::Vec<std::vec::Vec<u8>> {
        let n = fragment_count(body.len(), block).unwrap();
        let cap = fragment_capacity(block);
        (0..n)
            .map(|i| {
                let mut buf = [0u8; MAX_BLOCK];
                let got = body.copy_from(i * cap, &mut buf[..cap]);
                buf[..got].to_vec()
            })
            .collect()
    }

    #[test]
    fn m4_message_bodies_round_trip_through_fragments() {
        let payload: std::vec::Vec<u8> = (0..700u32).map(|i| i as u8).collect();
        let messages = [
            Message::Publish {
                key: K,
                payload: &payload[..300],
            },
            Message::Request {
                deadline_ms: Some(1500),
                key: K,
                body: &payload,
            },
            Message::Request {
                deadline_ms: None,
                key: K,
                body: b"",
            },
            Message::Reply { body: &payload },
            Message::Reply { body: b"" },
            Message::Error {
                error: HandlerError::Unavailable,
            },
        ];
        for m in messages {
            for block in [16, SMBUS2_BLOCK, MAX_BLOCK] {
                let body = Body::of(&m);
                let parts = cut(&body, block);
                assert!(parts.iter().all(|p| p.len() <= fragment_capacity(block)));
                let joined: std::vec::Vec<u8> = parts.concat();
                assert_eq!(joined.len(), body.len());
                assert_eq!(decode_message(m.kind(), &joined), Ok(m));
            }
        }
        // u32::MAX would read back as "no deadline": it is carried one less.
        let m = Message::Request {
            deadline_ms: Some(u32::MAX),
            key: K,
            body: b"",
        };
        let joined = cut(&Body::of(&m), 32).concat();
        assert!(matches!(
            decode_message(Kind::Request, &joined),
            Ok(Message::Request { deadline_ms: Some(d), .. }) if d == u32::MAX - 1
        ));
    }

    #[test]
    fn m4_fragment_counts_are_bounded() {
        assert_eq!(fragment_count(0, 32), Ok(1));
        assert_eq!(fragment_count(29, 32), Ok(1));
        assert_eq!(fragment_count(30, 32), Ok(2));
        assert_eq!(max_body(32), 3712);
        assert_eq!(fragment_count(max_body(32), 32), Ok(128));
        assert_eq!(fragment_count(max_body(32) + 1, 32), Err(WireError::Index));
        assert_eq!(fragment_count(1, 3), Err(WireError::BlockSize));
    }

    #[test]
    fn m4_malformed_bodies_are_rejected() {
        assert_eq!(
            decode_message(Kind::Publish, &[]),
            Err(WireError::Malformed)
        );
        assert_eq!(
            decode_message(Kind::Publish, &[5, b'a']),
            Err(WireError::Malformed)
        );
        assert_eq!(
            decode_message(Kind::Publish, &[1, b' ']),
            Err(WireError::InvalidKey)
        );
        assert_eq!(
            decode_message(Kind::Publish, &[1, 0xFF]),
            Err(WireError::InvalidKey)
        );
        assert_eq!(
            decode_message(Kind::Request, &[0, 0, 0]),
            Err(WireError::Malformed)
        );
        assert_eq!(decode_message(Kind::Error, &[]), Err(WireError::Malformed));
        assert_eq!(
            decode_message(Kind::Error, &[1, 2]),
            Err(WireError::Malformed)
        );
        assert_eq!(
            decode_message(Kind::Error, &[99]),
            Ok(Message::Error {
                error: HandlerError::Internal
            })
        );
    }

    #[test]
    fn m2_hostile_input_never_panics() {
        // A fixed LCG drives every length and byte pattern through both
        // decoders; most inputs fail, none may panic.
        let mut x: u32 = 0x1234_5678;
        let mut buf = [0u8; MAX_TRANSACTION + 8];
        let mut decoded = 0;
        for round in 0..20_000u32 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let len = (x >> 8) as usize % buf.len();
            for b in buf.iter_mut().take(len) {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *b = (x >> 24) as u8;
            }
            if round.is_multiple_of(3) && len >= 3 {
                // Make the count consistent so the PEC path runs too.
                buf[1] = (len - 3).min(255) as u8;
            }
            let to = addr(0x40);
            if let Ok(f) = decode_block(to, &buf[..len], 1 + (x as usize % 300)) {
                decoded += 1;
                let _ = decode_message(f.kind, f.body);
            }
            for kind in [Kind::Publish, Kind::Request, Kind::Reply, Kind::Error] {
                let _ = decode_message(kind, &buf[..len]);
            }
        }
        // Valid blocks are rare but the loop does reach the end.
        assert!(decoded < 20_000);
    }
}
