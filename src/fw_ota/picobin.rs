//! RP2350 `PICOBIN` metadata: find an image's IMAGE_DEF and its version.
//!
//! The RP2350 bootrom identifies a bootable image by a *block loop* of
//! metadata blocks (RP2350 datasheet §5.9.1 "Blocks and block loops"; the
//! encodings below are the pico-sdk's `boot/picobin.h`, which the
//! datasheet tabulates in §5.9.3 "Image definition items"):
//!
//! ```text
//! block = 0xffffded3                      start marker
//!         item*                           items, each [type u8][size][data]
//!         0x000000ff | words << 8         LAST item: 2-byte size = words of the items before it
//!         link i32                        byte offset from this block to the next one
//!         0xab123579                      end marker
//! ```
//!
//! An item's first byte is its type; when bit 7 of the type is clear the
//! second byte is the item's size in words (header included), when it is
//! set the size is the next two bytes. The first block of the loop must
//! start in the first 4 KiB of the image ([`SEARCH_WINDOW`]); the loop is
//! closed when following the links returns to the first block (a single
//! block links to itself with offset 0). A block holding an IMAGE_TYPE item
//! (`0x42`, flags in the upper half-word) is an IMAGE_DEF; a VERSION item
//! (`0x48`) carries `major << 16 | minor` in its second word and, when the
//! header's top byte (the number of OTP rows) is non-zero, a rollback
//! version in the low half of its third word.
//!
//! [`find_image_def`] reads an image through a word reader (a slice on the
//! host, flash on the device), scans the search window for the first block
//! that parses, follows the loop for at most [`MAX_BLOCKS`] blocks of at most
//! [`MAX_BLOCK_WORDS`] words each, and returns the **last** IMAGE_DEF in loop
//! order. A loop that does not close, a block that does not parse or a
//! loop that leaves the image is no image at all (`None`). Every loop is
//! bounded by those constants.
//!
//! Assumptions (not settled against silicon; see design.md "A/B firmware
//! OTA (RP2350)"): the bootrom uses the last IMAGE_DEF of a loop, and an
//! IMAGE_DEF is any block containing an IMAGE_TYPE item.
//!
//! [`image_def_block`] builds the smallest IMAGE_DEF block with a VERSION
//! item, the block the RP base firmware places at the start of its image
//! (the same layout as the pico-sdk's `embedded_start_block.inc.S`).

/// Block start marker (`PICOBIN_BLOCK_MARKER_START`).
pub const BLOCK_MARKER_START: u32 = 0xffff_ded3;
/// Block end marker (`PICOBIN_BLOCK_MARKER_END`).
pub const BLOCK_MARKER_END: u32 = 0xab12_3579;
/// IMAGE_TYPE item type (`PICOBIN_BLOCK_ITEM_1BS_IMAGE_TYPE`).
pub const ITEM_IMAGE_TYPE: u8 = 0x42;
/// VERSION item type (`PICOBIN_BLOCK_ITEM_1BS_VERSION`).
pub const ITEM_VERSION: u8 = 0x48;
/// IGNORED item type (`PICOBIN_BLOCK_ITEM_2BS_IGNORED`).
pub const ITEM_IGNORED: u8 = 0xfe;
/// LAST item type (`PICOBIN_BLOCK_ITEM_2BS_LAST`).
pub const ITEM_LAST: u8 = 0xff;
/// The first block must start within this many bytes of the image start.
pub const SEARCH_WINDOW: u32 = 4096;
/// Longest block loop followed.
pub const MAX_BLOCKS: usize = 8;
/// Largest block (in words) parsed.
pub const MAX_BLOCK_WORDS: u32 = 256;

/// IMAGE_TYPE flags: image type field (bits 0..3).
pub const IMAGE_TYPE_MASK: u16 = 0x000f;
/// IMAGE_TYPE flags: an executable image.
pub const IMAGE_TYPE_EXE: u16 = 0x0001;
/// IMAGE_TYPE flags: secure-mode Arm executable (`EXE_SECURITY_S`).
pub const EXE_SECURITY_S: u16 = 0x0020;
/// IMAGE_TYPE flags: CPU field (bits 8..10; 0 Arm, 1 RISC-V).
pub const EXE_CPU_MASK: u16 = 0x0700;
/// IMAGE_TYPE flags: CPU is RISC-V.
pub const EXE_CPU_RISCV: u16 = 0x0100;
/// IMAGE_TYPE flags: chip field (bits 12..14; 0 RP2040, 1 RP2350).
pub const EXE_CHIP_MASK: u16 = 0x7000;
/// IMAGE_TYPE flags: chip is RP2350.
pub const EXE_CHIP_RP2350: u16 = 0x1000;
/// IMAGE_TYPE flags: try before you buy (`PICOBIN_IMAGE_TYPE_EXE_TBYB_BITS`).
pub const EXE_TBYB: u16 = 0x8000;
/// The IMAGE_TYPE flags of a secure Arm RP2350 executable.
pub const RP2350_ARM_S_EXE: u16 = IMAGE_TYPE_EXE | EXE_SECURITY_S | EXE_CHIP_RP2350;

/// An IMAGE_DEF's version item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageVersion {
    /// Major version.
    pub major: u16,
    /// Minor version.
    pub minor: u16,
    /// Rollback version (OTP-backed; secure boot only), when present.
    pub rollback: Option<u16>,
}

impl ImageVersion {
    /// `major << 16 | minor`: the protocol's 32-bit firmware version.
    pub const fn as_u32(&self) -> u32 {
        (self.major as u32) << 16 | self.minor as u32
    }
}

/// What an image's IMAGE_DEF says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageDef {
    /// IMAGE_TYPE flags.
    pub flags: u16,
    /// The VERSION item, if any.
    pub version: Option<ImageVersion>,
    /// Byte offset of the IMAGE_TYPE item header in the image (where the
    /// bootrom's explicit buy clears the TBYB bit).
    pub type_offset: u32,
    /// The highest offset at which a block of the loop starts inside the
    /// first [`SEARCH_WINDOW`] bytes (see [`SENTINEL_FROM`]).
    pub window_tail: u32,
}

impl ImageDef {
    /// An executable.
    pub const fn is_exe(&self) -> bool {
        self.flags & IMAGE_TYPE_MASK == IMAGE_TYPE_EXE
    }

    /// Built for the RP2350.
    pub const fn is_rp2350(&self) -> bool {
        self.flags & EXE_CHIP_MASK == EXE_CHIP_RP2350
    }

    /// Flagged try-before-you-buy.
    pub const fn is_tbyb(&self) -> bool {
        self.flags & EXE_TBYB != 0
    }

    /// The protocol version (`major << 16 | minor`; 0 without a VERSION
    /// item).
    pub const fn version_u32(&self) -> u32 {
        match self.version {
            Some(v) => v.as_u32(),
            None => 0,
        }
    }
}

/// One parsed block.
struct Block {
    image_def: Option<ImageDef>,
    link: i32,
}

/// Reads the word at a byte offset. The parser only asks for word-aligned
/// offsets with `offset + 4 <= len`.
fn word<E>(read: &mut impl FnMut(u32) -> Result<u32, E>, at: u32) -> Result<u32, E> {
    read(at)
}

/// Parses the block whose start marker is at `at`; `None` when it is not a
/// well-formed block inside `len` bytes.
fn parse_block<E>(
    read: &mut impl FnMut(u32) -> Result<u32, E>,
    at: u32,
    len: u32,
) -> Result<Option<Block>, E> {
    let fits = |off: u32| off.checked_add(4).is_some_and(|end| end <= len);
    if !fits(at) || !at.is_multiple_of(4) || word(read, at)? != BLOCK_MARKER_START {
        return Ok(None);
    }
    let first_item = at + 4;
    let mut pos = first_item;
    let mut image_type: Option<(u16, u32)> = None;
    let mut version = None;
    // Bounded: each iteration advances `pos` by at least one word and the
    // block may not exceed MAX_BLOCK_WORDS.
    while pos - at < MAX_BLOCK_WORDS * 4 {
        if !fits(pos) {
            return Ok(None);
        }
        let header = word(read, pos)?;
        let ty = (header & 0xff) as u8;
        let size = if ty & 0x80 == 0 {
            (header >> 8) & 0xff
        } else {
            (header >> 8) & 0xffff
        };
        if ty == ITEM_LAST {
            let items = (pos - first_item) / 4;
            if size != items || !fits(pos.saturating_add(8)) {
                return Ok(None);
            }
            let link = word(read, pos + 4)? as i32;
            if word(read, pos + 8)? != BLOCK_MARKER_END {
                return Ok(None);
            }
            let image_def = image_type.map(|(flags, type_offset)| ImageDef {
                flags,
                version,
                type_offset,
                window_tail: 0,
            });
            return Ok(Some(Block { image_def, link }));
        }
        if size == 0 {
            return Ok(None);
        }
        match ty {
            ITEM_IMAGE_TYPE if size == 1 => image_type = Some(((header >> 16) as u16, pos)),
            ITEM_VERSION if size >= 2 => {
                if !fits(pos.saturating_add(4)) {
                    return Ok(None);
                }
                let major_minor = word(read, pos + 4)?;
                let rollback = if header >> 24 != 0 {
                    if size < 3 || !fits(pos.saturating_add(8)) {
                        return Ok(None);
                    }
                    Some((word(read, pos + 8)? & 0xffff) as u16)
                } else {
                    None
                };
                version = Some(ImageVersion {
                    major: (major_minor >> 16) as u16,
                    minor: (major_minor & 0xffff) as u16,
                    rollback,
                });
            }
            ITEM_IMAGE_TYPE | ITEM_VERSION => return Ok(None),
            _ => {}
        }
        pos = match pos.checked_add(size * 4) {
            Some(p) => p,
            None => return Ok(None),
        };
    }
    Ok(None)
}

/// Finds the IMAGE_DEF of the `len`-byte image read through `read` (see
/// the module docs). `Ok(None)` when the image has no valid block loop or
/// no IMAGE_DEF in it; `Err` only when `read` fails.
pub fn find_image_def<E>(
    mut read: impl FnMut(u32) -> Result<u32, E>,
    len: u32,
) -> Result<Option<ImageDef>, E> {
    let window = len.min(SEARCH_WINDOW) & !3;
    let mut first = None;
    // Bounded by the 4 KiB window: at most 1024 words.
    for at in (0..window).step_by(4) {
        if word(&mut read, at)? == BLOCK_MARKER_START
            && let Some(block) = parse_block(&mut read, at, len)?
        {
            first = Some((at, block));
            break;
        }
    }
    let Some((start, mut block)) = first else {
        return Ok(None);
    };
    let mut at = start;
    let mut tail = start;
    let mut found = None;
    // Bounded by MAX_BLOCKS.
    for _ in 0..MAX_BLOCKS {
        if block.image_def.is_some() {
            found = block.image_def;
        }
        let next = (at as i64) + i64::from(block.link);
        if next == i64::from(start) {
            return Ok(found.map(|d| ImageDef {
                window_tail: tail,
                ..d
            }));
        }
        let Ok(next) = u32::try_from(next) else {
            return Ok(None);
        };
        match parse_block(&mut read, next, len)? {
            Some(b) => {
                at = next;
                if next < SEARCH_WINDOW {
                    tail = tail.max(next);
                }
                block = b;
            }
            None => return Ok(None),
        }
    }
    Ok(None)
}

/// [`find_image_def`] over an image held in memory.
pub fn find_image_def_in(image: &[u8]) -> Option<ImageDef> {
    let len = u32::try_from(image.len()).ok()?;
    let read = |at: u32| -> Result<u32, ()> {
        let i = at as usize;
        let b = image.get(i..i + 4).ok_or(())?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    find_image_def(read, len).ok().flatten()
}

/// Words of [`image_def_block`].
pub const IMAGE_DEF_BLOCK_WORDS: usize = 7;
/// Words of [`sentinel_block`].
pub const SENTINEL_BLOCK_WORDS: usize = 5;

/// Where the *sentinel* block of an updatable image must start: in the
/// last 256-byte flash page of the first sector.
///
/// The bootrom's explicit buy erases the image's first sector and programs
/// it again with the TBYB bit clear. Flash is programmed page by page in
/// ascending order, so a power cut part way leaves the first pages (with an
/// IMAGE_DEF that is no longer TBYB) and an erased tail: if the block loop
/// lay entirely in those first pages the bootrom would boot a torn image.
/// With a block of the loop in the sector's last page the loop only closes
/// once the whole sector is written again, so a torn buy leaves no valid
/// image in that slot and the previous one boots.
pub const SENTINEL_FROM: u32 = SEARCH_WINDOW - 256;

/// A block holding an IMAGE_DEF with `flags` and version `major.minor`
/// (no rollback version), linking `link` bytes on:
/// `START, IMAGE_TYPE, VERSION, major<<16|minor, LAST(3), link, END`.
/// With `link` 0 it is a loop of its own.
pub const fn image_def_block(
    flags: u16,
    major: u16,
    minor: u16,
    link: i32,
) -> [u32; IMAGE_DEF_BLOCK_WORDS] {
    [
        BLOCK_MARKER_START,
        ITEM_IMAGE_TYPE as u32 | 1 << 8 | (flags as u32) << 16,
        ITEM_VERSION as u32 | 2 << 8,
        (major as u32) << 16 | minor as u32,
        ITEM_LAST as u32 | 3 << 8,
        link as u32,
        BLOCK_MARKER_END,
    ]
}

/// A block holding only an ignored item, linking `link` bytes on (back to
/// the IMAGE_DEF block): `START, IGNORED(1), LAST(1), link, END`. Placed at
/// [`SENTINEL_FROM`] or later in the first sector.
pub const fn sentinel_block(link: i32) -> [u32; SENTINEL_BLOCK_WORDS] {
    [
        BLOCK_MARKER_START,
        ITEM_IGNORED as u32 | 1 << 8,
        ITEM_LAST as u32 | 1 << 8,
        link as u32,
        BLOCK_MARKER_END,
    ]
}

const fn le_bytes<const W: usize, const B: usize>(words: [u32; W]) -> [u8; B] {
    let mut out = [0u8; B];
    let mut i = 0;
    while i < W && 4 * i + 3 < B {
        let b = words[i].to_le_bytes();
        out[4 * i] = b[0];
        out[4 * i + 1] = b[1];
        out[4 * i + 2] = b[2];
        out[4 * i + 3] = b[3];
        i += 1;
    }
    out
}

/// [`sentinel_block`] as little-endian bytes.
pub const fn sentinel_block_bytes(link: i32) -> [u8; SENTINEL_BLOCK_WORDS * 4] {
    le_bytes(sentinel_block(link))
}

/// [`image_def_block`] as little-endian bytes.
pub const fn image_def_block_bytes(
    flags: u16,
    major: u16,
    minor: u16,
    link: i32,
) -> [u8; IMAGE_DEF_BLOCK_WORDS * 4] {
    le_bytes(image_def_block(flags, major, minor, link))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(words: &[u32]) -> [u8; 512] {
        let mut out = [0u8; 512];
        for (i, w) in words.iter().enumerate() {
            out[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
        }
        out
    }

    #[test]
    fn y2_block_built_here_parses_with_version_and_tbyb() {
        let flags = RP2350_ARM_S_EXE | EXE_TBYB;
        assert_eq!(flags, 0x9021);
        let mut img = [0u8; 600];
        img[64..64 + 28].copy_from_slice(&image_def_block_bytes(flags, 2, 7, 0));
        let def = find_image_def_in(&img).unwrap();
        assert!(def.is_exe() && def.is_rp2350() && def.is_tbyb());
        assert_eq!(
            def.version,
            Some(ImageVersion {
                major: 2,
                minor: 7,
                rollback: None
            })
        );
        assert_eq!(def.version_u32(), 0x0002_0007);
        assert_eq!((def.type_offset, def.window_tail), (68, 64));
        // Without TBYB.
        img[64..64 + 28].copy_from_slice(&image_def_block_bytes(RP2350_ARM_S_EXE, 1, 0, 0));
        assert!(!find_image_def_in(&img).unwrap().is_tbyb());
    }

    /// The pico-sdk start block layout (`embedded_start_block.inc.S`) with
    /// a two-block loop: the start block links to an end block that holds
    /// only an ignored item, which links back.
    #[test]
    fn y2_two_block_loop_like_the_pico_sdk() {
        let mut img = [0u8; 2048];
        let start = [
            BLOCK_MARKER_START,
            0x42 | 1 << 8 | (0x1021u32 << 16),
            0x48 | 2 << 8,
            3 << 16 | 1,
            0xff | 3 << 8,
            1024, // to the end block
            BLOCK_MARKER_END,
        ];
        let end = [
            BLOCK_MARKER_START,
            0xfe | 1 << 8, // 2BS ignored, size 1
            0xff | 1 << 8,
            (-1024i32) as u32,
            BLOCK_MARKER_END,
        ];
        img[..512].copy_from_slice(&bytes(&start));
        img[1024..1536].copy_from_slice(&bytes(&end));
        let def = find_image_def_in(&img).unwrap();
        assert_eq!(def.version_u32(), 3 << 16 | 1);
        assert!(!def.is_tbyb());
        // The last IMAGE_DEF in the loop wins.
        let end_def = [
            BLOCK_MARKER_START,
            0x42 | 1 << 8 | (0x9021u32 << 16),
            0x48 | 2 << 8,
            4 << 16,
            0xff | 3 << 8,
            (-1024i32) as u32,
            BLOCK_MARKER_END,
        ];
        img[1024..1536].copy_from_slice(&bytes(&end_def));
        let def = find_image_def_in(&img).unwrap();
        assert_eq!(
            (def.version_u32(), def.is_tbyb(), def.type_offset),
            (4 << 16, true, 1028)
        );
    }

    #[test]
    fn y2_rollback_version_is_read() {
        let block = [
            BLOCK_MARKER_START,
            0x42 | 1 << 8 | (0x1021u32 << 16),
            0x48 | 3 << 8 | 1 << 24,
            5 << 16 | 6,
            9 | 77 << 16,
            0xff | 4 << 8,
            0,
            BLOCK_MARKER_END,
        ];
        let v = find_image_def_in(&bytes(&block)).unwrap().version.unwrap();
        assert_eq!(
            v,
            ImageVersion {
                major: 5,
                minor: 6,
                rollback: Some(9)
            }
        );
    }

    /// Hostile metadata never panics and is no image: open loops, loops
    /// that leave the image, blocks without an end marker, wrong LAST
    /// sizes, zero-size items, truncated images, no block at all.
    #[test]
    fn y2_malformed_metadata_is_no_image() {
        let good = image_def_block(0x1021, 1, 0, 0);
        let mut cases: [[u32; 7]; 6] = [good; 6];
        cases[0][5] = 4096; // links out of the image
        cases[1][6] = 0; // no end marker
        cases[2][4] = 0xff | 2 << 8; // LAST size wrong
        cases[3][1] = 0x42; // zero-size item
        cases[4][5] = 8; // links into the middle of itself
        cases[5][2] = 0x48 | 1 << 8; // version item too short
        for (i, c) in cases.iter().enumerate() {
            assert_eq!(find_image_def_in(&bytes(c)), None, "case {i}");
        }
        assert_eq!(find_image_def_in(&bytes(&good)[..20]), None, "truncated");
        assert_eq!(find_image_def_in(&[0xffu8; 8192]), None, "erased flash");
        assert_eq!(find_image_def_in(&[]), None);
        // A block past the 4 KiB window is not found.
        let mut img = [0u8; 8192];
        img[4096..4124].copy_from_slice(&image_def_block_bytes(0x1021, 1, 0, 0));
        assert_eq!(find_image_def_in(&img), None);
        // A loop with more than MAX_BLOCKS blocks is refused: each block
        // links to the next 64 bytes on, the last back to the first.
        let mut img = [0u8; 4096];
        let n = MAX_BLOCKS + 1;
        for b in 0..n {
            let link = if b + 1 == n { -((64 * b) as i32) } else { 64 };
            let words = [BLOCK_MARKER_START, 0xff, link as u32, BLOCK_MARKER_END];
            img[64 * b..64 * b + 16].copy_from_slice(&bytes(&words)[..16]);
        }
        assert_eq!(find_image_def_in(&img), None);
        // The same loop with MAX_BLOCKS blocks closes (and has no IMAGE_DEF).
        let mut img = [0u8; 4096];
        for b in 0..MAX_BLOCKS {
            let link = if b + 1 == MAX_BLOCKS {
                -((64 * b) as i32)
            } else {
                64
            };
            let words = [BLOCK_MARKER_START, 0xff, link as u32, BLOCK_MARKER_END];
            img[64 * b..64 * b + 16].copy_from_slice(&bytes(&words)[..16]);
        }
        assert_eq!(find_image_def_in(&img), None);
    }

    /// The sentinel block in the first sector's last page is found and
    /// reported as the loop's window tail.
    #[test]
    fn y2_sentinel_block_closes_the_loop() {
        let mut img = [0u8; 8192];
        let at = 0x40usize;
        let sentinel = SENTINEL_FROM as usize + 0x40;
        let link = (sentinel - at) as i32;
        img[at..at + 28].copy_from_slice(&image_def_block_bytes(
            RP2350_ARM_S_EXE | EXE_TBYB,
            1,
            2,
            link,
        ));
        img[sentinel..sentinel + 20].copy_from_slice(&sentinel_block_bytes(-link));
        let def = find_image_def_in(&img).unwrap();
        assert_eq!(
            (def.version_u32(), def.window_tail),
            (0x0001_0002, sentinel as u32)
        );
        assert!(def.window_tail >= SENTINEL_FROM);
        // A torn rewrite of the first sector (its tail erased): no image.
        img[2048..4096].fill(0xff);
        assert_eq!(find_image_def_in(&img), None);
    }

    /// Pseudo-random metadata (start markers and item headers sprinkled
    /// over random words) never panics.
    #[test]
    fn y2_random_metadata_never_panics() {
        let mut x = 0x2545_f491_u32;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        for _ in 0..400 {
            let mut img = [0u8; 2048];
            for w in img.chunks_mut(4) {
                let r = next();
                let v = match r % 8 {
                    0 => BLOCK_MARKER_START,
                    1 => BLOCK_MARKER_END,
                    2 => 0x42 | (r & 0xff00) | 0x9021 << 16,
                    3 => 0x48 | (r & 0xff00),
                    4 => 0xff | (r & 0x0f00),
                    5 => (r % 512).wrapping_sub(256),
                    _ => next(),
                };
                w.copy_from_slice(&v.to_le_bytes());
            }
            let _ = find_image_def_in(&img);
            let _ = find_image_def_in(&img[..(next() % 2048) as usize]);
        }
    }

    /// A read error is reported, not swallowed.
    #[test]
    fn y2_read_errors_propagate() {
        assert_eq!(find_image_def(|_| Err::<u32, u8>(7), 64), Err(7));
    }
}
