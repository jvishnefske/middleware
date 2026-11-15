//! The request/reply format of a remote key-value store: what
//! [`RemoteKv`](super::remote::RemoteKv) sends and
//! [`KvHandler`](super::remote::KvHandler) answers. Pure and `no_std`: every
//! function encodes into or decodes from a caller's buffer.
//!
//! Decoding never panics: truncated, garbled or trailing input is a
//! [`DecodeError`]. Encoding returns `None` when the output buffer is too
//! small, never a truncated message.
//!
//! # Layout
//!
//! Every message starts with [`VERSION`]. Integers are little-endian. A key
//! is `[len u8][bytes]` with `len` 1..=[`MAX_KEY_LEN`], and its bytes must be
//! a valid [`Key`]. An [`Expect`] is one tag byte (`0` Any, `1` Absent, `2`
//! Exact) followed, for Exact, by the revision as `u64`.
//!
//! | request | body after `[VERSION][op]` |
//! |---|---|
//! | `1` get    | `[key]` |
//! | `2` put    | `[key][expect][value: the rest]` |
//! | `3` delete | `[key][expect]` |
//! | `4` scan   | `[max_entries u16, ≥ 1][filter kind u8: 0 exact, 1 prefix][filter key][after len u8][after bytes]`; after len 0 means no cursor |
//!
//! | response | body after `[VERSION][status]` |
//! |---|---|
//! | `0x00` absent        | — (get of an absent key; delete of an absent key) |
//! | `0x01` entry         | `[revision u64][value: the rest]` |
//! | `0x02` written       | `[revision u64]` (put: the new revision; delete: the removed entry's) |
//! | `0x03` page          | `[complete u8][count u16]` then `count` × `[key][revision u64][value len u32][value]`, keys strictly ascending |
//! | `0x10` conflict      | `[current u64]`, 0 for an absent key |
//! | `0x11` buffer too small | `[needed u32]`: the **responder's** reply buffer cannot hold the value |
//! | `0x12` too large     | — |
//! | `0x13` full          | — |
//! | `0x14` backend       | `[code u16]`: the served store failed |
//!
//! Revisions in responses are never 0 (revision 0 is never issued), except
//! as the "absent" marker of a conflict.
//!
//! # Sizes
//!
//! A client with `N`-byte request and response buffers can carry any key
//! and any value of up to [`max_value`]`(N)` bytes: a put needs at most
//! [`PUT_OVERHEAD`] bytes besides the value, a get reply [`ENTRY_VALUE_AT`],
//! and a scan page holding that one entry [`PAGE_ENTRY_OVERHEAD`]. A scan
//! request needs up to [`SCAN_REQUEST_MAX`] bytes, so `N` must be at least
//! [`MIN_BUFFER`].

use crate::key::{Filter, Key, KeyFilter, MAX_KEY_LEN};

use super::{Expect, KvError, Revision};

/// The format version, first byte of every message.
pub const VERSION: u8 = 1;

const OP_GET: u8 = 1;
const OP_PUT: u8 = 2;
const OP_DELETE: u8 = 3;
const OP_SCAN: u8 = 4;

const EXPECT_ANY: u8 = 0;
const EXPECT_ABSENT: u8 = 1;
const EXPECT_EXACT: u8 = 2;

const FILTER_EXACT: u8 = 0;
const FILTER_PREFIX: u8 = 1;

const STATUS_ABSENT: u8 = 0x00;
const STATUS_ENTRY: u8 = 0x01;
const STATUS_WRITTEN: u8 = 0x02;
const STATUS_PAGE: u8 = 0x03;
const STATUS_CONFLICT: u8 = 0x10;
const STATUS_BUFFER_TOO_SMALL: u8 = 0x11;
const STATUS_TOO_LARGE: u8 = 0x12;
const STATUS_FULL: u8 = 0x13;
const STATUS_BACKEND: u8 = 0x14;

/// Bytes of a put request besides its value, with the longest key and an
/// Exact expect: `[ver][op][len][key 128][tag][rev 8]`.
pub const PUT_OVERHEAD: usize = 2 + 1 + MAX_KEY_LEN + 9;
/// Offset of the value in an entry response: `[ver][status][rev 8]`.
pub const ENTRY_VALUE_AT: usize = 10;
/// Header of a page response: `[ver][status][complete][count 2]`.
pub const PAGE_HEADER: usize = 5;
/// Bytes of one page entry besides its key and value:
/// `[key len][rev 8][value len 4]`.
pub const PAGE_ENTRY_FIXED: usize = 13;
/// A page holding one entry with the longest key, besides its value.
pub const PAGE_ENTRY_OVERHEAD: usize = PAGE_HEADER + PAGE_ENTRY_FIXED + MAX_KEY_LEN;
/// The longest scan request: two longest keys.
pub const SCAN_REQUEST_MAX: usize = 2 + 2 + 1 + 1 + MAX_KEY_LEN + 1 + MAX_KEY_LEN;
/// The smallest request/response buffer a client may use.
pub const MIN_BUFFER: usize = SCAN_REQUEST_MAX;

/// The largest value a client with `n`-byte buffers can carry in every
/// operation (0 if `n` is below the fixed overheads).
pub const fn max_value(n: usize) -> usize {
    let overhead = if PAGE_ENTRY_OVERHEAD > PUT_OVERHEAD {
        PAGE_ENTRY_OVERHEAD
    } else {
        PUT_OVERHEAD
    };
    n.saturating_sub(overhead)
}

/// Why bytes could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// The message ended inside a field.
    Truncated,
    /// The first byte is not [`VERSION`].
    Version(u8),
    /// An unknown operation or status byte.
    Unknown(u8),
    /// A field is out of range: an invalid key, an unknown expect or filter
    /// tag, a zero revision, a zero `max_entries`, a flag other than 0 or
    /// 1, or page keys out of order.
    Invalid,
    /// Bytes are left over after the message.
    Trailing,
}

/// A decoded request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request<'a> {
    /// Read one key.
    Get {
        /// The key to read.
        key: Key<'a>,
    },
    /// Conditionally write one key.
    Put {
        /// The key to write.
        key: Key<'a>,
        /// The precondition.
        expect: Expect,
        /// The new value.
        value: &'a [u8],
    },
    /// Conditionally remove one key.
    Delete {
        /// The key to remove.
        key: Key<'a>,
        /// The precondition.
        expect: Expect,
    },
    /// One page of a scan.
    Scan {
        /// Which keys to visit.
        filter: KeyFilter<'a>,
        /// Visit keys strictly after this one.
        after: Option<Key<'a>>,
        /// At most this many entries in the page (at least 1).
        max_entries: u16,
    },
}

/// A failed operation as carried on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// [`KvError::Conflict`].
    Conflict {
        /// The key's revision, `None` if absent.
        current: Option<Revision>,
    },
    /// The responder's reply buffer cannot hold a value of `needed` bytes
    /// (not the caller's buffer: the client checks that itself).
    BufferTooSmall {
        /// Bytes the value (or page entry) needs.
        needed: u32,
    },
    /// [`KvError::TooLarge`].
    TooLarge,
    /// [`KvError::Full`].
    Full,
    /// [`KvError::Backend`], reduced to a code chosen by the responder.
    Backend(u16),
}

impl Failure {
    /// The wire form of a store error; `code` reduces a backend error.
    pub fn from_kv<E>(error: &KvError<E>, code: impl FnOnce(&E) -> u16) -> Self {
        match error {
            KvError::Conflict { current } => Failure::Conflict { current: *current },
            KvError::BufferTooSmall { needed } => Failure::BufferTooSmall {
                needed: u32::try_from(*needed).unwrap_or(u32::MAX),
            },
            KvError::TooLarge => Failure::TooLarge,
            KvError::Full => Failure::Full,
            KvError::Backend(e) => Failure::Backend(code(e)),
        }
    }
}

/// A decoded response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Response<'a> {
    /// The key is absent (get), or was already absent (delete).
    Absent,
    /// A get's value and its revision.
    Entry {
        /// The revision of the write that set the value.
        revision: Revision,
        /// The value.
        value: &'a [u8],
    },
    /// A put's new revision, or a delete's removed revision.
    Written(Revision),
    /// One page of a scan.
    Page(Page<'a>),
    /// The operation failed.
    Failed(Failure),
}

/// A validated scan page; iterate it with [`Page::entries`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page<'a> {
    /// True if the responder has no matching entry after the last one in
    /// the page.
    pub complete: bool,
    /// Entries in the page.
    pub count: u16,
    /// The encoded entries, already validated.
    entries: &'a [u8],
}

/// One entry of a [`Page`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageEntry<'a> {
    /// The entry's key.
    pub key: Key<'a>,
    /// The revision of the write that set it.
    pub revision: Revision,
    /// Its value.
    pub value: &'a [u8],
}

impl<'a> Page<'a> {
    /// The page's entries, in ascending key order.
    pub fn entries(&self) -> PageEntries<'a> {
        PageEntries {
            reader: Reader::new(self.entries),
            left: self.count,
        }
    }
}

/// Iterator over a [`Page`]'s entries.
#[derive(Debug, Clone)]
pub struct PageEntries<'a> {
    reader: Reader<'a>,
    left: u16,
}

impl<'a> Iterator for PageEntries<'a> {
    type Item = PageEntry<'a>;

    fn next(&mut self) -> Option<PageEntry<'a>> {
        self.left = self.left.checked_sub(1)?;
        // The page was validated when decoded, so this cannot fail; if it
        // somehow did, the iteration just ends.
        self.reader.page_entry().ok()
    }
}

/// A cursor over input bytes; every read is checked.
#[derive(Debug, Clone)]
struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Reader { bytes }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let (head, rest) = self
            .bytes
            .split_at_checked(n)
            .ok_or(DecodeError::Truncated)?;
        self.bytes = rest;
        Ok(head)
    }

    fn array<const L: usize>(&mut self) -> Result<[u8; L], DecodeError> {
        let b = self.take(L)?;
        <[u8; L]>::try_from(b).map_err(|_| DecodeError::Truncated)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(u8::from_le_bytes(self.array()?))
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn flag(&mut self) -> Result<bool, DecodeError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(DecodeError::Invalid),
        }
    }

    fn rest(&mut self) -> &'a [u8] {
        core::mem::take(&mut self.bytes)
    }

    /// A key of `len` bytes (1..=MAX_KEY_LEN).
    fn key_of(&mut self, len: usize) -> Result<Key<'a>, DecodeError> {
        if len == 0 || len > MAX_KEY_LEN {
            return Err(DecodeError::Invalid);
        }
        let bytes = self.take(len)?;
        let s = core::str::from_utf8(bytes).map_err(|_| DecodeError::Invalid)?;
        Key::new(s).map_err(|_| DecodeError::Invalid)
    }

    fn key(&mut self) -> Result<Key<'a>, DecodeError> {
        let len = usize::from(self.u8()?);
        self.key_of(len)
    }

    /// A revision that must not be 0.
    fn revision(&mut self) -> Result<Revision, DecodeError> {
        match self.u64()? {
            0 => Err(DecodeError::Invalid),
            r => Ok(Revision(r)),
        }
    }

    fn expect(&mut self) -> Result<Expect, DecodeError> {
        match self.u8()? {
            EXPECT_ANY => Ok(Expect::Any),
            EXPECT_ABSENT => Ok(Expect::Absent),
            EXPECT_EXACT => Ok(Expect::Exact(Revision(self.u64()?))),
            _ => Err(DecodeError::Invalid),
        }
    }

    fn version(&mut self) -> Result<(), DecodeError> {
        match self.u8()? {
            VERSION => Ok(()),
            v => Err(DecodeError::Version(v)),
        }
    }

    fn page_entry(&mut self) -> Result<PageEntry<'a>, DecodeError> {
        let key = self.key()?;
        let revision = self.revision()?;
        let len = usize::try_from(self.u32()?).map_err(|_| DecodeError::Truncated)?;
        let value = self.take(len)?;
        Ok(PageEntry {
            key,
            revision,
            value,
        })
    }

    fn end(&self) -> Result<(), DecodeError> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::Trailing)
        }
    }
}

/// A cursor over an output buffer; every write is checked.
struct Writer<'o> {
    out: &'o mut [u8],
    pos: usize,
}

impl<'o> Writer<'o> {
    fn new(out: &'o mut [u8]) -> Self {
        Writer { out, pos: 0 }
    }

    fn bytes(&mut self, b: &[u8]) -> Option<()> {
        let end = self.pos.checked_add(b.len())?;
        self.out.get_mut(self.pos..end)?.copy_from_slice(b);
        self.pos = end;
        Some(())
    }

    fn u8(&mut self, v: u8) -> Option<()> {
        self.bytes(&[v])
    }

    fn key(&mut self, key: Key<'_>) -> Option<()> {
        let b = key.as_bytes();
        self.u8(u8::try_from(b.len()).ok()?)?;
        self.bytes(b)
    }

    fn expect(&mut self, e: Expect) -> Option<()> {
        match e {
            Expect::Any => self.u8(EXPECT_ANY),
            Expect::Absent => self.u8(EXPECT_ABSENT),
            Expect::Exact(r) => {
                self.u8(EXPECT_EXACT)?;
                self.bytes(&r.0.to_le_bytes())
            }
        }
    }

    fn done(self) -> usize {
        self.pos
    }
}

impl<'a> Request<'a> {
    /// Writes the request into `out` and returns its length; `None` if
    /// `out` is too small.
    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        let mut w = Writer::new(out);
        w.u8(VERSION)?;
        match *self {
            Request::Get { key } => {
                w.u8(OP_GET)?;
                w.key(key)?;
            }
            Request::Put { key, expect, value } => {
                w.u8(OP_PUT)?;
                w.key(key)?;
                w.expect(expect)?;
                w.bytes(value)?;
            }
            Request::Delete { key, expect } => {
                w.u8(OP_DELETE)?;
                w.key(key)?;
                w.expect(expect)?;
            }
            Request::Scan {
                filter,
                after,
                max_entries,
            } => {
                w.u8(OP_SCAN)?;
                w.bytes(&max_entries.to_le_bytes())?;
                let kind = match filter {
                    Filter::Exact(_) => FILTER_EXACT,
                    Filter::Prefix(_) => FILTER_PREFIX,
                };
                w.u8(kind)?;
                w.key(filter.key())?;
                match after {
                    Some(a) => w.key(a)?,
                    None => w.u8(0)?,
                }
            }
        }
        Some(w.done())
    }

    /// Parses a request; any malformed input is an error.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(bytes);
        r.version()?;
        let req = match r.u8()? {
            OP_GET => Request::Get { key: r.key()? },
            OP_PUT => {
                let key = r.key()?;
                let expect = r.expect()?;
                Request::Put {
                    key,
                    expect,
                    value: r.rest(),
                }
            }
            OP_DELETE => {
                let key = r.key()?;
                Request::Delete {
                    key,
                    expect: r.expect()?,
                }
            }
            OP_SCAN => {
                let max_entries = r.u16()?;
                if max_entries == 0 {
                    return Err(DecodeError::Invalid);
                }
                let kind = r.u8()?;
                let key = r.key()?;
                let filter = match kind {
                    FILTER_EXACT => Filter::Exact(key),
                    FILTER_PREFIX => Filter::Prefix(key),
                    _ => return Err(DecodeError::Invalid),
                };
                let after = match usize::from(r.u8()?) {
                    0 => None,
                    len => Some(r.key_of(len)?),
                };
                Request::Scan {
                    filter,
                    after,
                    max_entries,
                }
            }
            op => return Err(DecodeError::Unknown(op)),
        };
        r.end()?;
        Ok(req)
    }
}

impl<'a> Response<'a> {
    /// Writes the response into `out` and returns its length; `None` if
    /// `out` is too small. A [`Response::Page`] is copied as decoded; a
    /// responder builds new pages with [`PageWriter`].
    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        let mut w = Writer::new(out);
        w.u8(VERSION)?;
        match *self {
            Response::Absent => w.u8(STATUS_ABSENT)?,
            Response::Entry { revision, value } => {
                w.u8(STATUS_ENTRY)?;
                w.bytes(&revision.0.to_le_bytes())?;
                w.bytes(value)?;
            }
            Response::Written(r) => {
                w.u8(STATUS_WRITTEN)?;
                w.bytes(&r.0.to_le_bytes())?;
            }
            Response::Page(p) => {
                w.u8(STATUS_PAGE)?;
                w.u8(u8::from(p.complete))?;
                w.bytes(&p.count.to_le_bytes())?;
                w.bytes(p.entries)?;
            }
            Response::Failed(f) => match f {
                Failure::Conflict { current } => {
                    w.u8(STATUS_CONFLICT)?;
                    w.bytes(&current.map_or(0, |r| r.0).to_le_bytes())?;
                }
                Failure::BufferTooSmall { needed } => {
                    w.u8(STATUS_BUFFER_TOO_SMALL)?;
                    w.bytes(&needed.to_le_bytes())?;
                }
                Failure::TooLarge => w.u8(STATUS_TOO_LARGE)?,
                Failure::Full => w.u8(STATUS_FULL)?,
                Failure::Backend(code) => {
                    w.u8(STATUS_BACKEND)?;
                    w.bytes(&code.to_le_bytes())?;
                }
            },
        }
        Some(w.done())
    }

    /// Parses a response; any malformed input is an error. A page is
    /// validated whole (every entry, key order, no trailing bytes) before
    /// it is returned, so iterating it cannot fail.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(bytes);
        r.version()?;
        let resp = match r.u8()? {
            STATUS_ABSENT => Response::Absent,
            STATUS_ENTRY => {
                let revision = r.revision()?;
                Response::Entry {
                    revision,
                    value: r.rest(),
                }
            }
            STATUS_WRITTEN => Response::Written(r.revision()?),
            STATUS_PAGE => {
                let complete = r.flag()?;
                let count = r.u16()?;
                let entries = r.rest();
                validate_page(entries, count)?;
                Response::Page(Page {
                    complete,
                    count,
                    entries,
                })
            }
            STATUS_CONFLICT => Response::Failed(Failure::Conflict {
                current: match r.u64()? {
                    0 => None,
                    c => Some(Revision(c)),
                },
            }),
            STATUS_BUFFER_TOO_SMALL => {
                Response::Failed(Failure::BufferTooSmall { needed: r.u32()? })
            }
            STATUS_TOO_LARGE => Response::Failed(Failure::TooLarge),
            STATUS_FULL => Response::Failed(Failure::Full),
            STATUS_BACKEND => Response::Failed(Failure::Backend(r.u16()?)),
            s => return Err(DecodeError::Unknown(s)),
        };
        r.end()?;
        Ok(resp)
    }
}

/// Checks `count` well-formed entries with strictly ascending keys fill
/// `entries` exactly. Bounded by `count` (at most `u16::MAX`).
fn validate_page(entries: &[u8], count: u16) -> Result<(), DecodeError> {
    let mut r = Reader::new(entries);
    let mut previous: Option<Key<'_>> = None;
    for _ in 0..count {
        let e = r.page_entry()?;
        if previous.is_some_and(|p| p.as_bytes() >= e.key.as_bytes()) {
            return Err(DecodeError::Invalid);
        }
        previous = Some(e.key);
    }
    r.end()
}

/// Writes `[VERSION][entry status][revision]` into `out`, the header of an
/// entry response whose value the responder has already placed at
/// [`ENTRY_VALUE_AT`]. Returns the header length; `None` if `out` is too
/// small.
pub fn encode_entry_header(out: &mut [u8], revision: Revision) -> Option<usize> {
    let mut w = Writer::new(out);
    w.u8(VERSION)?;
    w.u8(STATUS_ENTRY)?;
    w.bytes(&revision.0.to_le_bytes())?;
    Some(w.done())
}

/// Builds a page response in place, entry by entry, without a second
/// buffer: a responder asks for [`PageWriter::value_slot`], has its store
/// read the next value straight into it, then [`PageWriter::commit`]s the
/// entry, which moves the value next to its header.
#[derive(Debug)]
pub struct PageWriter<'o> {
    out: &'o mut [u8],
    pos: usize,
    count: u16,
}

impl<'o> PageWriter<'o> {
    /// A writer over `out`; `None` if it cannot hold the page header.
    pub fn new(out: &'o mut [u8]) -> Option<Self> {
        if out.len() < PAGE_HEADER {
            return None;
        }
        Some(PageWriter {
            out,
            pos: PAGE_HEADER,
            count: 0,
        })
    }

    /// Entries committed so far.
    pub const fn count(&self) -> u16 {
        self.count
    }

    /// Where the next entry's value goes when its key may be up to
    /// `key_room` bytes long: the free space past room for that entry's
    /// header. Empty when there is no room at all.
    pub fn value_slot(&mut self, key_room: usize) -> &mut [u8] {
        let at = self
            .pos
            .saturating_add(PAGE_ENTRY_FIXED)
            .saturating_add(key_room);
        self.out.get_mut(at..).unwrap_or(&mut [])
    }

    /// Commits the entry whose `value_len`-byte value was written to
    /// [`PageWriter::value_slot`]`(key_room)`. `None` (and nothing
    /// committed) if `key` is longer than `key_room`, the value did not fit
    /// the slot, or the page already holds `u16::MAX` entries.
    pub fn commit(
        &mut self,
        key: Key<'_>,
        revision: Revision,
        value_len: usize,
        key_room: usize,
    ) -> Option<()> {
        let k = key.as_bytes();
        if k.len() > key_room {
            return None;
        }
        let count = self.count.checked_add(1)?;
        let len = u32::try_from(value_len).ok()?;
        let src = self
            .pos
            .checked_add(PAGE_ENTRY_FIXED)?
            .checked_add(key_room)?;
        let src_end = src.checked_add(value_len)?;
        let dst = self
            .pos
            .checked_add(PAGE_ENTRY_FIXED)?
            .checked_add(k.len())?;
        if src_end > self.out.len() {
            return None;
        }
        // Moves the value back over the unused key room (dst <= src), then
        // writes the header in front of it.
        self.out.copy_within(src..src_end, dst);
        let mut w = Writer {
            out: &mut *self.out,
            pos: self.pos,
        };
        w.key(key)?;
        w.bytes(&revision.0.to_le_bytes())?;
        w.bytes(&len.to_le_bytes())?;
        self.pos = dst.checked_add(value_len)?;
        self.count = count;
        Some(())
    }

    /// Copies one entry into the page; `None` (nothing written) if it does
    /// not fit.
    pub fn push(&mut self, key: Key<'_>, revision: Revision, value: &[u8]) -> Option<()> {
        let room = key.as_bytes().len();
        let slot = self.value_slot(room).get_mut(..value.len())?;
        slot.copy_from_slice(value);
        self.commit(key, revision, value.len(), room)
    }

    /// Writes the page header and returns the page's length.
    pub fn finish(self, complete: bool) -> usize {
        let [lo, hi] = self.count.to_le_bytes();
        let header = [VERSION, STATUS_PAGE, u8::from(complete), lo, hi];
        if let Some(h) = self.out.get_mut(..PAGE_HEADER) {
            h.copy_from_slice(&header);
        }
        self.pos
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    const A: Key<'static> = Key::from_static("app/a");
    const B: Key<'static> = Key::from_static("app/b");

    fn requests() -> [Request<'static>; 7] {
        [
            Request::Get { key: A },
            Request::Put {
                key: A,
                expect: Expect::Any,
                value: b"hello",
            },
            Request::Put {
                key: B,
                expect: Expect::Exact(Revision(u64::MAX)),
                value: b"",
            },
            Request::Delete {
                key: A,
                expect: Expect::Absent,
            },
            Request::Scan {
                filter: KeyFilter::Prefix(Key::from_static("app")),
                after: Some(A),
                max_entries: 7,
            },
            Request::Scan {
                filter: KeyFilter::Exact(B),
                after: None,
                max_entries: u16::MAX,
            },
            Request::Delete {
                key: B,
                expect: Expect::Exact(Revision(3)),
            },
        ]
    }

    fn page_bytes() -> Vec<u8> {
        let mut buf = [0u8; 128];
        let mut w = PageWriter::new(&mut buf).unwrap();
        w.push(A, Revision(1), b"one").unwrap();
        w.push(B, Revision(9), b"").unwrap();
        let n = w.finish(true);
        buf[..n].to_vec()
    }

    fn responses() -> [Response<'static>; 9] {
        [
            Response::Absent,
            Response::Entry {
                revision: Revision(2),
                value: b"v",
            },
            Response::Written(Revision(5)),
            Response::Failed(Failure::Conflict { current: None }),
            Response::Failed(Failure::Conflict {
                current: Some(Revision(4)),
            }),
            Response::Failed(Failure::BufferTooSmall { needed: 300 }),
            Response::Failed(Failure::TooLarge),
            Response::Failed(Failure::Full),
            Response::Failed(Failure::Backend(0xBEEF)),
        ]
    }

    #[test]
    fn requests_round_trip() {
        for req in requests() {
            let mut buf = [0u8; 64];
            let n = req.encode(&mut buf).unwrap();
            assert_eq!(Request::decode(&buf[..n]), Ok(req), "{req:?}");
            // Too small an output buffer is None, never a partial message.
            assert_eq!(req.encode(&mut buf[..n - 1]), None);
        }
    }

    #[test]
    fn responses_round_trip() {
        for resp in responses() {
            let mut buf = [0u8; 64];
            let n = resp.encode(&mut buf).unwrap();
            assert_eq!(Response::decode(&buf[..n]), Ok(resp), "{resp:?}");
            assert_eq!(resp.encode(&mut buf[..n - 1]), None);
        }
    }

    #[test]
    fn layout_is_as_documented() {
        let mut buf = [0u8; 64];
        let n = Request::Put {
            key: Key::from_static("k"),
            expect: Expect::Exact(Revision(2)),
            value: b"xy",
        }
        .encode(&mut buf)
        .unwrap();
        assert_eq!(
            &buf[..n],
            &[1, 2, 1, b'k', 2, 2, 0, 0, 0, 0, 0, 0, 0, b'x', b'y']
        );
        let n = Request::Scan {
            filter: KeyFilter::Prefix(Key::from_static("k")),
            after: None,
            max_entries: 3,
        }
        .encode(&mut buf)
        .unwrap();
        assert_eq!(&buf[..n], &[1, 4, 3, 0, 1, 1, b'k', 0]);
        let n = Response::Written(Revision(1)).encode(&mut buf).unwrap();
        assert_eq!(&buf[..n], &[1, 2, 1, 0, 0, 0, 0, 0, 0, 0]);
        let page = page_bytes();
        assert_eq!(&page[..PAGE_HEADER], &[1, 3, 1, 2, 0]);
        assert_eq!(&page[PAGE_HEADER..PAGE_HEADER + 6], b"\x05app/a");
    }

    #[test]
    fn sizes_cover_the_worst_case() {
        let long = [b'k'; MAX_KEY_LEN];
        let long = Key::new(core::str::from_utf8(&long).unwrap()).unwrap();
        let mut buf = [0u8; 1024];
        let n = Request::Scan {
            filter: KeyFilter::Prefix(long),
            after: Some(long),
            max_entries: 1,
        }
        .encode(&mut buf)
        .unwrap();
        assert_eq!(n, SCAN_REQUEST_MAX);
        let value = [7u8; 100];
        let n = Request::Put {
            key: long,
            expect: Expect::Exact(Revision(1)),
            value: &value,
        }
        .encode(&mut buf)
        .unwrap();
        assert_eq!(n, PUT_OVERHEAD + value.len());
        let mut w = PageWriter::new(&mut buf).unwrap();
        w.push(long, Revision(1), &value).unwrap();
        assert_eq!(w.finish(true), PAGE_ENTRY_OVERHEAD + value.len());
        assert_eq!(max_value(MIN_BUFFER), MIN_BUFFER - PAGE_ENTRY_OVERHEAD);
        assert_eq!(max_value(10), 0);
    }

    #[test]
    fn pages_decode_and_iterate() {
        let page = page_bytes();
        let Ok(Response::Page(p)) = Response::decode(&page) else {
            panic!("not a page");
        };
        assert!(p.complete);
        let got: Vec<_> = p.entries().collect();
        assert_eq!(
            got,
            [
                PageEntry {
                    key: A,
                    revision: Revision(1),
                    value: b"one"
                },
                PageEntry {
                    key: B,
                    revision: Revision(9),
                    value: b""
                },
            ]
        );
        // Re-encoding a decoded page reproduces it.
        let mut buf = [0u8; 128];
        let n = Response::Page(p).encode(&mut buf).unwrap();
        assert_eq!(&buf[..n], &page[..]);
    }

    #[test]
    fn page_writer_moves_values_over_unused_key_room() {
        let mut buf = [0u8; 64];
        let mut w = PageWriter::new(&mut buf).unwrap();
        // The store reads the value into a slot sized for a 20-byte key.
        let slot = w.value_slot(20);
        slot[..3].copy_from_slice(b"abc");
        w.commit(A, Revision(3), 3, 20).unwrap();
        // A key longer than its room, or a value past the end, is refused.
        assert_eq!(
            w.commit(Key::from_static("app/zz"), Revision(1), 1, 2),
            None
        );
        assert_eq!(w.commit(B, Revision(1), 64, 20), None);
        assert_eq!(w.count(), 1);
        let n = w.finish(false);
        let Ok(Response::Page(p)) = Response::decode(&buf[..n]) else {
            panic!("not a page");
        };
        assert!(!p.complete);
        let e = p.entries().next().unwrap();
        assert_eq!((e.key, e.revision, e.value), (A, Revision(3), &b"abc"[..]));
        // No room at all: the slot is empty and push refuses.
        let mut tiny = [0u8; PAGE_HEADER + 3];
        let mut w = PageWriter::new(&mut tiny).unwrap();
        assert!(w.value_slot(MAX_KEY_LEN).is_empty());
        assert_eq!(w.push(A, Revision(1), b""), None);
        assert_eq!(w.finish(true), PAGE_HEADER);
        assert!(PageWriter::new(&mut [0u8; 4]).is_none());
    }

    #[test]
    fn entry_header_matches_entry_encoding() {
        let mut a = [0u8; 16];
        let mut b = [0u8; 16];
        let n = encode_entry_header(&mut a, Revision(7)).unwrap();
        assert_eq!(n, ENTRY_VALUE_AT);
        a[n..n + 2].copy_from_slice(b"hi");
        let m = Response::Entry {
            revision: Revision(7),
            value: b"hi",
        }
        .encode(&mut b)
        .unwrap();
        assert_eq!(&a[..n + 2], &b[..m]);
        assert_eq!(encode_entry_header(&mut a[..9], Revision(7)), None);
    }

    #[test]
    fn specific_malformations_are_named() {
        use DecodeError::*;
        let cases: [(&[u8], DecodeError); 14] = [
            (&[], Truncated),
            (&[2, 1, 1, b'a'], Version(2)),
            (&[1, 9], Unknown(9)),
            (&[1, 1, 0], Invalid),                         // zero-length key
            (&[1, 1, 2, b'a'], Truncated),                 // key cut short
            (&[1, 1, 1, b'*'], Invalid),                   // not a valid key
            (&[1, 1, 1, 0xff], Invalid),                   // not UTF-8
            (&[1, 1, 1, b'a', 0], Trailing),               // get with extra bytes
            (&[1, 3, 1, b'a', 7], Invalid),                // unknown expect tag
            (&[1, 3, 1, b'a', 2, 1], Truncated),           // exact without revision
            (&[1, 4, 0, 0, 0, 1, b'a', 0], Invalid),       // max_entries 0
            (&[1, 4, 1, 0, 5, 1, b'a', 0], Invalid),       // unknown filter kind
            (&[1, 4, 1, 0, 0, 1, b'a'], Truncated),        // scan without cursor byte
            (&[1, 4, 1, 0, 0, 1, b'a', 1, b'/'], Invalid), // bad cursor key
        ];
        for (bytes, want) in cases {
            assert_eq!(Request::decode(bytes), Err(want), "{bytes:?}");
        }
        let cases: [(&[u8], DecodeError); 8] = [
            (&[1], Truncated),
            (&[1, 0x7f], Unknown(0x7f)),
            (&[1, 2, 0, 0, 0, 0, 0, 0, 0, 0], Invalid), // revision 0
            (&[1, 2, 1, 0, 0, 0, 0, 0, 0], Truncated),
            (&[1, 0, 0], Trailing),
            (&[1, 3, 2, 0, 0], Invalid),   // complete flag 2
            (&[1, 3, 1, 1, 0], Truncated), // one entry promised, none there
            (&[1, 0x11, 1, 0], Truncated),
        ];
        for (bytes, want) in cases {
            assert_eq!(Response::decode(bytes), Err(want), "{bytes:?}");
        }
    }

    #[test]
    fn pages_out_of_order_or_with_trailing_bytes_are_rejected() {
        let mut buf = [0u8; 128];
        // Build B then A by hand: the writer does not sort.
        let mut w = PageWriter::new(&mut buf).unwrap();
        w.push(B, Revision(1), b"").unwrap();
        w.push(A, Revision(1), b"").unwrap();
        let n = w.finish(true);
        assert_eq!(Response::decode(&buf[..n]), Err(DecodeError::Invalid));
        let mut w = PageWriter::new(&mut buf).unwrap();
        w.push(A, Revision(1), b"").unwrap();
        w.push(A, Revision(2), b"").unwrap();
        let n = w.finish(true);
        assert_eq!(Response::decode(&buf[..n]), Err(DecodeError::Invalid));
        let mut page = page_bytes();
        page.push(0);
        assert_eq!(Response::decode(&page), Err(DecodeError::Trailing));
    }

    /// A small deterministic generator (xorshift), so the fuzz-style cases
    /// are reproducible without a dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    #[test]
    fn every_truncation_of_a_valid_message_is_an_error() {
        let mut encoded: Vec<(bool, Vec<u8>)> = Vec::new();
        for req in requests() {
            let mut buf = [0u8; 64];
            let n = req.encode(&mut buf).unwrap();
            encoded.push((true, buf[..n].to_vec()));
        }
        for resp in responses() {
            let mut buf = [0u8; 64];
            let n = resp.encode(&mut buf).unwrap();
            encoded.push((false, buf[..n].to_vec()));
        }
        encoded.push((false, page_bytes()));
        for (is_request, bytes) in &encoded {
            for cut in 0..bytes.len() {
                let short = &bytes[..cut];
                let decoded_ok = if *is_request {
                    Request::decode(short).is_ok()
                } else {
                    Response::decode(short).is_ok()
                };
                // A put's value and an entry's value are "the rest", so
                // cutting them yields a shorter valid message; nothing else
                // may survive truncation.
                let tail = if *is_request { OP_PUT } else { STATUS_ENTRY };
                let rest_valued = bytes.get(1) == Some(&tail);
                if decoded_ok {
                    assert!(rest_valued, "truncated to {cut} decoded: {bytes:?}");
                }
            }
        }
    }

    #[test]
    fn garbled_input_never_panics_and_valid_decodes_reencode() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut seeds: Vec<Vec<u8>> = Vec::new();
        for req in requests() {
            let mut buf = [0u8; 64];
            let n = req.encode(&mut buf).unwrap();
            seeds.push(buf[..n].to_vec());
        }
        seeds.push(page_bytes());
        for round in 0..20_000u32 {
            let mut bytes = seeds[round as usize % seeds.len()].clone();
            // Flip, insert or drop a few bytes.
            for _ in 0..(rng.next() % 4 + 1) {
                let at = (rng.next() as usize) % (bytes.len() + 1);
                match rng.next() % 3 {
                    0 if at < bytes.len() => bytes[at] = rng.next() as u8,
                    1 => bytes.insert(at, rng.next() as u8),
                    _ if at < bytes.len() => {
                        bytes.remove(at);
                    }
                    _ => {}
                }
            }
            let mut out = [0u8; 512];
            if let Ok(req) = Request::decode(&bytes) {
                let n = req.encode(&mut out).unwrap();
                assert_eq!(&out[..n], &bytes[..], "request did not re-encode");
            }
            if let Ok(resp) = Response::decode(&bytes) {
                if let Response::Page(p) = resp {
                    assert_eq!(p.entries().count(), usize::from(p.count));
                }
                let n = resp.encode(&mut out).unwrap();
                assert_eq!(&out[..n], &bytes[..], "response did not re-encode");
            }
        }
        // Pure noise.
        for len in 0..64 {
            let noise: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            let _ = Request::decode(&noise);
            let _ = Response::decode(&noise);
        }
    }

    #[test]
    fn failures_from_store_errors() {
        let code = |e: &u8| u16::from(*e);
        assert_eq!(
            Failure::from_kv(&KvError::Conflict { current: None }, code),
            Failure::Conflict { current: None }
        );
        assert_eq!(
            Failure::from_kv(&KvError::<u8>::BufferTooSmall { needed: 9 }, code),
            Failure::BufferTooSmall { needed: 9 }
        );
        assert_eq!(
            Failure::from_kv(&KvError::<u8>::TooLarge, code),
            Failure::TooLarge
        );
        assert_eq!(Failure::from_kv(&KvError::<u8>::Full, code), Failure::Full);
        assert_eq!(
            Failure::from_kv(&KvError::Backend(42u8), code),
            Failure::Backend(42)
        );
    }
}
