//! micro:bit v2 (nRF52833) bootloader: pure logic.
//!
//! This module holds everything that does not touch hardware, so the host
//! unit tests (`cargo test --lib --target x86_64-unknown-linux-gnu`) can cover
//! it. `src/main.rs` drives the registers and calls these functions.
//!
//! # Flash map (nRF52833, 512 KiB)
//!
//! | range               | owner                                   |
//! |---------------------|-----------------------------------------|
//! | `0x00000..0x04000`  | this bootloader (16 KiB)                |
//! | `0x04000..0x6E000`  | application (424 KiB); the only range this bootloader writes |
//! | `0x6E000..0x6F000`  | trust record                            |
//! | `0x6F000..0x70000`  | config                                  |
//! | `0x70000..0x80000`  | payload                                 |
//!
//! The bootloader never writes UICR (`0x10001000`) or FICR, and refuses any
//! address outside `APP_BASE..APP_END`.
//!
//! # Boot decision
//!
//! At reset, reflash mode is entered if button A (P0.14, active low) is held,
//! or if the application vector table is invalid ([`vector_table_valid`]).
//! Otherwise the bootloader sets VTOR to `0x4000`, sets MSP to the app's
//! initial SP and branches to its reset vector.
//!
//! # Reflash header (host to device)
//!
//! Sent over the VCOM UART (UARTE0, TX = P0.06, RX = P1.08, 115200 8N1).
//! The layout is the 60-byte `MWFW` record of `middleware::fw_ota::record`,
//! with A/B and sequence/rollback meaning removed. Every header field is
//! checked before anything is erased.
//!
//! ```text
//! offset  size  field
//! 0       4     magic  b"MWFW"
//! 4       1     format = 1
//! 5       1     flags  = 0x01 (pending set, rebooted clear); anything else is rejected
//! 6       1     slot   = 0 (single slot)
//! 7       1     zero
//! 8       4     sequence    u32 LE, ignored
//! 12      4     min_version u32 LE, ignored
//! 16      4     version     u32 LE, ignored
//! 20      4     len         u32 LE, image length, 1..=0x6A000
//! 24      32    SHA-256 of the image
//! 56      4     CRC-32 (IEEE) of bytes 0..56, u32 LE
//! ```
//!
//! The header is followed by `len` image bytes, written at `0x4000`.
//!
//! CRC-32 is IEEE, reflected, polynomial `0xEDB88320`, init `0xFFFFFFFF`,
//! xorout `0xFFFFFFFF`: the same function as `middleware::crc::Crc32`. It
//! protects the header only; the image is checked with SHA-256.
//!
//! # Reply sequence (one byte each)
//!
//! * `0x15` NAK, sent at once if the header is invalid (magic, format,
//!   flags, slot, zero byte, length range or header CRC). Nothing was erased
//!   or written. The parser then waits for the magic again.
//! * `0x06` ACK, sent after a valid header once the app pages covering
//!   `0x4000..0x4000+len` are erased. The host sends the body only after this.
//! * `0x18` CAN, on an inter-byte timeout inside a frame. If any body byte
//!   was written, page `0x4000` is erased so the app stays invalid.
//! * `0x06` ACK, sent after the body when its SHA-256 matches the header.
//! * `0x15` NAK, sent after the body when the digest does not match. Page
//!   `0x4000` is erased, so the next reset stays in reflash mode.
//!
//! Bytes that are not part of a frame are ignored until the magic appears
//! again (the parser resynchronises on the 4-byte magic).
//!
//! # Write strategy
//!
//! RAM is too small to buffer the image, so the bootloader erases the app
//! pages covering `0x4000..0x4000+len` after the header is accepted, then
//! writes words as they arrive. The first two words (the vector table) are
//! held back in RAM. After the body arrives, the digest is computed over the
//! image as it is in flash, with the held-back vector bytes taken from RAM in
//! place of the erased words. Only if that matches are the vector words
//! written, so the vector table never becomes valid for an unverified image.
//! The written words are read back to confirm them.
//!
//! //! # Size budget
//!
//! The bootloader links into a 16 KiB FLASH region (`memory.x`). If `.text`,
//! `.rodata` or `.data` (LMA) exceed 16 384 bytes the link fails with
//! `region FLASH overflowed`. Check the result with
//! `size -A target/thumbv7em-none-eabihf/release/bootloader-nrf52`.

#![cfg_attr(not(test), no_std)]

use sha2::{Digest, Sha256};

/// Start of the application (and of the bootloader's writable region).
pub const APP_BASE: u32 = 0x0000_4000;
/// Exclusive end of the application region (start of the trust record).
pub const APP_END: u32 = 0x0006_E000;
/// Largest image the frame may carry: the whole application region.
pub const APP_MAX_LEN: u32 = APP_END - APP_BASE;
/// nRF52833 RAM.
pub const RAM_START: u32 = 0x2000_0000;
/// One past the last RAM byte (128 KiB).
pub const RAM_END: u32 = 0x2002_0000;
/// NVMC erase granularity.
pub const PAGE_SIZE: u32 = 4096;

/// Header magic.
pub const MAGIC: [u8; 4] = *b"MWFW";
/// Magic as the big-endian window the parser shifts bytes through.
const MAGIC_WORD: u32 = u32::from_be_bytes(MAGIC);
/// Header length in bytes, magic included.
pub const HEADER_LEN: usize = 60;
/// Header format byte.
pub const FORMAT: u8 = 1;
/// The only accepted flags byte: pending set, rebooted clear.
pub const FLAGS: u8 = 0x01;
/// The only accepted slot.
pub const SLOT: u8 = 0;
/// Reply: header accepted and pages erased (send the body), or image verified.
pub const ACK: u8 = 0x06;
/// Reply: header invalid, or image digest mismatch.
pub const NAK: u8 = 0x15;
/// Reply: inter-byte timeout inside a frame.
pub const CAN: u8 = 0x18;

/// True if `len` bytes at `addr` lie entirely inside the application region.
pub const fn in_app(addr: u32, len: u32) -> bool {
    let end = addr as u64 + len as u64;
    addr >= APP_BASE && end <= APP_END as u64
}

/// True if an application vector table with initial SP `sp` and reset vector
/// `reset` is plausible: SP is word aligned and inside RAM (the top of RAM
/// counts, since a stack pointer starts one past its last word), and the reset
/// vector has the Thumb bit set and points inside the application.
pub const fn vector_table_valid(sp: u32, reset: u32) -> bool {
    sp >= RAM_START && sp <= RAM_END && sp % 4 == 0
        && reset & 1 == 1
        && in_app(reset & !1, 1)
}

/// Number of erase pages needed for an image of `len` bytes.
pub const fn pages_for(len: u32) -> u32 {
    (len + PAGE_SIZE - 1) / PAGE_SIZE
}

/// Exclusive end of the erased range for an image of `len` bytes: `APP_BASE`
/// plus the length rounded up to whole pages. Never exceeds `APP_END` for a
/// length accepted by the parser.
pub const fn erase_end(len: u32) -> u32 {
    APP_BASE + pages_for(len) * PAGE_SIZE
}

/// CRC-32 (IEEE, reflected) state, identical to `middleware::crc::Crc32`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crc32(u32);

impl Crc32 {
    pub const fn new() -> Self {
        Crc32(0xFFFF_FFFF)
    }

    pub const fn update(self, data: &[u8]) -> Self {
        let mut crc = self.0;
        let mut i = 0;
        while i < data.len() {
            crc ^= data[i] as u32;
            let mut bit = 0;
            while bit < 8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
                bit += 1;
            }
            i += 1;
        }
        Crc32(crc)
    }

    pub const fn finish(self) -> u32 {
        !self.0
    }
}

/// CRC-32 of `data` in one call.
pub const fn crc32(data: &[u8]) -> u32 {
    Crc32::new().update(data).finish()
}

/// A header that passed every check. Nothing has been erased yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// Image length in bytes, `1..=APP_MAX_LEN`.
    pub len: u32,
    /// SHA-256 the image must match.
    pub sha256: [u8; 32],
}

/// Checks a complete 60-byte header. Returns `None` unless the magic, format,
/// flags, slot, zero byte, length range and header CRC are all valid. The
/// sequence, min_version and version fields are not checked.
pub fn parse_header(b: &[u8; HEADER_LEN]) -> Option<Header> {
    if &b[..4] != &MAGIC || b[4] != FORMAT || b[5] != FLAGS || b[6] != SLOT || b[7] != 0 {
        return None;
    }
    let len = u32::from_le_bytes([b[20], b[21], b[22], b[23]]);
    if len == 0 || len > APP_MAX_LEN {
        return None;
    }
    let crc = u32::from_le_bytes([b[56], b[57], b[58], b[59]]);
    if crc32(&b[..56]) != crc {
        return None;
    }
    let mut sha256 = [0u8; 32];
    sha256.copy_from_slice(&b[24..56]);
    Some(Header { len, sha256 })
}

/// SHA-256 of the concatenation of `parts`.
pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for part in parts {
        h.update(part);
    }
    h.finalize().into()
}

/// True if the SHA-256 of the concatenation of `parts` equals `expected`.
pub fn digest_matches(expected: &[u8; 32], parts: &[&[u8]]) -> bool {
    sha256(parts) == *expected
}

/// One step of the frame parser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Nothing to report.
    Pending,
    /// A header that validates. The caller erases the app pages and replies
    /// ACK. The parser then expects `len` body bytes.
    Header(Header),
    /// A header that failed a check. Reply NAK. The parser has resynchronised.
    BadHeader,
    /// One image byte. `last` marks the final byte of the image.
    Body { byte: u8, last: bool },
    /// Inter-byte timeout inside a frame, in the header or the body. Reply
    /// CAN. The parser has already resynchronised.
    Timeout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Shifting bytes through a 4-byte window, waiting for the magic.
    Hunt { window: u32 },
    /// Collecting the rest of the header. `buf[..n]` holds the bytes so far.
    Header { buf: [u8; HEADER_LEN], n: usize },
    /// Collecting `remaining` image bytes.
    Body { remaining: u32 },
}

/// Frame parser. Feed it one received byte at a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Parser {
    state: State,
}

impl Parser {
    pub const fn new() -> Self {
        Parser { state: State::Hunt { window: 0 } }
    }

    /// True between the magic and the end of the image (or an abort).
    pub const fn in_frame(&self) -> bool {
        !matches!(self.state, State::Hunt { .. })
    }

    /// Feeds one received byte.
    pub fn push(&mut self, byte: u8) -> Step {
        match self.state {
            State::Hunt { window } => {
                let window = (window << 8) | byte as u32;
                self.state = if window == MAGIC_WORD {
                    let mut buf = [0u8; HEADER_LEN];
                    buf[..4].copy_from_slice(&MAGIC);
                    State::Header { buf, n: 4 }
                } else {
                    State::Hunt { window }
                };
                Step::Pending
            }
            State::Header { mut buf, n } => {
                buf[n] = byte;
                if n + 1 < HEADER_LEN {
                    self.state = State::Header { buf, n: n + 1 };
                    return Step::Pending;
                }
                match parse_header(&buf) {
                    Some(h) => {
                        self.state = State::Body { remaining: h.len };
                        Step::Header(h)
                    }
                    None => {
                        self.abort();
                        Step::BadHeader
                    }
                }
            }
            State::Body { remaining } => {
                let last = remaining == 1;
                self.state = if last {
                    State::Hunt { window: 0 }
                } else {
                    State::Body { remaining: remaining - 1 }
                };
                Step::Body { byte, last }
            }
        }
    }

    /// Call when no byte arrived within the inter-byte timeout. Inside a frame
    /// this aborts it and returns [`Step::Timeout`]; in hunt mode it does nothing.
    pub fn timeout(&mut self) -> Step {
        if self.in_frame() {
            self.abort();
            Step::Timeout
        } else {
            Step::Pending
        }
    }

    /// Drops any frame in progress and returns to hunting for the magic.
    pub fn abort(&mut self) {
        self.state = State::Hunt { window: 0 };
    }
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

/// A completed 32-bit word of the image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Word {
    /// Word number from the start of the image (`0` is `APP_BASE`).
    pub index: u32,
    pub value: u32,
}

/// Packs image bytes into little-endian words for NVMC, which writes whole
/// aligned words only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WordStager {
    buf: [u8; 4],
    n: usize,
    index: u32,
}

impl WordStager {
    pub const fn new() -> Self {
        WordStager { buf: [0; 4], n: 0, index: 0 }
    }

    /// Adds one byte. Returns the word once four bytes are gathered.
    pub fn push(&mut self, byte: u8) -> Option<Word> {
        self.buf[self.n] = byte;
        self.n += 1;
        if self.n == 4 {
            self.n = 0;
            Some(self.emit())
        } else {
            None
        }
    }

    /// Returns the partial word, padded with `0xFF` (the erased value), if any
    /// bytes are pending.
    pub fn finish(&mut self) -> Option<Word> {
        if self.n == 0 {
            return None;
        }
        for b in &mut self.buf[self.n..] {
            *b = 0xFF;
        }
        self.n = 0;
        Some(self.emit())
    }

    fn emit(&mut self) -> Word {
        let word = Word { index: self.index, value: u32::from_le_bytes(self.buf) };
        self.index += 1;
        word
    }
}

/// Config record at `CONFIG_BASE` (flash `0x6F000`, read only here; the app
/// and provisioning write it). 12 bytes:
///
/// ```text
/// offset  size  field
/// 0       4     magic  b"MWCF"
/// 4       1     format = 1
/// 5       1     board id (see BOARDS)
/// 6       2     zero
/// 8       4     CRC-32 (IEEE) of bytes 0..8, u32 LE
/// ```
///
/// A blank or invalid record selects the default board, so an erased config
/// region still boots the micro:bit.
pub const CONFIG_BASE: u32 = 0x0006_F000;
/// Config record length.
pub const CONFIG_LEN: usize = 12;
/// Config record magic.
pub const CONFIG_MAGIC: [u8; 4] = *b"MWCF";
/// Config record format.
pub const CONFIG_FORMAT: u8 = 1;

/// A board's pin set. Pins are PSEL numbers: `port * 32 + pin` (P0.14 is
/// 14, P1.08 is 40).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Board {
    /// The id the config record stores.
    pub id: u8,
    /// Name, for documentation.
    pub name: &'static str,
    /// Button A (active low, pull-up): the reflash-mode button.
    pub button: u32,
    /// Console UART transmit pin (VCOM TX).
    pub tx: u32,
    /// Console UART receive pin (VCOM RX).
    pub rx: u32,
}

/// The micro:bit v2 (nRF52833): button A P0.14, VCOM TX P0.06, RX P1.08.
pub const MICROBIT_V2: Board = Board { id: 1, name: "micro:bit v2", button: 14, tx: 6, rx: 40 };

/// Every board this bootloader binary runs on. Adding one is a table entry,
/// not a new binary.
pub const BOARDS: &[Board] = &[MICROBIT_V2];

/// The board used when the config record is blank or invalid.
pub const DEFAULT_BOARD: Board = MICROBIT_V2;

/// Parses a config record. `Some(id)` only if the magic, format, zero bytes
/// and CRC are all valid. Anything else (erased or damaged) is `None`.
pub fn parse_config(b: &[u8; CONFIG_LEN]) -> Option<u8> {
    if b[..4] != CONFIG_MAGIC || b[4] != CONFIG_FORMAT || b[6] != 0 || b[7] != 0 {
        return None;
    }
    let crc = u32::from_le_bytes([b[8], b[9], b[10], b[11]]);
    if crc32(&b[..8]) != crc {
        return None;
    }
    Some(b[5])
}

/// The board for a config record's id, or `None` if this binary has no
/// table entry for it.
pub fn board_for(id: u8) -> Option<Board> {
    BOARDS.iter().copied().find(|b| b.id == id)
}

/// The board to run on given the config record bytes: the id's entry, or
/// [`DEFAULT_BOARD`] if the record is blank, invalid or names an unknown
/// board.
pub fn select_board(config: &[u8; CONFIG_LEN]) -> Board {
    parse_config(config).and_then(board_for).unwrap_or(DEFAULT_BOARD)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SP: u32 = 0x2001_0000;
    const RESET: u32 = 0x0000_4101;

    /// A header with every field valid, the CRC sealed over it.
    fn good_header(len: u32) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[..4].copy_from_slice(&MAGIC);
        h[4] = FORMAT;
        h[5] = FLAGS;
        h[6] = SLOT;
        h[7] = 0;
        h[8..12].copy_from_slice(&7u32.to_le_bytes());
        h[12..16].copy_from_slice(&2u32.to_le_bytes());
        h[16..20].copy_from_slice(&3u32.to_le_bytes());
        h[20..24].copy_from_slice(&len.to_le_bytes());
        h[24..56].copy_from_slice(&[0xAB; 32]);
        seal(&mut h);
        h
    }

    fn seal(h: &mut [u8; HEADER_LEN]) {
        let crc = crc32(&h[..56]);
        h[56..60].copy_from_slice(&crc.to_le_bytes());
    }

    /// Feeds `bytes` and returns every step.
    fn run(p: &mut Parser, bytes: &[u8]) -> std::vec::Vec<Step> {
        bytes.iter().map(|&b| p.push(b)).collect()
    }

    #[test]
    fn vector_table_valid_accepts_typical_app() {
        assert!(vector_table_valid(SP, RESET));
        // Top of RAM is a legal initial SP.
        assert!(vector_table_valid(RAM_END, RESET));
        // Reset vector on the last halfword of the app region.
        assert!(vector_table_valid(SP, (APP_END - 2) | 1));
    }

    #[test]
    fn vector_table_rejects_sp_out_of_ram() {
        assert!(!vector_table_valid(0x0000_0000, RESET));
        assert!(!vector_table_valid(RAM_START - 4, RESET));
        assert!(!vector_table_valid(RAM_END + 4, RESET));
        assert!(!vector_table_valid(0xFFFF_FFFF, RESET));
    }

    #[test]
    fn vector_table_rejects_misaligned_sp() {
        assert!(!vector_table_valid(SP + 1, RESET));
        assert!(!vector_table_valid(SP + 2, RESET));
        assert!(!vector_table_valid(SP + 3, RESET));
    }

    #[test]
    fn vector_table_rejects_reset_outside_app() {
        // Bootloader, payload, trust record, config, and past the end.
        assert!(!vector_table_valid(SP, 0x0000_0001));
        assert!(!vector_table_valid(SP, 0x0007_0001));
        assert!(!vector_table_valid(SP, 0x0006_E001));
        assert!(!vector_table_valid(SP, 0x0006_F001));
        // Below the app base.
        assert!(!vector_table_valid(SP, 0x0000_3001));
    }

    #[test]
    fn vector_table_rejects_clear_thumb_bit() {
        assert!(!vector_table_valid(SP, 0x0000_4100));
        assert!(!vector_table_valid(SP, 0x0000_4000));
    }

    #[test]
    fn erased_vector_table_is_invalid() {
        // An erased page reads as 0xFFFF_FFFF in both words.
        assert!(!vector_table_valid(0xFFFF_FFFF, 0xFFFF_FFFF));
    }

    #[test]
    fn in_app_accepts_app_range() {
        assert!(in_app(APP_BASE, 4));
        assert!(in_app(APP_END - 4, 4));
        assert!(in_app(APP_END, 0));
        assert!(!in_app(APP_END - 3, 4));
    }

    #[test]
    fn in_app_rejects_other_regions() {
        assert!(!in_app(0x0000_0000, 4)); // bootloader
        assert!(!in_app(0x0000_3FFC, 8)); // straddles into the app
        assert!(!in_app(0x0006_E000, 4)); // trust record
        assert!(!in_app(0x0006_EFFC, 4)); // trust record
        assert!(!in_app(0x0006_F000, 4)); // config
        assert!(!in_app(0x0007_0000, 4)); // payload
        assert!(!in_app(0x0007_FFFC, 4)); // payload end
        assert!(!in_app(0x1000_1000, 4)); // UICR
        assert!(!in_app(0x1000_0000, 4)); // FICR
        assert!(!in_app(APP_END - 4, 8)); // runs past the end
        assert!(!in_app(u32::MAX, 4)); // arithmetic must not wrap
    }

    #[test]
    fn crc32_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn crc32_streaming_matches_one_shot() {
        let s = Crc32::new().update(b"1234").update(b"56789").finish();
        assert_eq!(s, 0xCBF4_3926);
    }

    #[test]
    fn page_arithmetic_rounds_up() {
        assert_eq!(pages_for(1), 1);
        assert_eq!(pages_for(PAGE_SIZE - 1), 1);
        assert_eq!(pages_for(PAGE_SIZE), 1);
        assert_eq!(pages_for(PAGE_SIZE + 1), 2);
        assert_eq!(pages_for(3 * PAGE_SIZE + 17), 4);
        assert_eq!(pages_for(APP_MAX_LEN), 106);
    }

    #[test]
    fn erase_end_covers_whole_pages() {
        assert_eq!(erase_end(1), APP_BASE + PAGE_SIZE);
        assert_eq!(erase_end(4097), APP_BASE + 2 * PAGE_SIZE);
        assert_eq!(erase_end(APP_MAX_LEN), APP_END);
        assert_eq!(APP_MAX_LEN % PAGE_SIZE, 0);
    }

    #[test]
    fn header_layout_is_60_bytes() {
        assert_eq!(HEADER_LEN, 60);
        assert_eq!(good_header(1).len(), 60);
    }

    #[test]
    fn valid_header_is_accepted() {
        let h = parse_header(&good_header(0x1234)).expect("valid header");
        assert_eq!(h.len, 0x1234);
        assert_eq!(h.sha256, [0xAB; 32]);
    }

    #[test]
    fn header_length_range_bounds() {
        assert_eq!(parse_header(&good_header(1)).map(|h| h.len), Some(1));
        assert_eq!(
            parse_header(&good_header(APP_MAX_LEN)).map(|h| h.len),
            Some(APP_MAX_LEN)
        );
        assert_eq!(parse_header(&good_header(0)), None);
        assert_eq!(parse_header(&good_header(APP_MAX_LEN + 1)), None);
        assert_eq!(parse_header(&good_header(0x7FFF_FFFF)), None);
        assert_eq!(parse_header(&good_header(u32::MAX)), None);
    }

    #[test]
    fn header_ignores_sequence_min_version_and_version() {
        let mut h = good_header(16);
        h[8..12].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        h[12..16].copy_from_slice(&0u32.to_le_bytes());
        h[16..20].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        seal(&mut h);
        assert!(parse_header(&h).is_some());
    }

    #[test]
    fn header_rejects_bad_magic() {
        let mut h = good_header(16);
        h[0] = b'X';
        seal(&mut h);
        assert_eq!(parse_header(&h), None);
    }

    #[test]
    fn header_rejects_bad_format() {
        for format in [0u8, 2, 0xFF] {
            let mut h = good_header(16);
            h[4] = format;
            seal(&mut h);
            assert_eq!(parse_header(&h), None, "format {format}");
        }
    }

    #[test]
    fn header_rejects_bad_flags() {
        // Only 0x01 is accepted: pending set, rebooted clear.
        for flags in [0x00u8, 0x02, 0x03, 0x05, 0x80, 0xFF] {
            let mut h = good_header(16);
            h[5] = flags;
            seal(&mut h);
            assert_eq!(parse_header(&h), None, "flags {flags:#x}");
        }
    }

    #[test]
    fn header_rejects_bad_slot() {
        for slot in [1u8, 2, 0xFF] {
            let mut h = good_header(16);
            h[6] = slot;
            seal(&mut h);
            assert_eq!(parse_header(&h), None, "slot {slot}");
        }
    }

    #[test]
    fn header_rejects_nonzero_byte_seven() {
        let mut h = good_header(16);
        h[7] = 1;
        seal(&mut h);
        assert_eq!(parse_header(&h), None);
    }

    #[test]
    fn header_rejects_bad_crc() {
        // Flipped CRC bit.
        let mut h = good_header(16);
        h[56] ^= 1;
        assert_eq!(parse_header(&h), None);
        // Field changed after sealing.
        let mut h = good_header(16);
        h[24] ^= 0x80;
        assert_eq!(parse_header(&h), None);
        // Length changed after sealing.
        let mut h = good_header(16);
        h[20] = 17;
        assert_eq!(parse_header(&h), None);
    }

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            sha256(&[b"abc"]),
            [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d,
                0xae, 0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10,
                0xff, 0x61, 0xf2, 0x00, 0x15, 0xad,
            ]
        );
        assert_eq!(
            sha256(&[b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"]),
            [
                0x24, 0x8d, 0x6a, 0x61, 0xd2, 0x06, 0x38, 0xb8, 0xe5, 0xc0, 0x26, 0x93, 0x0c,
                0x3e, 0x60, 0x39, 0xa3, 0x3c, 0xe4, 0x59, 0x64, 0xff, 0x21, 0x67, 0xf6, 0xec,
                0xed, 0xd4, 0x19, 0xdb, 0x06, 0xc1,
            ]
        );
    }

    #[test]
    fn sha256_is_independent_of_split_points() {
        assert_eq!(sha256(&[b"a", b"", b"bc"]), sha256(&[b"abc"]));
    }

    #[test]
    fn digest_matches_checks_expected() {
        let good = sha256(&[b"abc"]);
        assert!(digest_matches(&good, &[b"ab", b"c"]));
        let mut bad = good;
        bad[31] ^= 1;
        assert!(!digest_matches(&bad, &[b"ab", b"c"]));
        assert!(!digest_matches(&good, &[b"abd"]));
    }

    #[test]
    fn parser_accepts_header_then_body() {
        let mut p = Parser::new();
        let mut bytes = std::vec::Vec::from(good_header(3));
        bytes.extend_from_slice(&[1, 2, 3]);
        let steps = run(&mut p, &bytes);
        // Pending through the header, Header on its last byte.
        assert!(steps[..59].iter().all(|s| *s == Step::Pending));
        assert_eq!(steps[59], Step::Header(parse_header(&good_header(3)).unwrap()));
        assert_eq!(steps[60], Step::Body { byte: 1, last: false });
        assert_eq!(steps[61], Step::Body { byte: 2, last: false });
        assert_eq!(steps[62], Step::Body { byte: 3, last: true });
        assert!(!p.in_frame());
    }

    #[test]
    fn parser_emits_no_header_before_60_bytes() {
        // A header with a bad CRC is not reported until its last byte.
        let mut h = good_header(3);
        h[56] ^= 1;
        let mut p = Parser::new();
        let steps = run(&mut p, &h);
        assert!(steps[..59].iter().all(|s| *s == Step::Pending));
        assert_eq!(steps[59], Step::BadHeader);
    }

    #[test]
    fn parser_reports_bad_header_and_resyncs() {
        let mut p = Parser::new();
        let mut bad = good_header(3);
        bad[4] = 9; // format
        seal(&mut bad);
        let steps = run(&mut p, &bad);
        assert_eq!(steps[59], Step::BadHeader);
        assert!(!p.in_frame());
        // A good frame after the rejected one is still parsed.
        let mut frame = std::vec::Vec::from(good_header(1));
        frame.push(0x42);
        let steps = run(&mut p, &frame);
        assert_eq!(steps[59], Step::Header(parse_header(&good_header(1)).unwrap()));
        assert_eq!(steps[60], Step::Body { byte: 0x42, last: true });
    }

    #[test]
    fn parser_rejects_each_invalid_header_field() {
        let mutations: [(&str, fn(&mut [u8; HEADER_LEN])); 5] = [
            ("magic", |h| h[1] = b'Q'),
            ("format", |h| h[4] = 2),
            ("flags", |h| h[5] = 0x03),
            ("slot", |h| h[6] = 1),
            ("zero byte", |h| h[7] = 1),
        ];
        for (name, f) in mutations {
            let mut h = good_header(16);
            f(&mut h);
            seal(&mut h);
            let mut p = Parser::new();
            let steps = run(&mut p, &h);
            assert!(
                !steps.iter().any(|s| matches!(s, Step::Header(_))),
                "{name} accepted"
            );
            if name == "magic" {
                // Not a frame at all: the parser never leaves the hunt.
                assert!(steps.iter().all(|s| *s == Step::Pending));
                assert!(!p.in_frame());
            } else {
                assert_eq!(steps[59], Step::BadHeader, "{name}");
            }
        }
    }

    #[test]
    fn parser_rejects_bad_length_as_bad_header() {
        for len in [0, APP_MAX_LEN + 1, u32::MAX] {
            let mut p = Parser::new();
            let steps = run(&mut p, &good_header(len));
            assert_eq!(steps[59], Step::BadHeader, "len {len:#x}");
        }
    }

    #[test]
    fn parser_accepts_max_length() {
        let mut p = Parser::new();
        let steps = run(&mut p, &good_header(APP_MAX_LEN));
        assert_eq!(
            steps[59],
            Step::Header(Header { len: APP_MAX_LEN, sha256: [0xAB; 32] })
        );
        assert!(p.in_frame());
    }

    #[test]
    fn parser_resyncs_on_garbage() {
        let mut p = Parser::new();
        // Noise, including near-misses of the magic.
        for b in [0x00u8, b'M', b'W', b'M', 0xFF, b'W', b'F', b'X'] {
            assert_eq!(p.push(b), Step::Pending);
        }
        assert!(!p.in_frame());
        // A real frame after the garbage is still parsed.
        let steps = run(&mut p, &good_header(1));
        assert!(matches!(steps[59], Step::Header(h) if h.len == 1));
    }

    #[test]
    fn parser_finds_magic_after_repeated_first_letter() {
        // A stray 'M' before the real magic must not hide it.
        for input in [&b"MMWFW"[..], &b"MWMWFW"[..]] {
            let mut p = Parser::new();
            for &b in input {
                p.push(b);
            }
            assert!(p.in_frame(), "magic not found in {:?}", input);
        }
    }

    #[test]
    fn parser_times_out_mid_frame_only() {
        let mut p = Parser::new();
        assert_eq!(p.timeout(), Step::Pending);
        p.push(b'M');
        p.push(b'W');
        // Start a header: magic then a few more bytes.
        for b in b"MWFW\x10" {
            p.push(*b);
        }
        assert!(p.in_frame());
        assert_eq!(p.timeout(), Step::Timeout);
        assert!(!p.in_frame());
    }

    #[test]
    fn parser_times_out_mid_body() {
        let mut p = Parser::new();
        run(&mut p, &good_header(4));
        p.push(0x11);
        assert!(p.in_frame());
        assert_eq!(p.timeout(), Step::Timeout);
        assert!(!p.in_frame());
    }

    #[test]
    fn parser_abort_returns_to_hunt() {
        let mut p = Parser::new();
        // Magic and part of the header: the parser is mid-header.
        run(&mut p, &good_header(2)[..12]);
        assert!(p.in_frame());
        p.abort();
        assert!(!p.in_frame());
    }

    #[test]
    fn stager_packs_little_endian_words() {
        let mut s = WordStager::new();
        let mut words = std::vec::Vec::new();
        for b in [0x11u8, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88] {
            if let Some(w) = s.push(b) {
                words.push(w);
            }
        }
        assert_eq!(words, [
            Word { index: 0, value: 0x4433_2211 },
            Word { index: 1, value: 0x8877_6655 },
        ]);
        assert_eq!(s.finish(), None);
    }

    #[test]
    fn stager_pads_partial_word_with_erased_value() {
        let mut s = WordStager::new();
        for b in [0xAAu8, 0xBB, 0xCC, 0xDD, 0xEE] {
            s.push(b);
        }
        assert_eq!(s.finish(), Some(Word { index: 1, value: 0xFFFF_FFEE }));
        assert_eq!(s.finish(), None);
    }

    #[test]
    fn stager_pads_one_byte_image() {
        let mut s = WordStager::new();
        s.push(0x7F);
        assert_eq!(s.finish(), Some(Word { index: 0, value: 0xFFFF_FF7F }));
    }

    /// A config record for board `id`, with a valid CRC.
    fn config(id: u8) -> [u8; CONFIG_LEN] {
        let mut b = [0u8; CONFIG_LEN];
        b[..4].copy_from_slice(&CONFIG_MAGIC);
        b[4] = CONFIG_FORMAT;
        b[5] = id;
        let crc = crc32(&b[..8]);
        b[8..12].copy_from_slice(&crc.to_le_bytes());
        b
    }

    #[test]
    fn v9_the_micro_bit_pins_are_unchanged() {
        // The pins the bootloader used before the board table: button A P0.14,
        // VCOM TX P0.06, RX P1.08.
        assert_eq!(MICROBIT_V2.button, 14);
        assert_eq!(MICROBIT_V2.tx, 6);
        assert_eq!(MICROBIT_V2.rx, 40);
    }

    #[test]
    fn v9_a_valid_config_selects_its_board() {
        assert_eq!(parse_config(&config(1)), Some(1));
        assert_eq!(select_board(&config(1)), MICROBIT_V2);
    }

    #[test]
    fn v9_a_blank_or_damaged_config_selects_the_default_board() {
        // Erased flash (0xFF), zeros, and a valid record with one bit flipped.
        assert_eq!(select_board(&[0xFF; CONFIG_LEN]), DEFAULT_BOARD);
        assert_eq!(select_board(&[0; CONFIG_LEN]), DEFAULT_BOARD);
        let mut bad = config(1);
        bad[5] ^= 0x02;
        assert_eq!(parse_config(&bad), None);
        assert_eq!(select_board(&bad), DEFAULT_BOARD);
    }

    #[test]
    fn v9_nonzero_reserved_bytes_or_format_are_rejected() {
        let mut b = config(1);
        b[6] = 1;
        assert_eq!(parse_config(&b), None);
        let mut b = config(1);
        b[4] = 2;
        assert_eq!(parse_config(&b), None);
    }

    #[test]
    fn v9_an_unknown_board_id_selects_the_default_board() {
        // A valid record naming a board this binary has no entry for.
        let b = config(0x42);
        assert_eq!(parse_config(&b), Some(0x42));
        assert_eq!(board_for(0x42), None);
        assert_eq!(select_board(&b), DEFAULT_BOARD);
    }

    #[test]
    fn v9_every_table_entry_has_a_unique_id_and_distinct_pins() {
        for (i, a) in BOARDS.iter().enumerate() {
            for b in &BOARDS[i + 1..] {
                assert_ne!(a.id, b.id, "{} and {}", a.name, b.name);
            }
            assert_ne!(a.tx, a.rx);
            assert_ne!(a.button, a.tx);
            assert_ne!(a.button, a.rx);
        }
        assert!(BOARDS.contains(&DEFAULT_BOARD));
    }
}
