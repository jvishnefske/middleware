//! The persisted channel binding (BD3): the payload's current
//! `middleware::loader::binding` record on flash.
//!
//! A binding region is two 4 KiB sectors ([`REGION_LEN`]); each may hold
//! one record at its start:
//!
//! | offset | field                                                   |
//! |--------|---------------------------------------------------------|
//! | 0      | magic `b"MWBD"`                                         |
//! | 4      | format version (1), then three zero bytes               |
//! | 8      | sequence number u32 LE                                  |
//! | 12     | record length u32 LE, `0..=BINDING_MAX` (0: no binding) |
//! | 16     | CRC-32 of bytes 4..16 and the record, u32 LE            |
//! | 20     | the binding record, zero padded to a whole word         |
//!
//! [`BindingStore::save`] works as the trust store's does: it erases the
//! sector that does *not* hold the current record, writes everything but
//! the magic, then the magic, and only then treats the new record as
//! current. A power cut leaves at most a half record that fails its
//! magic or CRC, so [`BindingStore::open`] still finds the previous one
//! (the valid record with the higher sequence number wins). A record of
//! length 0 means "no binding" ([`BindingStore::clear`]).
//!
//! The store keeps what it holds, never what it means: whether the record
//! is for the installed image is decided when the payload starts (the
//! shell compares the record's image SHA-256 with the image it loads, and
//! ignores a record for another image).

use crate::dispatch::{Action, Outcome, State};
use crate::image::{SECTOR, WORD};
use crate::store::StoreError;
use embedded_storage::nor_flash::NorFlash;
use middleware::crc::Crc32;
use middleware::loader::binding::{BINDING_MAX, Binding, BindingMark};
use middleware::transport::HandlerError;

/// Bytes of a binding region: two sectors.
pub const REGION_LEN: u32 = 2 * SECTOR;

/// Record magic.
pub const MAGIC: [u8; 4] = *b"MWBD";

/// Record format version.
pub const VERSION: u8 = 1;

/// Bytes before the binding record.
pub const HEADER_LEN: usize = 20;

/// Largest record on flash, padded to a word.
pub const RECORD_MAX: usize = HEADER_LEN + BINDING_MAX;

const fn padded(n: usize) -> usize {
    n.div_ceil(WORD as usize) * WORD as usize
}

fn crc(head: &[u8], record: &[u8]) -> u32 {
    Crc32::new().update(head).update(record).finish()
}

/// The persisted binding region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindingStore {
    base: u32,
    /// Sector (0 or 1), sequence number and record length of the current
    /// record.
    current: Option<(u32, u32, usize)>,
    /// The current binding's image and version.
    mark: Option<BindingMark>,
}

impl BindingStore {
    /// Reads the region at absolute offset `base` (sector aligned, at least
    /// [`REGION_LEN`] bytes). Unreadable sectors and torn records count as
    /// empty. A valid record that does not decode as a binding (a later
    /// format) is kept but has no mark.
    pub fn open<F: NorFlash>(flash: &mut F, base: u32, size: u32) -> Result<Self, StoreError> {
        if !base.is_multiple_of(SECTOR) {
            return Err(StoreError::Flash);
        }
        if size < REGION_LEN {
            return Err(StoreError::TooLarge);
        }
        let mut newest: Option<(u32, u32, usize)> = None;
        let mut buf = [0u8; padded(RECORD_MAX)];
        for sector in 0..2u32 {
            if let Some((seq, len)) = read_record(flash, base + sector * SECTOR, &mut buf)
                && newest.is_none_or(|(_, s, _)| seq > s)
            {
                newest = Some((sector, seq, len));
            }
        }
        let mut store = BindingStore {
            base,
            current: newest,
            mark: None,
        };
        store.mark = store.read_mark(flash, &mut buf);
        Ok(store)
    }

    fn read_mark<F: NorFlash>(&self, flash: &mut F, buf: &mut [u8]) -> Option<BindingMark> {
        let n = self.load(flash, buf).ok()??;
        Binding::decode(&buf[..n]).ok().map(|b| b.mark())
    }

    /// The current binding's image and version, `None` without one.
    pub const fn mark(&self) -> Option<BindingMark> {
        self.mark
    }

    /// Copies the current binding record into `out` (at least
    /// [`BINDING_MAX`] bytes, plus padding to a word); returns its length,
    /// `None` without one.
    pub fn load<F: NorFlash>(
        &self,
        flash: &mut F,
        out: &mut [u8],
    ) -> Result<Option<usize>, StoreError> {
        let Some((sector, _, len)) = self.current else {
            return Ok(None);
        };
        if len == 0 {
            return Ok(None);
        }
        let mut buf = [0u8; padded(RECORD_MAX)];
        let at = self.base + sector * SECTOR;
        read_record(flash, at, &mut buf).ok_or(StoreError::Flash)?;
        let dst = out.get_mut(..len).ok_or(StoreError::TooLarge)?;
        dst.copy_from_slice(&buf[HEADER_LEN..HEADER_LEN + len]);
        Ok(Some(len))
    }

    /// Persists `record` (an encoded binding, at most [`BINDING_MAX`]
    /// bytes; empty to clear) in the sector that does not hold the current
    /// one. On error the previous record is still what [`BindingStore::open`]
    /// finds.
    pub fn save<F: NorFlash>(&mut self, flash: &mut F, record: &[u8]) -> Result<(), StoreError> {
        if record.len() > BINDING_MAX {
            return Err(StoreError::TooLarge);
        }
        let mark = if record.is_empty() {
            None
        } else {
            Some(
                Binding::decode(record)
                    .map_err(|_| StoreError::TooLarge)?
                    .mark(),
            )
        };
        let (sector, seq) = match self.current {
            Some((s, seq, _)) => (1 - s, seq.wrapping_add(1)),
            None => (0, 1),
        };
        let mut buf = [0u8; padded(RECORD_MAX)];
        let len = record.len();
        buf[..4].copy_from_slice(&MAGIC);
        buf[4] = VERSION;
        buf[8..12].copy_from_slice(&seq.to_le_bytes());
        buf[12..16].copy_from_slice(&(len as u32).to_le_bytes());
        buf[HEADER_LEN..HEADER_LEN + len].copy_from_slice(record);
        let sum = crc(&buf[4..16], record);
        buf[16..20].copy_from_slice(&sum.to_le_bytes());
        let total = padded(HEADER_LEN + len);
        let at = self.base + sector * SECTOR;
        flash
            .erase(at, at + SECTOR)
            .map_err(|_| StoreError::Flash)?;
        flash
            .write(at + WORD, &buf[WORD as usize..total])
            .map_err(|_| StoreError::Flash)?;
        flash
            .write(at, &buf[..WORD as usize])
            .map_err(|_| StoreError::Flash)?;
        self.current = Some((sector, seq, len));
        self.mark = mark;
        Ok(())
    }

    /// Persists "no binding".
    pub fn clear<F: NorFlash>(&mut self, flash: &mut F) -> Result<(), StoreError> {
        self.save(flash, &[])
    }
}

/// Performs a dispatch outcome's binding action for a shell (BD3): for
/// [`Action::Bind`] saves the record and returns the next state with its
/// mark; for [`Action::Binding`] copies the stored record into `out`. The
/// reply is `Unavailable` without a binding region (an older flash layout
/// or partition table), `Internal` when the flash fails (the previous
/// binding stays), `NotFound` when there is no binding to read. `None` for
/// any other action, which the shell performs as before.
pub fn perform<F: NorFlash>(
    store: Option<&mut BindingStore>,
    flash: &mut F,
    outcome: &Outcome<'_>,
    out: &mut [u8],
) -> Option<(State, Result<usize, HandlerError>)> {
    let next = outcome.state;
    let reply = match (outcome.action, store) {
        (Action::Bind { .. } | Action::Binding, None) => Err(HandlerError::Unavailable),
        (Action::Bind { record, mark }, Some(s)) => match s.save(flash, record) {
            Ok(()) => return Some((next.bound(Some(mark)), Ok(0))),
            Err(_) => Err(HandlerError::Internal),
        },
        (Action::Binding, Some(s)) => match s.load(flash, out) {
            Ok(Some(n)) => Ok(n),
            Ok(None) => Err(HandlerError::NotFound),
            Err(StoreError::TooLarge) => Err(HandlerError::BufferTooSmall),
            Err(_) => Err(HandlerError::Internal),
        },
        _ => return None,
    };
    Some((next, reply))
}

/// Why a stored binding is not used for the image being started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ignored {
    /// The record names another image (an image installed after it).
    OtherImage,
    /// The record does not decode (a later format).
    Malformed,
}

/// The binding stored in `record` if it is for `image`: its image SHA-256
/// is the image's. Shells call this while the image is still in memory,
/// before compiling it, and start the payload with the binding, or without
/// one (logging why) otherwise. Without the `trust` feature the image
/// cannot be hashed and any record that decodes is used.
pub fn for_image<'r>(record: &'r [u8], image: &[u8]) -> Result<Binding<'r>, Ignored> {
    let b = Binding::decode(record).map_err(|_| Ignored::Malformed)?;
    #[cfg(feature = "trust")]
    if middleware::loader::sha256::ImageHash::digest(image) != b.image_sha256 {
        return Err(Ignored::OtherImage);
    }
    #[cfg(not(feature = "trust"))]
    let _ = image;
    Ok(b)
}

/// Reads and checks the record at `at` into `buf`; its sequence number
/// and length, `None` when there is no valid one.
fn read_record<F: NorFlash>(flash: &mut F, at: u32, buf: &mut [u8]) -> Option<(u32, usize)> {
    flash.read(at, buf.get_mut(..HEADER_LEN)?).ok()?;
    if buf[..4] != MAGIC || buf[4] != VERSION || buf[5..8] != [0; 3] {
        return None;
    }
    let word = |i: usize| u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
    let (seq, len, sum) = (word(8), word(12) as usize, word(16));
    if len > BINDING_MAX {
        return None;
    }
    let total = padded(HEADER_LEN + len);
    flash
        .read(at + HEADER_LEN as u32, buf.get_mut(HEADER_LEN..total)?)
        .ok()?;
    (crc(&buf[4..16], &buf[HEADER_LEN..HEADER_LEN + len]) == sum).then_some((seq, len))
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use middleware::key::Key;
    use middleware::loader::binding::Target;
    use middleware::loader::trust::Version;
    use std::vec::Vec;

    const BASE: u32 = SECTOR;
    type Flash = crate::ram_flash::RamFlash<{ (SECTOR + REGION_LEN) as usize }>;

    fn record(version: u32, key: &'static str) -> Vec<u8> {
        let mut b = Binding::new([5; 32], Version(version));
        b.bind("cmd", Target::Key(Key::from_static(key))).unwrap();
        let mut out = [0u8; BINDING_MAX];
        let n = b.encode(&mut out).unwrap();
        out[..n].to_vec()
    }

    fn loaded(s: &BindingStore, f: &mut Flash) -> Option<Vec<u8>> {
        let mut out = [0u8; padded(BINDING_MAX)];
        s.load(f, &mut out).unwrap().map(|n| out[..n].to_vec())
    }

    #[test]
    fn bd3_saves_alternate_sectors_and_reopen_to_the_newest() {
        let mut f = Flash::new();
        let mut s = BindingStore::open(&mut f, BASE, REGION_LEN).unwrap();
        assert_eq!((s.mark(), loaded(&s, &mut f)), (None, None));
        let (a, b) = (record(1, "a/k"), record(2, "bb/key"));
        s.save(&mut f, &a).unwrap();
        s.save(&mut f, &b).unwrap();
        let r = BindingStore::open(&mut f, BASE, REGION_LEN).unwrap();
        assert_eq!(loaded(&r, &mut f), Some(b.clone()));
        assert_eq!(r.mark().map(|m| m.version), Some(Version(2)));
        let mut r = r;
        r.clear(&mut f).unwrap();
        let r = BindingStore::open(&mut f, BASE, REGION_LEN).unwrap();
        assert_eq!((r.mark(), loaded(&r, &mut f)), (None, None));
        assert_eq!(s.save(&mut f, &[1, 2, 3]), Err(StoreError::TooLarge));
        assert_eq!(
            BindingStore::open(&mut f, BASE + 1, REGION_LEN),
            Err(StoreError::Flash)
        );
        assert_eq!(
            BindingStore::open(&mut f, BASE, SECTOR),
            Err(StoreError::TooLarge)
        );
    }

    #[test]
    fn bd3_a_binding_applies_only_to_its_image() {
        let image = b"\0asm\x01\0\0\0";
        let sha = middleware::loader::sha256::ImageHash::digest(image);
        let mut b = Binding::new(sha, Version(1));
        b.bind("cmd", Target::Key(Key::from_static("k"))).unwrap();
        let mut out = [0u8; BINDING_MAX];
        let n = b.encode(&mut out).unwrap();
        assert_eq!(for_image(&out[..n], image), Ok(b));
        assert_eq!(for_image(&out[..n], b"other"), Err(Ignored::OtherImage));
        assert_eq!(for_image(&[1, 2], image), Err(Ignored::Malformed));
    }

    #[test]
    fn bd3_perform_saves_reads_and_records_the_mark() {
        use crate::dispatch::{Context, handle_request};
        use middleware::loader::keys;
        use middleware::transport::Request;
        let ctx = Context {
            chip: "t",
            firmware: "0",
            capacity: 0,
            profile: None,
            board: None,
            runtime: true,
            rekey_window: false,
            staging: false,
            node: None,
            manifest: None,
        };
        let mut f = Flash::new();
        let mut store = BindingStore::open(&mut f, BASE, REGION_LEN).unwrap();
        let rec = record(4, "a/k");
        let mut out = [0u8; BINDING_MAX];
        let s = State::boot(true);
        let o = handle_request(s, &ctx, &Request::new(keys::BIND, &rec), &mut out);
        let (next, reply) = perform(Some(&mut store), &mut f, &o, &mut out).unwrap();
        assert_eq!(
            (reply, next.binding().map(|m| m.version)),
            (Ok(0), Some(Version(4)))
        );
        let o = handle_request(next, &ctx, &Request::new(keys::BINDING, &[]), &mut out);
        let (_, reply) = perform(Some(&mut store), &mut f, &o, &mut out).unwrap();
        assert_eq!(&out[..reply.unwrap()], &rec[..]);
        // No region: Unavailable; another action: not ours.
        assert_eq!(
            perform(None, &mut f, &o, &mut out).map(|(_, r)| r),
            Some(Err(HandlerError::Unavailable))
        );
        let info = handle_request(s, &ctx, &Request::new(keys::INFO, &[]), &mut out);
        assert!(perform(Some(&mut store), &mut f, &info, &mut out).is_none());
        // Nothing stored yet: NotFound.
        let mut g = Flash::new();
        let mut empty = BindingStore::open(&mut g, BASE, REGION_LEN).unwrap();
        assert_eq!(
            perform(Some(&mut empty), &mut g, &o, &mut out).map(|(_, r)| r),
            Some(Err(HandlerError::NotFound))
        );
    }

    #[test]
    fn bd3_a_cut_save_keeps_the_previous_binding() {
        let (a, b) = (record(1, "a/k"), record(2, "bb/key"));
        // Ops of a save: erase, write fields, write magic.
        for cut in 0..3 {
            let mut f = Flash::new();
            let mut s = BindingStore::open(&mut f, BASE, REGION_LEN).unwrap();
            s.save(&mut f, &a).unwrap();
            f.cut_at(cut);
            assert!(s.save(&mut f, &b).is_err(), "cut {cut}");
            f.restore();
            let r = BindingStore::open(&mut f, BASE, REGION_LEN).unwrap();
            assert_eq!(loaded(&r, &mut f), Some(a.clone()), "cut {cut}");
        }
        // A corrupted byte fails the CRC.
        let mut f = Flash::new();
        let mut s = BindingStore::open(&mut f, BASE, REGION_LEN).unwrap();
        s.save(&mut f, &a).unwrap();
        f.mem[(BASE as usize) + HEADER_LEN + 3] ^= 1;
        let r = BindingStore::open(&mut f, BASE, REGION_LEN).unwrap();
        assert_eq!(loaded(&r, &mut f), None);
    }
}
