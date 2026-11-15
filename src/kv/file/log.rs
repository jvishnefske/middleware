//! The on-disk log format of [`FileKv`](super::FileKv), as pure functions:
//! encode a record, and split a log image into its valid records and a
//! discardable torn tail. No I/O happens here.
//!
//! # Layout
//!
//! All integers are little-endian.
//!
//! ```text
//! file   = header frame*
//! header = "MWKV" version:u32                       (8 bytes, version 1)
//! frame  = len:u32 !len:u32 body[len] crc:u32
//! body   = kind:u8 revision:u64 key_len:u16 key[key_len] value[..]
//! ```
//!
//! * `!len` is the bitwise complement of `len`, a cheap check that the
//!   length itself is intact before it is trusted to find the CRC.
//! * `crc` is CRC-32/ISO-HDLC ([`Crc32`]) of `body`.
//! * The value runs to the end of the body: its length is
//!   `len - 11 - key_len`.
//! * `kind` 1 is a put (key 1..=[`MAX_KEY_LEN`] bytes, a valid [`Key`];
//!   value up to [`MAX_VALUE`] bytes), 2 a delete (key as for a put, no
//!   value), 3 a checkpoint (no key, no value), which says that every
//!   revision up to and including its `revision` may have been issued, so
//!   the next one must exceed it.
//!
//! # Replay rules
//!
//! [`parse`] reads frames from just after the header. At the first frame
//! that is not intact (shorter than its length, a broken `!len`, a length
//! outside the legal range, or a CRC mismatch) at offset `x`:
//!
//! * If no intact frame starts at any byte offset after `x`, the bytes from
//!   `x` on are a **torn tail**, the remains of an append that a crash
//!   interrupted, and the log is kept up to `x`.
//! * Otherwise data was durably written after the damage, so it is not a
//!   torn append: the log is **corrupt** at `x` and [`parse`] fails, unless
//!   the caller chose [`Recovery::TruncateAtFirstBad`], which keeps the log
//!   up to `x` and drops everything after.
//!
//! A frame whose CRC holds but whose body does not decode (unknown kind,
//! invalid key, wrong lengths for its kind) was written whole, so it is
//! always corruption, even at the tail. An intact frame embedded inside a
//! torn record's value makes the tail look like corruption; that errs on
//! the side of reporting, never of silently dropping data.

use std::vec::Vec;

use crate::crc::Crc32;
use crate::key::{Key, MAX_KEY_LEN};

/// The file magic.
pub const MAGIC: [u8; 4] = *b"MWKV";
/// The format version this code reads and writes.
pub const VERSION: u32 = 1;
/// Length of the file header.
pub const HEADER_LEN: usize = 8;
/// The largest value a put may carry: 1 MiB.
pub const MAX_VALUE: usize = 1 << 20;

/// `len` and `!len`.
const FRAME_HEAD: usize = 8;
/// The trailing CRC.
const FRAME_TAIL: usize = 4;
/// kind, revision, key_len.
const BODY_FIXED: usize = 1 + 8 + 2;
/// The longest legal body.
const MAX_BODY: usize = BODY_FIXED + MAX_KEY_LEN + MAX_VALUE;
/// The shortest legal frame.
const MIN_FRAME: usize = FRAME_HEAD + BODY_FIXED + FRAME_TAIL;

const KIND_PUT: u8 = 1;
const KIND_DELETE: u8 = 2;
const KIND_CHECKPOINT: u8 = 3;

/// The file header for this format version.
pub fn header() -> [u8; HEADER_LEN] {
    let v = VERSION.to_le_bytes();
    [
        MAGIC[0], MAGIC[1], MAGIC[2], MAGIC[3], v[0], v[1], v[2], v[3],
    ]
}

/// One logged event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Record<'a> {
    /// `key` now holds `value`, set by the write stamped `revision`.
    Put {
        revision: u64,
        key: Key<'a>,
        value: &'a [u8],
    },
    /// `key` was removed by the write stamped `revision`.
    Delete { revision: u64, key: Key<'a> },
    /// Every revision up to `revision` may have been issued.
    Checkpoint { revision: u64 },
}

impl Record<'_> {
    /// The revision the record carries.
    pub const fn revision(&self) -> u64 {
        match *self {
            Record::Put { revision, .. }
            | Record::Delete { revision, .. }
            | Record::Checkpoint { revision } => revision,
        }
    }
}

/// Why a record could not be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    /// The value is longer than [`MAX_VALUE`].
    ValueTooLarge,
}

/// What is wrong at a corrupt offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Corruption {
    /// The frame's length runs past the end of the file.
    Truncated,
    /// `len` and `!len` disagree, or `len` is outside the legal range.
    Length,
    /// The body does not match its CRC.
    Checksum,
    /// An intact frame of an unknown kind.
    Kind(u8),
    /// An intact frame whose key or value length is wrong for its kind.
    Layout,
    /// An intact frame whose key is not a valid [`Key`].
    Key,
    /// A put or delete whose revision does not exceed the previous one's
    /// (detected by the replay in the `index` module).
    RevisionOrder,
}

/// Why a log image was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// The file does not start with [`MAGIC`].
    NotALog,
    /// The header names a version other than [`VERSION`].
    UnsupportedVersion(u32),
    /// The log is damaged at this byte offset.
    Corrupt { offset: u64, why: Corruption },
}

/// What [`parse`] does with a damaged frame that is not a torn tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Recovery {
    /// Fail with [`ParseError::Corrupt`] (the default).
    #[default]
    Strict,
    /// Keep the log up to the first damaged frame and discard the rest,
    /// including any intact frames after it.
    TruncateAtFirstBad,
}

/// A decoded record and the file offset of its frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Located<'a> {
    pub offset: u64,
    pub record: Record<'a>,
}

/// The valid part of a log image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed<'a> {
    /// `false` if the image was empty or a prefix of the header (a crash
    /// while the file was being created): the caller writes a fresh header.
    pub header_present: bool,
    /// The intact records in file order.
    pub records: Vec<Located<'a>>,
    /// Bytes of the image to keep; anything after is a torn tail (or, under
    /// [`Recovery::TruncateAtFirstBad`], discarded damage). `0` when the
    /// header is missing.
    pub keep: usize,
}

/// Appends the frame of `record` to `out`. On error `out` is unchanged.
pub fn encode(record: &Record<'_>, out: &mut Vec<u8>) -> Result<(), EncodeError> {
    let (kind, revision, key, value): (u8, u64, &[u8], &[u8]) = match *record {
        Record::Put {
            revision,
            key,
            value,
        } => (KIND_PUT, revision, key.as_bytes(), value),
        Record::Delete { revision, key } => (KIND_DELETE, revision, key.as_bytes(), &[]),
        Record::Checkpoint { revision } => (KIND_CHECKPOINT, revision, &[], &[]),
    };
    if value.len() > MAX_VALUE {
        return Err(EncodeError::ValueTooLarge);
    }
    // Keys are at most MAX_KEY_LEN (128) bytes, so neither conversion fails;
    // the error arm keeps the arithmetic panic free.
    let key_len = u16::try_from(key.len()).map_err(|_| EncodeError::ValueTooLarge)?;
    let body_len = BODY_FIXED + key.len() + value.len();
    let len = u32::try_from(body_len).map_err(|_| EncodeError::ValueTooLarge)?;

    let start = out.len();
    out.reserve(FRAME_HEAD + body_len + FRAME_TAIL);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&(!len).to_le_bytes());
    out.push(kind);
    out.extend_from_slice(&revision.to_le_bytes());
    out.extend_from_slice(&key_len.to_le_bytes());
    out.extend_from_slice(key);
    out.extend_from_slice(value);
    let crc = out.get(start + FRAME_HEAD..).map_or(0, Crc32::checksum);
    out.extend_from_slice(&crc.to_le_bytes());
    Ok(())
}

/// Splits `image` (a whole log file) into its intact records and the
/// length to keep, following the module's replay rules.
pub fn parse(image: &[u8], recovery: Recovery) -> Result<Parsed<'_>, ParseError> {
    let expected = header();
    if image.len() < HEADER_LEN {
        return if expected.starts_with(image) {
            Ok(Parsed {
                header_present: false,
                records: Vec::new(),
                keep: 0,
            })
        } else {
            Err(ParseError::NotALog)
        };
    }
    if image.get(..4) != Some(&MAGIC[..]) {
        return Err(ParseError::NotALog);
    }
    let version = read_u32(image, 4).unwrap_or(0);
    if version != VERSION {
        return Err(ParseError::UnsupportedVersion(version));
    }

    let mut records = Vec::new();
    let mut at = HEADER_LEN;
    // Each intact frame advances `at` by at least MIN_FRAME bytes, so the
    // loop runs at most image.len() / MIN_FRAME times.
    while at < image.len() {
        match frame_at(image, at) {
            Ok((body, next)) => {
                let record = decode_body(body).map_err(|why| ParseError::Corrupt {
                    offset: at as u64,
                    why,
                })?;
                records.push(Located {
                    offset: at as u64,
                    record,
                });
                at = next;
            }
            Err(why) => {
                let torn = !intact_frame_after(image, at);
                if torn || recovery == Recovery::TruncateAtFirstBad {
                    break;
                }
                return Err(ParseError::Corrupt {
                    offset: at as u64,
                    why,
                });
            }
        }
    }
    Ok(Parsed {
        header_present: true,
        records,
        keep: at.min(image.len()),
    })
}

/// The body of the intact frame at `at` and the offset just past it.
fn frame_at(image: &[u8], at: usize) -> Result<(&[u8], usize), Corruption> {
    let len = read_u32(image, at).ok_or(Corruption::Truncated)?;
    let check = read_u32(image, at + 4).ok_or(Corruption::Truncated)?;
    if check != !len {
        return Err(Corruption::Length);
    }
    let len = usize::try_from(len).map_err(|_| Corruption::Length)?;
    if !(BODY_FIXED..=MAX_BODY).contains(&len) {
        return Err(Corruption::Length);
    }
    let body_start = at + FRAME_HEAD;
    let body_end = body_start + len;
    let body = image
        .get(body_start..body_end)
        .ok_or(Corruption::Truncated)?;
    let crc = read_u32(image, body_end).ok_or(Corruption::Truncated)?;
    if Crc32::checksum(body) != crc {
        return Err(Corruption::Checksum);
    }
    Ok((body, body_end + FRAME_TAIL))
}

/// Whether an intact frame starts at any offset after `at`. Bounded by the
/// image length; the cheap `!len` check rejects almost every offset before
/// a CRC is computed.
fn intact_frame_after(image: &[u8], at: usize) -> bool {
    let last = image.len().saturating_sub(MIN_FRAME);
    (at + 1..=last).any(|y| frame_at(image, y).is_ok())
}

/// Decodes the body of an intact frame.
fn decode_body(body: &[u8]) -> Result<Record<'_>, Corruption> {
    let kind = *body.first().ok_or(Corruption::Layout)?;
    let revision = body
        .get(1..9)
        .and_then(|b| <[u8; 8]>::try_from(b).ok())
        .map(u64::from_le_bytes)
        .ok_or(Corruption::Layout)?;
    let key_len = body
        .get(9..11)
        .and_then(|b| <[u8; 2]>::try_from(b).ok())
        .map(u16::from_le_bytes)
        .ok_or(Corruption::Layout)?;
    let key_end = BODY_FIXED + usize::from(key_len);
    let key = body.get(BODY_FIXED..key_end).ok_or(Corruption::Layout)?;
    let value = body.get(key_end..).ok_or(Corruption::Layout)?;
    match kind {
        KIND_PUT | KIND_DELETE => {
            let key = core::str::from_utf8(key).map_err(|_| Corruption::Key)?;
            let key = Key::new(key).map_err(|_| Corruption::Key)?;
            if kind == KIND_PUT {
                if value.len() > MAX_VALUE {
                    return Err(Corruption::Layout);
                }
                Ok(Record::Put {
                    revision,
                    key,
                    value,
                })
            } else if value.is_empty() {
                Ok(Record::Delete { revision, key })
            } else {
                Err(Corruption::Layout)
            }
        }
        KIND_CHECKPOINT if key.is_empty() && value.is_empty() => {
            Ok(Record::Checkpoint { revision })
        }
        KIND_CHECKPOINT => Err(Corruption::Layout),
        other => Err(Corruption::Kind(other)),
    }
}

fn read_u32(image: &[u8], at: usize) -> Option<u32> {
    image
        .get(at..at.checked_add(4)?)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map(u32::from_le_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(s: &'static str) -> Key<'static> {
        Key::from_static(s)
    }

    /// A log image of `records` after the header, and each frame's offset.
    fn image(records: &[Record<'_>]) -> (Vec<u8>, Vec<usize>) {
        let mut out = header().to_vec();
        let mut offsets = Vec::new();
        for r in records {
            offsets.push(out.len());
            encode(r, &mut out).unwrap();
        }
        (out, offsets)
    }

    fn sample() -> [Record<'static>; 4] {
        [
            Record::Put {
                revision: 1,
                key: k("a/b"),
                value: b"one",
            },
            Record::Put {
                revision: 2,
                key: k("c"),
                value: b"",
            },
            Record::Delete {
                revision: 3,
                key: k("a/b"),
            },
            Record::Checkpoint { revision: 9 },
        ]
    }

    fn decoded<'a>(p: &Parsed<'a>) -> Vec<Record<'a>> {
        p.records.iter().map(|l| l.record).collect()
    }

    #[test]
    fn encode_parse_round_trip() {
        let recs = sample();
        let (img, offsets) = image(&recs);
        let p = parse(&img, Recovery::Strict).unwrap();
        assert!(p.header_present);
        assert_eq!(decoded(&p), recs.to_vec());
        assert_eq!(p.keep, img.len());
        let got: Vec<usize> = p.records.iter().map(|l| l.offset as usize).collect();
        assert_eq!(got, offsets);
    }

    #[test]
    fn frame_layout_is_as_documented() {
        let mut out = Vec::new();
        encode(
            &Record::Put {
                revision: 0x0102,
                key: k("ab"),
                value: b"xyz",
            },
            &mut out,
        )
        .unwrap();
        let len: u32 = 11 + 2 + 3;
        assert_eq!(&out[0..4], &len.to_le_bytes());
        assert_eq!(&out[4..8], &(!len).to_le_bytes());
        assert_eq!(out[8], KIND_PUT);
        assert_eq!(&out[9..17], &0x0102u64.to_le_bytes());
        assert_eq!(&out[17..19], &2u16.to_le_bytes());
        assert_eq!(&out[19..21], b"ab");
        assert_eq!(&out[21..24], b"xyz");
        assert_eq!(&out[24..28], &Crc32::checksum(&out[8..24]).to_le_bytes());
        assert_eq!(out.len(), 28);
    }

    #[test]
    fn oversized_value_is_refused_and_out_unchanged() {
        let big = vec![0u8; MAX_VALUE + 1];
        let mut out = vec![7u8];
        let r = Record::Put {
            revision: 1,
            key: k("a"),
            value: &big,
        };
        assert_eq!(encode(&r, &mut out), Err(EncodeError::ValueTooLarge));
        assert_eq!(out, vec![7u8]);
        let max = vec![0u8; MAX_VALUE];
        let r = Record::Put {
            revision: 1,
            key: k("a"),
            value: &max,
        };
        let mut out = header().to_vec();
        encode(&r, &mut out).unwrap();
        assert_eq!(decoded(&parse(&out, Recovery::Strict).unwrap()), vec![r]);
    }

    #[test]
    fn empty_or_partial_header_is_a_fresh_log() {
        let h = header();
        for n in 0..HEADER_LEN {
            let p = parse(&h[..n], Recovery::Strict).unwrap();
            assert!(!p.header_present);
            assert_eq!(p.keep, 0);
        }
        let p = parse(&h, Recovery::Strict).unwrap();
        assert!(p.header_present);
        assert!(p.records.is_empty());
    }

    #[test]
    fn foreign_file_and_other_version_are_rejected() {
        assert_eq!(parse(b"hello", Recovery::Strict), Err(ParseError::NotALog));
        assert_eq!(
            parse(b"NOTAKVLOG", Recovery::Strict),
            Err(ParseError::NotALog)
        );
        let mut img = header().to_vec();
        img[4] = 2;
        assert_eq!(
            parse(&img, Recovery::Strict),
            Err(ParseError::UnsupportedVersion(2))
        );
    }

    #[test]
    fn every_truncation_of_the_last_frame_is_a_torn_tail() {
        let recs = sample();
        let (img, offsets) = image(&recs);
        let last = *offsets.last().unwrap();
        for cut in last..img.len() {
            let p = parse(&img[..cut], Recovery::Strict).unwrap();
            assert_eq!(p.keep, last, "cut at {cut}");
            assert_eq!(decoded(&p), recs[..3].to_vec(), "cut at {cut}");
        }
    }

    #[test]
    fn zero_filled_or_garbage_tail_is_torn() {
        let recs = sample();
        let (img, _) = image(&recs);
        for tail in [vec![0u8; 3], vec![0u8; 40], vec![0xA5u8; 17]] {
            let mut t = img.clone();
            t.extend_from_slice(&tail);
            let p = parse(&t, Recovery::Strict).unwrap();
            assert_eq!(p.keep, img.len());
            assert_eq!(decoded(&p), recs.to_vec());
        }
    }

    #[test]
    fn checksum_failure_in_last_frame_is_torn() {
        let recs = sample();
        let (mut img, offsets) = image(&recs);
        let n = img.len();
        img[n - 1] ^= 0xFF;
        let p = parse(&img, Recovery::Strict).unwrap();
        assert_eq!(p.keep, offsets[3]);
    }

    #[test]
    fn damage_before_an_intact_frame_is_corruption() {
        let recs = sample();
        let (img, offsets) = image(&recs);
        // Flip a value byte of the first record (its CRC fails).
        let mut bad = img.clone();
        bad[offsets[0] + FRAME_HEAD + BODY_FIXED + 3] ^= 1;
        assert_eq!(
            parse(&bad, Recovery::Strict),
            Err(ParseError::Corrupt {
                offset: offsets[0] as u64,
                why: Corruption::Checksum
            })
        );
        // Break the length check of the second.
        let mut bad = img.clone();
        bad[offsets[1]] ^= 1;
        assert_eq!(
            parse(&bad, Recovery::Strict),
            Err(ParseError::Corrupt {
                offset: offsets[1] as u64,
                why: Corruption::Length
            })
        );
    }

    #[test]
    fn truncate_at_first_bad_keeps_the_prefix() {
        let recs = sample();
        let (mut img, offsets) = image(&recs);
        img[offsets[1] + FRAME_HEAD] ^= 1;
        let p = parse(&img, Recovery::TruncateAtFirstBad).unwrap();
        assert_eq!(p.keep, offsets[1]);
        assert_eq!(decoded(&p), recs[..1].to_vec());
    }

    #[test]
    fn intact_frame_with_bad_content_is_corruption_even_at_tail() {
        // Kind 9 with a valid CRC.
        let mut img = header().to_vec();
        let at = img.len();
        encode(&Record::Checkpoint { revision: 1 }, &mut img).unwrap();
        img[at + FRAME_HEAD] = 9;
        let crc = Crc32::checksum(&img[at + FRAME_HEAD..img.len() - 4]);
        let n = img.len();
        img[n - 4..].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(
            parse(&img, Recovery::TruncateAtFirstBad),
            Err(ParseError::Corrupt {
                offset: at as u64,
                why: Corruption::Kind(9)
            })
        );
    }

    #[test]
    fn bad_keys_and_layouts_are_corruption() {
        let reframe = |body: &[u8]| {
            let mut img = header().to_vec();
            let len = body.len() as u32;
            img.extend_from_slice(&len.to_le_bytes());
            img.extend_from_slice(&(!len).to_le_bytes());
            img.extend_from_slice(body);
            img.extend_from_slice(&Crc32::checksum(body).to_le_bytes());
            parse(&img, Recovery::Strict).map(|p| p.records.len())
        };
        let body = |kind: u8, key: &[u8], value: &[u8]| {
            let mut b = vec![kind];
            b.extend_from_slice(&1u64.to_le_bytes());
            b.extend_from_slice(&(key.len() as u16).to_le_bytes());
            b.extend_from_slice(key);
            b.extend_from_slice(value);
            b
        };
        let corrupt = |why| -> Result<usize, ParseError> {
            Err(ParseError::Corrupt {
                offset: HEADER_LEN as u64,
                why,
            })
        };
        assert_eq!(
            reframe(&body(KIND_PUT, b"a//b", b"")),
            corrupt(Corruption::Key)
        );
        assert_eq!(
            reframe(&body(KIND_PUT, b"", b"v")),
            corrupt(Corruption::Key)
        );
        assert_eq!(
            reframe(&body(KIND_DELETE, b"a", b"v")),
            corrupt(Corruption::Layout)
        );
        assert_eq!(
            reframe(&body(KIND_CHECKPOINT, b"a", b"")),
            corrupt(Corruption::Layout)
        );
        // key_len pointing past the body.
        let mut b = body(KIND_PUT, b"a", b"");
        b[9] = 50;
        assert_eq!(reframe(&b), corrupt(Corruption::Layout));
    }

    #[test]
    fn frame_inside_a_torn_value_reports_corruption() {
        // A value that itself contains an intact frame: when that put is torn
        // the embedded frame is found, and parse reports rather than drops.
        let mut inner = Vec::new();
        encode(&Record::Checkpoint { revision: 5 }, &mut inner).unwrap();
        let mut value = inner.clone();
        value.extend_from_slice(&[0u8; 8]);
        let (img, offsets) = image(&[Record::Put {
            revision: 1,
            key: k("x"),
            value: &value,
        }]);
        let cut = img.len() - 2;
        assert!(matches!(
            parse(&img[..cut], Recovery::Strict),
            Err(ParseError::Corrupt { offset, .. }) if offset == offsets[0] as u64
        ));
    }
}
