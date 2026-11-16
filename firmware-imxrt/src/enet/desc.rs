//! Legacy ENET buffer descriptors and the receive and transmit rings over
//! DMA memory.
//!
//! The ring memory is `&[AtomicU32]` the ENET DMA also reads and writes (in
//! the firmware a `static` in DTCM, which is not cached, so no cache
//! maintenance). Each legacy descriptor is two words (8 bytes); with
//! ECR.DBSWP set the DMA uses them as little-endian words:
//!
//! | word | bits  | field |
//! |------|-------|-------|
//! | 0    | 31:16 | control and status (below) |
//! | 0    | 15:0  | data length |
//! | 1    | 31:0  | buffer address |
//!
//! Receive status, word 0: E (31, owned by the DMA: empty), RO1 (30),
//! W (29, wrap: the last descriptor), RO2 (28), L (27, last buffer of a
//! frame), M (24, a promiscuous miss), BC (23, broadcast), MC (22,
//! multicast), LG (21, too long), NO (20, non-octet), CR (18, CRC error),
//! OV (17, FIFO overrun), TR (16, truncated).
//!
//! Transmit control, word 0: R (31, owned by the DMA: ready), TO1 (30),
//! W (29), TO2 (28), L (27), TC (26, append the CRC), ABC (25).
//!
//! The ownership hand-off: the CPU fills a buffer, then stores word 0 with
//! E or R set (the DMA owns it); the DMA clears E or R when it is done
//! (the CPU owns it again). Every store that gives a descriptor to the DMA
//! is preceded and followed by a full fence (a `DMB` on the Cortex-M7), so
//! the buffer is written before the hand-off and the hand-off before the
//! shell's RDAR/TDAR write; every take is a load followed by a fence, so
//! the buffer is read after it. A ring holds one [`Cursor`] that moves
//! through the descriptors in the DMA's order and wraps at W.

use core::sync::atomic::{AtomicU32, Ordering, fence};

/// Words per legacy descriptor.
pub const DESC_WORDS: usize = 2;
/// Bytes per legacy descriptor.
pub const DESC_BYTES: usize = DESC_WORDS * 4;
/// Ring base (RDSR / TDSR) alignment in bytes. The ENET needs 8; 64 keeps
/// each ring on its own cache line should it ever move to cached memory.
pub const RING_ALIGN: u32 = 64;
/// Receive buffer address alignment in bytes.
pub const RX_BUFFER_ALIGN: u32 = 16;
/// The bytes each receive buffer holds (MRBR, [`super::mac::MRBR`]).
pub const RX_BUFFER_BYTES: usize = 1536;
/// The longest frame sent, without the CRC the MAC appends (TC):
/// 1518 − 4.
pub const MAX_TX_FRAME: usize = 1514;

/// The shortest frame put on the wire without the CRC (64 − 4); shorter
/// frames are zero padded to it by [`padded_len`].
pub const MIN_TX_FRAME: usize = 60;

/// The length to send a frame of `len` bytes as: at least
/// [`MIN_TX_FRAME`] (the bytes after `len` being zero).
pub const fn padded_len(len: usize) -> usize {
    if len < MIN_TX_FRAME { MIN_TX_FRAME } else { len }
}

/// Receive: owned by the DMA (empty).
pub const RX_E: u32 = 1 << 31;
/// Wrap: the last descriptor of the ring (receive and transmit).
pub const W: u32 = 1 << 29;
/// Last buffer of a frame (receive and transmit).
pub const L: u32 = 1 << 27;
/// Receive: a frame accepted only because of promiscuous mode.
pub const RX_M: u32 = 1 << 24;
/// Receive: broadcast destination.
pub const RX_BC: u32 = 1 << 23;
/// Receive: multicast destination.
pub const RX_MC: u32 = 1 << 22;
/// Receive: frame longer than RCR.MAX_FL.
pub const RX_LG: u32 = 1 << 21;
/// Receive: non-octet-aligned frame.
pub const RX_NO: u32 = 1 << 20;
/// Receive: CRC error.
pub const RX_CR: u32 = 1 << 18;
/// Receive: FIFO overrun.
pub const RX_OV: u32 = 1 << 17;
/// Receive: frame truncated (longer than the buffer chain).
pub const RX_TR: u32 = 1 << 16;
/// Receive: every error bit (LG, NO, CR, OV, TR), the probe's
/// `(w0 >> 16) & 0x37`.
pub const RX_ERRORS: u32 = RX_LG | RX_NO | RX_CR | RX_OV | RX_TR;
/// Transmit: owned by the DMA (ready).
pub const TX_R: u32 = 1 << 31;
/// Transmit: append the CRC.
pub const TX_TC: u32 = 1 << 26;
/// The data length field of word 0.
pub const LEN_MASK: u32 = 0xFFFF;

/// Why a ring operation was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingError {
    /// The ring memory is empty or not a whole number of descriptors.
    BadRingMemory,
    /// The buffer list is not one address per descriptor.
    BufferCount,
    /// A buffer address is not aligned as the DMA needs.
    BufferAlignment,
    /// The descriptor at the cursor is owned by the DMA.
    NotOwned,
    /// A frame length of zero or past [`MAX_TX_FRAME`] or the buffer.
    BadLength,
}

/// A position in a ring of `len` descriptors that advances and wraps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    index: usize,
    len: usize,
}

impl Cursor {
    /// The first descriptor of a ring of `len`; `None` for an empty ring.
    pub const fn new(len: usize) -> Option<Cursor> {
        if len == 0 { None } else { Some(Cursor { index: 0, len }) }
    }

    /// The descriptor index.
    pub const fn index(&self) -> usize {
        self.index
    }

    /// The ring's descriptor count.
    pub const fn count(&self) -> usize {
        self.len
    }

    /// Whether this is the ring's last descriptor (the one with W).
    pub const fn is_last(&self) -> bool {
        self.index + 1 >= self.len
    }

    /// Moves to the next descriptor, back to 0 after the last.
    pub const fn advance(&mut self) {
        self.index = if self.is_last() { 0 } else { self.index + 1 };
    }

    /// Back to the first descriptor (as the DMA after ECR enable).
    pub const fn reset(&mut self) {
        self.index = 0;
    }
}

/// The receive descriptor word 0 given to the DMA: E, and W on the last.
pub const fn rx_empty_word(last: bool) -> u32 {
    if last { RX_E | W } else { RX_E }
}

/// The transmit descriptor word 0 that sends `len` bytes: R, L, TC, and W
/// on the last; `None` for a zero length or one past [`MAX_TX_FRAME`].
pub const fn tx_ready_word(len: usize, last: bool) -> Option<u32> {
    if len == 0 || len > MAX_TX_FRAME {
        return None;
    }
    let w = if last { W } else { 0 };
    Some(TX_R | w | L | TX_TC | len as u32)
}

/// The receive error flags of a descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct RxErrors {
    /// LG: longer than RCR.MAX_FL.
    pub too_long: bool,
    /// NO: not a whole number of octets.
    pub non_octet: bool,
    /// CR: bad CRC.
    pub crc: bool,
    /// OV: receive FIFO overrun.
    pub overrun: bool,
    /// TR: truncated.
    pub truncated: bool,
}

impl RxErrors {
    /// Whether any error is set.
    pub const fn any(&self) -> bool {
        self.too_long || self.non_octet || self.crc || self.overrun || self.truncated
    }
}

/// A received descriptor the CPU owns, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RxFrame {
    /// The descriptor (and buffer) index.
    pub index: usize,
    /// The data length the DMA wrote (the frame's length when `last`).
    pub len: u16,
    /// L: the last (with MRBR ≥ MAX_FL, the only) buffer of the frame.
    pub last: bool,
    /// BC: sent to the broadcast address.
    pub broadcast: bool,
    /// MC: sent to a multicast address.
    pub multicast: bool,
    /// M: accepted only because the MAC is promiscuous.
    pub miss: bool,
    /// The error flags.
    pub errors: RxErrors,
}

impl RxFrame {
    /// Decodes receive word 0 `word0` of descriptor `index`; `None` while
    /// the DMA owns it (E set).
    pub const fn decode(index: usize, word0: u32) -> Option<RxFrame> {
        if word0 & RX_E != 0 {
            return None;
        }
        Some(RxFrame {
            index,
            len: (word0 & LEN_MASK) as u16,
            last: word0 & L != 0,
            broadcast: word0 & RX_BC != 0,
            multicast: word0 & RX_MC != 0,
            miss: word0 & RX_M != 0,
            errors: RxErrors {
                too_long: word0 & RX_LG != 0,
                non_octet: word0 & RX_NO != 0,
                crc: word0 & RX_CR != 0,
                overrun: word0 & RX_OV != 0,
                truncated: word0 & RX_TR != 0,
            },
        })
    }

    /// The frame's length when it can be handed up: a whole frame in this
    /// one buffer (L), no error, not empty and within the buffer.
    pub const fn good_len(&self) -> Option<usize> {
        let len = self.len as usize;
        if self.last && !self.errors.any() && len > 0 && len <= RX_BUFFER_BYTES {
            Some(len)
        } else {
            None
        }
    }
}

/// Whether `addr` can be a ring base (RDSR / TDSR): [`RING_ALIGN`]-aligned
/// and not zero.
pub const fn ring_base_ok(addr: u32) -> bool {
    addr != 0 && addr.is_multiple_of(RING_ALIGN)
}

fn word0(words: &[AtomicU32], index: usize) -> Option<&AtomicU32> {
    words.get(index.checked_mul(DESC_WORDS)?)
}

fn word1(words: &[AtomicU32], index: usize) -> Option<&AtomicU32> {
    words.get(index.checked_mul(DESC_WORDS)?.checked_add(1)?)
}

/// Gives a descriptor to the DMA: fence, store word 0, fence.
fn hand_to_dma(cell: &AtomicU32, value: u32) {
    fence(Ordering::SeqCst);
    cell.store(value, Ordering::Relaxed);
    fence(Ordering::SeqCst);
}

/// Reads word 0, then fences so the buffer is read after it.
fn take(cell: &AtomicU32) -> u32 {
    let v = cell.load(Ordering::Relaxed);
    fence(Ordering::SeqCst);
    v
}

fn descriptor_count(words: &[AtomicU32]) -> Option<usize> {
    let n = words.len() / DESC_WORDS;
    if n == 0 || !words.len().is_multiple_of(DESC_WORDS) { None } else { Some(n) }
}

/// The receive ring: descriptors over `words`, a cursor at the next
/// descriptor the DMA fills.
pub struct RxRing<'a> {
    words: &'a [AtomicU32],
    cursor: Cursor,
}

impl<'a> RxRing<'a> {
    /// A ring over `words` (two per descriptor); `None` for memory that is
    /// empty or not whole descriptors.
    pub fn new(words: &'a [AtomicU32]) -> Option<RxRing<'a>> {
        let cursor = Cursor::new(descriptor_count(words)?)?;
        Some(RxRing { words, cursor })
    }

    /// The ring's cursor.
    pub const fn cursor(&self) -> Cursor {
        self.cursor
    }

    /// Writes every descriptor: buffer `buffers[i]` (each
    /// [`RX_BUFFER_ALIGN`]-aligned, [`RX_BUFFER_BYTES`] long) and word 0
    /// given to the DMA, W on the last; the cursor back to 0. Nothing is
    /// written when refused.
    pub fn init(&mut self, buffers: &[u32]) -> Result<(), RingError> {
        if buffers.len() != self.cursor.count() {
            return Err(RingError::BufferCount);
        }
        if buffers.iter().any(|a| *a == 0 || !a.is_multiple_of(RX_BUFFER_ALIGN)) {
            return Err(RingError::BufferAlignment);
        }
        let n = self.cursor.count();
        for (i, addr) in buffers.iter().enumerate() {
            if let (Some(w0), Some(w1)) = (word0(self.words, i), word1(self.words, i)) {
                w1.store(*addr, Ordering::Relaxed);
                w0.store(rx_empty_word(i + 1 == n), Ordering::Relaxed);
            }
        }
        fence(Ordering::SeqCst);
        self.cursor.reset();
        Ok(())
    }

    /// The frame at the cursor when the DMA has handed it back, else
    /// `None`. The buffer at [`RxFrame::index`] may be read until
    /// [`release`](Self::release).
    pub fn peek(&self) -> Option<RxFrame> {
        let i = self.cursor.index();
        RxFrame::decode(i, take(word0(self.words, i)?))
    }

    /// Gives the descriptor at the cursor back to the DMA (empty) and
    /// advances; the index released. The shell then writes RDAR.
    pub fn release(&mut self) -> Result<usize, RingError> {
        let i = self.cursor.index();
        let w0 = word0(self.words, i).ok_or(RingError::BadRingMemory)?;
        if w0.load(Ordering::Relaxed) & RX_E != 0 {
            return Err(RingError::NotOwned);
        }
        hand_to_dma(w0, rx_empty_word(self.cursor.is_last()));
        self.cursor.advance();
        Ok(i)
    }
}

/// The transmit ring: descriptors over `words`, a cursor at the next
/// descriptor to send from.
pub struct TxRing<'a> {
    words: &'a [AtomicU32],
    cursor: Cursor,
}

impl<'a> TxRing<'a> {
    /// A ring over `words` (two per descriptor); `None` for memory that is
    /// empty or not whole descriptors.
    pub fn new(words: &'a [AtomicU32]) -> Option<TxRing<'a>> {
        let cursor = Cursor::new(descriptor_count(words)?)?;
        Some(TxRing { words, cursor })
    }

    /// The ring's cursor.
    pub const fn cursor(&self) -> Cursor {
        self.cursor
    }

    /// Writes every descriptor: buffer `buffers[i]` and word 0 owned by the
    /// CPU (R clear), W on the last; the cursor back to 0. Nothing is
    /// written when refused.
    pub fn init(&mut self, buffers: &[u32]) -> Result<(), RingError> {
        if buffers.len() != self.cursor.count() {
            return Err(RingError::BufferCount);
        }
        if buffers.contains(&0) {
            return Err(RingError::BufferAlignment);
        }
        let n = self.cursor.count();
        for (i, addr) in buffers.iter().enumerate() {
            if let (Some(w0), Some(w1)) = (word0(self.words, i), word1(self.words, i)) {
                w1.store(*addr, Ordering::Relaxed);
                w0.store(if i + 1 == n { W } else { 0 }, Ordering::Relaxed);
            }
        }
        fence(Ordering::SeqCst);
        self.cursor.reset();
        Ok(())
    }

    /// The index of the descriptor at the cursor when the CPU owns it (its
    /// buffer may be filled), else `None` (the ring is full).
    pub fn free(&self) -> Option<usize> {
        let i = self.cursor.index();
        let w = take(word0(self.words, i)?);
        if w & TX_R == 0 { Some(i) } else { None }
    }

    /// Hands the descriptor at the cursor to the DMA to send `len` bytes of
    /// its buffer and advances; the index submitted. The shell then writes
    /// TDAR.
    pub fn submit(&mut self, len: usize) -> Result<usize, RingError> {
        let i = self.cursor.index();
        let w0 = word0(self.words, i).ok_or(RingError::BadRingMemory)?;
        if w0.load(Ordering::Relaxed) & TX_R != 0 {
            return Err(RingError::NotOwned);
        }
        let value = tx_ready_word(len, self.cursor.is_last()).ok_or(RingError::BadLength)?;
        hand_to_dma(w0, value);
        self.cursor.advance();
        Ok(i)
    }

    /// Whether descriptor `index` is still owned by the DMA (not yet sent);
    /// `false` for an index outside the ring.
    pub fn pending(&self, index: usize) -> bool {
        word0(self.words, index).is_some_and(|w| w.load(Ordering::Relaxed) & TX_R != 0)
    }
}

/// Copies the first `len` bytes of a frame buffer (little-endian words, as
/// the DMA writes them with DBSWP) into `out`; the bytes copied, at most
/// `out` and the buffer.
pub fn copy_frame(buffer: &[AtomicU32], len: usize, out: &mut [u8]) -> usize {
    let n = len.min(out.len()).min(buffer.len().saturating_mul(4));
    let Some(dst) = out.get_mut(..n) else { return 0 };
    for (chunk, word) in dst.chunks_mut(4).zip(buffer) {
        let bytes = word.load(Ordering::Relaxed).to_le_bytes();
        for (d, s) in chunk.iter_mut().zip(bytes) {
            *d = s;
        }
    }
    n
}

/// Writes `data` into a frame buffer as little-endian words (the last word
/// zero padded). Refused when it does not fit.
pub fn fill_frame(buffer: &[AtomicU32], data: &[u8]) -> Result<(), RingError> {
    if data.len() > buffer.len().saturating_mul(4) {
        return Err(RingError::BadLength);
    }
    for (word, chunk) in buffer.iter().zip(data.chunks(4)) {
        let mut b = [0u8; 4];
        for (d, s) in b.iter_mut().zip(chunk) {
            *d = *s;
        }
        word.store(u32::from_le_bytes(b), Ordering::Relaxed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem<const N: usize>() -> [AtomicU32; N] {
        [const { AtomicU32::new(0) }; N]
    }

    fn load(w: &[AtomicU32], i: usize) -> u32 {
        w.get(i).map_or(0xDEAD_BEEF, |c| c.load(Ordering::Relaxed))
    }

    fn set(w: &[AtomicU32], i: usize, v: u32) {
        if let Some(c) = w.get(i) {
            c.store(v, Ordering::Relaxed);
        }
    }

    #[test]
    fn fl3_descriptor_bits_match_probe() {
        assert_eq!(RX_E, 1 << 31);
        assert_eq!(W, 1 << 29);
        assert_eq!(L, 1 << 27);
        assert_eq!(TX_R, 1 << 31);
        assert_eq!(TX_TC, 1 << 26);
        assert_eq!(RX_ERRORS, 0x37 << 16);
        assert_eq!(DESC_BYTES, 8);
    }

    #[test]
    fn fl3_cursor_advances_and_wraps() {
        assert_eq!(Cursor::new(0), None);
        let mut c = Cursor::new(4).expect("cursor");
        let mut seen = [0usize; 9];
        for slot in seen.iter_mut() {
            *slot = c.index();
            c.advance();
        }
        assert_eq!(seen, [0, 1, 2, 3, 0, 1, 2, 3, 0]);
        let mut one = Cursor::new(1).expect("cursor");
        assert!(one.is_last());
        one.advance();
        assert_eq!(one.index(), 0);
    }

    #[test]
    fn fl3_rx_ring_init_matches_probe() {
        let words = mem::<8>();
        let mut ring = RxRing::new(&words).expect("ring");
        let bufs = [0x2000_0000, 0x2000_0600, 0x2000_0C00, 0x2000_1200];
        assert_eq!(ring.init(&bufs), Ok(()));
        for (i, addr) in bufs.iter().enumerate() {
            let expect = if i == 3 { RX_E | W } else { RX_E };
            assert_eq!(load(&words, 2 * i), expect);
            assert_eq!(load(&words, 2 * i + 1), *addr);
        }
        assert_eq!(load(&words, 6), 0xA000_0000);
        assert_eq!(ring.peek(), None);
    }

    #[test]
    fn fl3_rx_ring_init_refusals() {
        let odd = mem::<3>();
        assert!(RxRing::new(&odd).is_none());
        let empty = mem::<0>();
        assert!(RxRing::new(&empty).is_none());
        let words = mem::<4>();
        let mut ring = RxRing::new(&words).expect("ring");
        assert_eq!(ring.init(&[0x2000_0000]), Err(RingError::BufferCount));
        assert_eq!(ring.init(&[0x2000_0000, 0x2000_0608]), Err(RingError::BufferAlignment));
        assert_eq!(ring.init(&[0, 0x2000_0600]), Err(RingError::BufferAlignment));
        assert_eq!(load(&words, 0), 0, "nothing written when refused");
    }

    #[test]
    fn fl3_rx_ownership_hand_off_and_wrap() {
        let words = mem::<8>();
        let mut ring = RxRing::new(&words).expect("ring");
        assert_eq!(ring.init(&[16, 32, 48, 64]), Ok(()));
        // Release while the DMA owns it is refused and does not advance.
        assert_eq!(ring.release(), Err(RingError::NotOwned));
        for round in 0..8usize {
            let i = round % 4;
            assert_eq!(ring.cursor().index(), i);
            // The DMA fills descriptor i: E cleared, keeps W, L, length.
            let w = load(&words, 2 * i) & W;
            set(&words, 2 * i, w | L | (60 + round as u32));
            let f = ring.peek().expect("frame");
            assert_eq!(f.index, i);
            assert_eq!(f.good_len(), Some(60 + round));
            assert_eq!(ring.release(), Ok(i));
            assert_eq!(load(&words, 2 * i), rx_empty_word(i == 3));
            assert_eq!(ring.peek(), None);
        }
    }

    #[test]
    fn fl3_rx_decode_length_and_flags() {
        assert_eq!(RxFrame::decode(0, RX_E | L | 60), None);
        let f = RxFrame::decode(2, L | RX_BC | 342).expect("frame");
        assert_eq!((f.index, f.len, f.last, f.broadcast, f.multicast), (2, 342, true, true, false));
        assert_eq!(f.good_len(), Some(342));
        let f = RxFrame::decode(0, L | RX_MC | RX_M | 90).expect("frame");
        assert!(f.multicast && f.miss);
        assert_eq!(f.good_len(), Some(90), "a promiscuous miss is not an error");
        for (bit, which) in [
            (RX_LG, RxErrors { too_long: true, ..RxErrors::default() }),
            (RX_NO, RxErrors { non_octet: true, ..RxErrors::default() }),
            (RX_CR, RxErrors { crc: true, ..RxErrors::default() }),
            (RX_OV, RxErrors { overrun: true, ..RxErrors::default() }),
            (RX_TR, RxErrors { truncated: true, ..RxErrors::default() }),
        ] {
            let f = RxFrame::decode(0, L | bit | 64).expect("frame");
            assert_eq!(f.errors, which);
            assert_eq!(f.good_len(), None);
            // The probe's error test agrees.
            assert_ne!(((L | bit | 64) >> 16) & 0x37, 0);
        }
        // Not the last buffer, empty, or longer than the buffer: not good.
        assert_eq!(RxFrame::decode(0, 64).and_then(|f| f.good_len()), None);
        assert_eq!(RxFrame::decode(0, L).and_then(|f| f.good_len()), None);
        assert_eq!(RxFrame::decode(0, L | 1537).and_then(|f| f.good_len()), None);
        assert_eq!(RxFrame::decode(0, L | 1536).and_then(|f| f.good_len()), Some(1536));
        // Every word decodes without panicking.
        for w in [0u32, u32::MAX, 0x7FFF_FFFF, 0x5555_5555] {
            let _ = RxFrame::decode(usize::MAX, w).and_then(|f| f.good_len());
        }
    }

    #[test]
    fn fl3_tx_ring_matches_probe() {
        // The probe: one TX descriptor, W at init, R | W | L | TC | len.
        let words = mem::<2>();
        let mut ring = TxRing::new(&words).expect("ring");
        assert_eq!(ring.init(&[0x2000_2000]), Ok(()));
        assert_eq!(load(&words, 0), W);
        assert_eq!(load(&words, 1), 0x2000_2000);
        assert_eq!(ring.free(), Some(0));
        assert_eq!(ring.submit(342), Ok(0));
        assert_eq!(load(&words, 0), TX_R | W | L | TX_TC | 342);
        assert_eq!(load(&words, 0), 0xAC00_0156);
        assert!(ring.pending(0));
        assert_eq!(ring.free(), None);
        assert_eq!(ring.submit(60), Err(RingError::NotOwned));
        // The DMA sends it and clears R.
        set(&words, 0, load(&words, 0) & !TX_R);
        assert!(!ring.pending(0));
        assert_eq!(ring.free(), Some(0));
    }

    #[test]
    fn fl3_tx_ring_wraps_and_refuses_lengths() {
        let words = mem::<6>();
        let mut ring = TxRing::new(&words).expect("ring");
        assert_eq!(ring.init(&[1, 2]), Err(RingError::BufferCount));
        assert_eq!(ring.init(&[1, 0, 3]), Err(RingError::BufferAlignment));
        assert_eq!(ring.init(&[0x100, 0x700, 0xD00]), Ok(()));
        assert_eq!((load(&words, 0), load(&words, 2), load(&words, 4)), (0, 0, W));
        assert_eq!(ring.submit(0), Err(RingError::BadLength));
        assert_eq!(ring.submit(MAX_TX_FRAME + 1), Err(RingError::BadLength));
        assert_eq!(ring.cursor().index(), 0, "a refusal does not advance");
        assert_eq!(ring.submit(MAX_TX_FRAME), Ok(0));
        assert_eq!(ring.submit(60), Ok(1));
        assert_eq!(ring.submit(60), Ok(2));
        assert_eq!(load(&words, 4), TX_R | W | L | TX_TC | 60);
        assert_eq!(ring.free(), None, "back at 0, still owned by the DMA");
        assert!(!ring.pending(3));
        assert!(!ring.pending(usize::MAX));
        assert_eq!(tx_ready_word(14, false), Some(TX_R | L | TX_TC | 14));
    }

    #[test]
    fn fl3_frame_buffer_copy_round_trip() {
        let buf = mem::<4>();
        let data = [1u8, 2, 3, 4, 5, 6, 7];
        assert_eq!(fill_frame(&buf, &data), Ok(()));
        assert_eq!(load(&buf, 0), 0x0403_0201);
        assert_eq!(load(&buf, 1), 0x0007_0605);
        let mut out = [0u8; 16];
        assert_eq!(copy_frame(&buf, 7, &mut out), 7);
        assert_eq!(out.get(..7), Some(&data[..]));
        // Bounded by the output and the buffer.
        let mut small = [0u8; 3];
        assert_eq!(copy_frame(&buf, 7, &mut small), 3);
        assert_eq!(small, [1, 2, 3]);
        assert_eq!(copy_frame(&buf, usize::MAX, &mut out), 16);
        assert_eq!(fill_frame(&buf, &[0u8; 17]), Err(RingError::BadLength));
    }

    #[test]
    fn fl3_ring_base_alignment() {
        // The probe's ring in DTCM.
        assert!(ring_base_ok(0x2004_1E00));
        assert!(!ring_base_ok(0x2004_1E08));
        assert!(!ring_base_ok(0));
    }

    #[test]
    fn fl3_short_frames_padded_to_60() {
        assert_eq!(padded_len(42), 60);
        assert_eq!(padded_len(0), 60);
        assert_eq!(padded_len(60), 60);
        assert_eq!(padded_len(1514), 1514);
    }
}
