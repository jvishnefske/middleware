//! The incremental decoder: [`Decoder`], its [`Step`]s and [`Strip`]s.
//!
//! [`Decoder::new`] (or [`Decoder::start`] in place) parses and validates
//! the markers; each decode call then does a bounded amount of the current
//! MCU row and reports [`Step::Row`] when the row is complete in the
//! caller's strip buffer:
//!
//! * [`Decoder::decode_blocks`] decodes at most `n` 8×8 blocks (Huffman
//!   decoding, dequantisation and IDCT into the decoder's MCU buffer); the
//!   call that finishes an MCU's last block also colour-converts the MCU
//!   into the strip. This is the finest step.
//! * [`Decoder::decode_mcus`] decodes at most `n` MCUs.
//! * [`Decoder::decode_mcu_row`] finishes the row.
//!
//! The decoder holds the tables, the bit position, the DC predictors, an
//! MCU cursor and the sample blocks of one MCU (384 bytes); the JPEG bytes
//! are passed to every call and the pixels go to the caller's buffer, so
//! neither lives in the decoder. A call uses one coefficient block (256
//! bytes) and the IDCT workspace (256 bytes) of stack.
//!
//! Colour conversion writes one MCU at a time: interior MCUs take a fast
//! path per layout ([`Layout::YCbCr420`] computes each chroma pair's RGB
//! offsets once for its two pixels), MCUs cut by the right image edge a
//! generic per-pixel path. Chroma is replicated, not interpolated.
//!
//! Once a decode call fails, the decoder stays failed: later calls return
//! the same error (except [`Error::InputChanged`] and
//! [`Error::BufferTooSmall`], which are checked before any work and leave
//! the state untouched). After the last row, and in [`Decoder::IDLE`],
//! calls return [`Step::Done`] whatever the input.

use crate::bits::BitReader;
use crate::color::{PixelFormat, apply, chroma, ycc_to_rgb};
use crate::error::{Error, Malformed, Unsupported};
use crate::header::{Component, Header, Layout, Tables, parse};
use crate::huffman::HuffTable;
use crate::idct::{Block, Extent, dc_only, idct_4x4, idct_8x8};

/// Natural (row-major) index of each zigzag position.
const ZIGZAG: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Transposed index (`u * 8 + v` for natural `v * 8 + u`) of each zigzag
/// position: where [`idct_8x8`] wants the coefficient. (Indexing in a
/// constant initialiser is checked at compile time.)
#[allow(clippy::indexing_slicing)]
const ZIGZAG_T: [u8; 64] = {
    let mut t = [0u8; 64];
    let mut k = 0;
    while k < 64 {
        let z = ZIGZAG[k];
        t[k] = (z % 8) * 8 + z / 8;
        k += 1;
    }
    t
};

/// Output resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scale {
    /// One output pixel per image pixel.
    #[default]
    Full,
    /// One output pixel per 2×2 image pixels, size rounded up: a 240×240
    /// frame becomes 120×120. Needs the `half` feature (on by default);
    /// without it [`Decoder::start`] returns [`Unsupported::Scale`] and the
    /// reduced IDCT is not compiled in (about 1 KB of WASM code). Each
    /// block goes through libjpeg's reduced 4×4 IDCT (`jpeg_idct_4x4`,
    /// equal to a 2×2 box average of the full IDCT for the low
    /// frequencies), so it costs about two thirds of
    /// [`Scale::Full`] per MCU and a quarter of the strip and SPI traffic.
    Half,
}

/// Whether [`Scale::Half`] is built in (feature `half`, on by default).
const HALF: bool = cfg!(feature = "half");

/// A completed band of output rows in the strip buffer.
///
/// The pixels are at the start of the buffer, row-major with a stride of
/// `width × F::BYTES` bytes and no padding: send the first
/// `width × height × F::BYTES` bytes to a panel window of
/// `width × height` at row `y`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Strip {
    /// First output row.
    pub y: u16,
    /// Rows in this strip (the strip height, less for the last row of MCUs).
    pub height: u16,
    /// Output width in pixels.
    pub width: u16,
}

impl Strip {
    /// Bytes of pixel data for a format of `bytes_per_pixel`.
    pub const fn bytes(&self, bytes_per_pixel: usize) -> usize {
        self.width as usize * self.height as usize * bytes_per_pixel
    }
}

/// What a decode call achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The MCU budget ran out inside a row; call again with the same buffer.
    Partial,
    /// A row of MCUs is complete and its pixels are in the buffer.
    Row(Strip),
    /// Every row has been returned.
    Done,
}

/// Decoding progress. The explicit `u8` tag makes `Done` zero (without it
/// the compiler packs the tag into a niche of [`Error`], and `Done` becomes
/// non-zero), so that [`Decoder::IDLE`] is all zero bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Phase {
    /// The last row was returned, or no image was started.
    Done = 0,
    /// More MCUs to decode.
    Running = 1,
    /// A decode call failed with this error.
    Failed(Error) = 2,
}

/// Position in the MCU grid and in the restart sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cursor {
    /// MCU column of the next MCU.
    x: u16,
    /// MCU row of the next MCU.
    y: u16,
    /// MCUs left in the current restart interval (unused without DRI).
    restart_left: u16,
    /// Number (0..=7) of the next expected RSTn marker.
    next_rst: u8,
}

impl Cursor {
    /// The cursor at the first MCU.
    const fn start(restart_interval: u16) -> Cursor {
        Cursor {
            x: 0,
            y: 0,
            restart_left: restart_interval,
            next_rst: 0,
        }
    }

    /// The cursor after one MCU of a row of `per_row` MCUs.
    const fn advanced(self, per_row: u16) -> Cursor {
        let x = self.x.saturating_add(1);
        let (x, y) = if x >= per_row {
            (0, self.y.saturating_add(1))
        } else {
            (x, self.y)
        };
        Cursor {
            x,
            y,
            restart_left: self.restart_left.saturating_sub(1),
            ..self
        }
    }

    /// The cursor after a restart marker of an interval of `interval` MCUs.
    const fn restarted(self, interval: u16) -> Cursor {
        Cursor {
            restart_left: interval,
            next_rst: (self.next_rst + 1) & 7,
            ..self
        }
    }
}

/// Where the entropy decoder is: the bit position and the DC predictor of
/// each component (both reset at a restart marker).
#[derive(Debug, Clone, Copy)]
struct EntropyState {
    /// Entropy-decoder position.
    bits: BitReader,
    /// DC predictor per component.
    preds: [i32; 3],
}

/// Sample blocks of one MCU (rows of 8), in MCU block order.
type McuBlocks = [[[u8; 8]; 8]; 6];

/// An incremental baseline JPEG decoder; see the [crate docs](crate).
#[derive(Debug, Clone)]
pub struct Decoder {
    /// Frame geometry.
    header: Header,
    /// Output resolution.
    scale: Scale,
    /// Quantisation and Huffman tables.
    tables: Tables,
    /// Table selectors per component.
    comps: [Component; 3],
    /// Length of the JPEG slice (checked on every call).
    input_len: usize,
    /// Entropy-decoder position and DC predictors.
    entropy: EntropyState,
    /// Next MCU.
    cursor: Cursor,
    /// Sample blocks of the MCU in progress.
    mcu: McuBlocks,
    /// Next block (in MCU order) of the MCU in progress.
    block: u8,
    /// Progress.
    phase: Phase,
}

impl Decoder {
    /// A decoder with no image: decode calls return [`Step::Done`]. Its
    /// bytes are all zero, so `static DEC: GuestCell<Decoder> =
    /// GuestCell::new(Decoder::IDLE)` lands in `.bss` and costs nothing in
    /// the `.wasm` file; [`start`](Self::start) then fills it in place,
    /// without a 4 KB temporary on the 16 KiB payload stack.
    pub const IDLE: Decoder = Decoder {
        header: Header {
            width: 0,
            height: 0,
            layout: Layout::Gray,
            restart_interval: 0,
        },
        scale: Scale::Full,
        tables: Tables::EMPTY,
        comps: [Component {
            quant: 0,
            dc: 0,
            ac: 0,
        }; 3],
        input_len: 0,
        entropy: EntropyState {
            bits: BitReader::new(0),
            preds: [0; 3],
        },
        cursor: Cursor::start(0),
        mcu: [[[0; 8]; 8]; 6],
        block: 0,
        phase: Phase::Done,
    };

    /// Parses and validates the markers of `jpeg` (up to SOS) for
    /// full-scale output.
    pub fn new(jpeg: &[u8]) -> Result<Decoder, Error> {
        Decoder::new_scaled(jpeg, Scale::Full)
    }

    /// Like [`new`](Self::new) with an output [`Scale`].
    pub fn new_scaled(jpeg: &[u8], scale: Scale) -> Result<Decoder, Error> {
        let mut dec = Decoder::IDLE;
        dec.start(jpeg, scale)?;
        Ok(dec)
    }

    /// Restarts this decoder on a new image in place (what
    /// [`new_scaled`](Self::new_scaled) does on a fresh one). On error the
    /// decoder is left [`IDLE`](Self::IDLE)-like: decode calls return
    /// [`Step::Done`].
    #[inline(never)]
    pub fn start(&mut self, jpeg: &[u8], scale: Scale) -> Result<(), Error> {
        self.phase = Phase::Done;
        if scale == Scale::Half && !HALF {
            return Err(Error::Unsupported(Unsupported::Scale));
        }
        self.tables = Tables::EMPTY;
        let parsed = parse(jpeg, &mut self.tables)?;
        self.header = parsed.header;
        self.scale = scale;
        self.comps = parsed.comps;
        self.input_len = jpeg.len();
        self.entropy = EntropyState {
            bits: BitReader::new(parsed.entropy),
            preds: [0; 3],
        };
        self.cursor = Cursor::start(parsed.header.restart_interval);
        self.block = 0;
        self.phase = Phase::Running;
        Ok(())
    }

    /// The frame header.
    pub const fn header(&self) -> &Header {
        &self.header
    }

    /// Output width: the image width, halved (rounded up) for
    /// [`Scale::Half`].
    pub const fn output_width(&self) -> u16 {
        self.scaled(self.header.width)
    }

    /// Output height, like [`output_width`](Self::output_width).
    pub const fn output_height(&self) -> u16 {
        self.scaled(self.header.height)
    }

    /// Output rows per full strip: the MCU height (8 or 16), halved for
    /// [`Scale::Half`].
    pub const fn strip_height(&self) -> u16 {
        self.scaled(self.header.layout.mcu_height())
    }

    /// Bytes the strip buffer needs for pixel format `F`:
    /// `output_width × strip_height × F::BYTES` (7 680 for 240-wide 4:2:0
    /// RGB565).
    pub fn strip_bytes<F: PixelFormat>(&self) -> usize {
        usize::from(self.output_width()) * usize::from(self.strip_height()) * F::BYTES
    }

    /// MCUs per MCU row.
    pub const fn mcus_per_row(&self) -> u16 {
        self.header.width.div_ceil(self.header.layout.mcu_width())
    }

    /// MCU rows in the image.
    pub const fn mcu_rows(&self) -> u16 {
        self.header.height.div_ceil(self.header.layout.mcu_height())
    }

    /// The output scale: always [`Scale::Full`] without the `half`
    /// feature, so the compiler drops the reduced IDCT and its paths
    /// ([`Decoder::start`] refuses [`Scale::Half`] then).
    const fn out_scale(&self) -> Scale {
        if HALF { self.scale } else { Scale::Full }
    }

    /// `v` at the output scale.
    const fn scaled(&self, v: u16) -> u16 {
        match self.out_scale() {
            Scale::Full => v,
            Scale::Half => v.div_ceil(2),
        }
    }

    /// Decodes the rest of the current MCU row into `out` (at least
    /// [`strip_bytes`](Self::strip_bytes) long): returns [`Step::Row`], or
    /// [`Step::Done`] once every row has been returned.
    pub fn decode_mcu_row<F: PixelFormat>(
        &mut self,
        jpeg: &[u8],
        out: &mut [u8],
    ) -> Result<Step, Error> {
        self.run::<F>(jpeg, out, u32::MAX)
    }

    /// Decodes at most `max_mcus` MCUs of the current MCU row into `out`
    /// (never crossing into the next row), as pixel format `F`. An MCU left
    /// half-done by [`decode_blocks`](Self::decode_blocks) counts as one.
    ///
    /// `jpeg` must be the slice the decoder was created from, and `out` the
    /// same buffer (unchanged) for every call of one row. Returns
    /// [`Step::Partial`] if the row is not finished (including for
    /// `max_mcus == 0`), [`Step::Row`] when it is, and [`Step::Done`] after
    /// the last row.
    pub fn decode_mcus<F: PixelFormat>(
        &mut self,
        jpeg: &[u8],
        out: &mut [u8],
        max_mcus: u16,
    ) -> Result<Step, Error> {
        let per_mcu = self.header.layout.blocks() as u32;
        let blocks = match max_mcus {
            0 => 0,
            n => (per_mcu - u32::from(self.block)) + u32::from(n - 1) * per_mcu,
        };
        self.run::<F>(jpeg, out, blocks)
    }

    /// The finest step: decodes at most `max_blocks` 8×8 blocks (entropy
    /// decoding and IDCT into the decoder's MCU buffer); when the last block
    /// of an MCU is done, the same call colour-converts the MCU into `out`.
    /// Otherwise like [`decode_mcus`](Self::decode_mcus).
    ///
    /// A 4:2:0 MCU is six blocks (4:2:2 four, 4:4:4 three, grayscale one),
    /// so `decode_blocks(.., 1)` bounds a call to one block plus at most
    /// one MCU conversion, for hosts whose fuel budget does not fit a whole
    /// MCU (see the crate docs for costs).
    pub fn decode_blocks<F: PixelFormat>(
        &mut self,
        jpeg: &[u8],
        out: &mut [u8],
        max_blocks: u16,
    ) -> Result<Step, Error> {
        self.run::<F>(jpeg, out, u32::from(max_blocks))
    }

    /// The common decode loop: at most `max_blocks` blocks, stopping at the
    /// end of the MCU row. Fails the decoder on a decoding error. Out of
    /// line so that a caller's function stays small: wasmi translates (and
    /// charges for) a function when it is first called.
    #[inline(never)]
    fn run<F: PixelFormat>(
        &mut self,
        jpeg: &[u8],
        out: &mut [u8],
        max_blocks: u32,
    ) -> Result<Step, Error> {
        match self.phase {
            Phase::Running => {}
            Phase::Done => return Ok(Step::Done),
            Phase::Failed(e) => return Err(e),
        }
        if jpeg.len() != self.input_len {
            return Err(Error::InputChanged);
        }
        let needed = self.strip_bytes::<F>();
        if out.len() < needed {
            return Err(Error::BufferTooSmall { needed });
        }
        let per_row = u32::from(self.mcus_per_row());
        let per_mcu = self.header.layout.blocks() as u32;
        // A row is at most `per_row * per_mcu` blocks.
        for _ in 0..max_blocks.min(per_row * per_mcu) {
            match self.step::<F>(jpeg, out) {
                Ok(None) => {}
                Ok(Some(step)) => return Ok(step),
                Err(e) => {
                    self.phase = Phase::Failed(e);
                    return Err(e);
                }
            }
        }
        Ok(Step::Partial)
    }

    /// Decodes the next block; after the last block of an MCU, converts the
    /// MCU and advances the cursor. Returns the [`Step`] that ends a row.
    fn step<F: PixelFormat>(&mut self, jpeg: &[u8], out: &mut [u8]) -> Result<Option<Step>, Error> {
        let layout = self.header.layout;
        let interval = self.header.restart_interval;
        if self.block == 0 && interval != 0 && self.cursor.restart_left == 0 {
            self.entropy.bits.restart(jpeg, self.cursor.next_rst)?;
            self.entropy.preds = [0; 3];
            self.cursor = self.cursor.restarted(interval);
        }
        let b = usize::from(self.block);
        let comp = layout.component_of_block(b);
        let scale = self.out_scale();
        let samples = self.mcu.get_mut(b).ok_or(Malformed::Coefficient)?;
        decode_block(
            &self.tables,
            &self.comps,
            &mut self.entropy,
            jpeg,
            comp,
            scale,
            samples,
        )?;
        if b + 1 < layout.blocks() {
            self.block += 1;
            return Ok(None);
        }
        self.block = 0;
        self.emit::<F>(&self.mcu, out);
        let y = self.cursor.y;
        self.cursor = self.cursor.advanced(self.mcus_per_row());
        if self.cursor.y == y {
            return Ok(None);
        }
        if self.cursor.y >= self.mcu_rows() {
            self.phase = Phase::Done;
        }
        Ok(Some(Step::Row(self.strip(y))))
    }

    /// The strip of MCU row `mcu_y`.
    fn strip(&self, mcu_y: u16) -> Strip {
        let full = self.strip_height();
        let y = mcu_y.saturating_mul(full);
        Strip {
            y,
            height: full.min(self.output_height().saturating_sub(y)),
            width: self.output_width(),
        }
    }

    /// Writes the MCU's samples into the strip. Each block holds `n × n`
    /// samples (8, or 4 for [`Scale::Half`]). Interior MCUs take a fast path
    /// per layout; MCUs cut by the right image edge take the generic
    /// per-pixel path.
    #[inline(never)]
    fn emit<F: PixelFormat>(&self, blocks: &McuBlocks, out: &mut [u8]) {
        let layout = self.header.layout;
        let sh = match self.out_scale() {
            Scale::Full => 3,
            Scale::Half => 2,
        };
        let (mw, mh) = (
            layout.mcu_width() >> (3 - sh),
            layout.mcu_height() >> (3 - sh),
        );
        let x0 = self.cursor.x.saturating_mul(mw);
        let y0 = self.cursor.y.saturating_mul(mh);
        let cols = usize::from(mw.min(self.output_width().saturating_sub(x0)));
        let rows = usize::from(mh.min(self.output_height().saturating_sub(y0)));
        let area = Area {
            start: usize::from(x0) * F::BYTES,
            stride: usize::from(self.output_width()) * F::BYTES,
            cols,
            rows,
            sh,
        };
        if cols < usize::from(mw) {
            emit_cropped::<F>(blocks, layout, out, area);
            return;
        }
        match layout {
            Layout::Gray => emit_gray::<F>(blocks, out, area),
            Layout::YCbCr444 => emit_444::<F>(blocks, out, area),
            Layout::YCbCr422 => emit_h2::<F, 0>(blocks, out, area),
            Layout::YCbCr420 => emit_h2::<F, 1>(blocks, out, area),
        }
    }
}

/// Decodes one block of component `comp` and inverse-transforms it into
/// `samples` (8×8, or the top-left 4×4 for [`Scale::Half`]), or fills it
/// for a block with only a DC term.
#[inline(never)]
fn decode_block(
    tables: &Tables,
    comps: &[Component; 3],
    state: &mut EntropyState,
    jpeg: &[u8],
    comp: usize,
    scale: Scale,
    samples: &mut [[u8; 8]; 8],
) -> Result<(), Error> {
    let c = comps.get(comp).copied().unwrap_or_default();
    let (Some(dc), Some(ac), Some(q), Some(pred)) = (
        tables.dc.get(usize::from(c.dc)),
        tables.ac.get(usize::from(c.ac)),
        tables.quant.get(usize::from(c.quant)),
        state.preds.get_mut(comp),
    ) else {
        return Err(Malformed::MissingTable.into());
    };
    // Work on a local copy so the reader's fields stay in wasm locals;
    // stored back on success (on error the decoder is failed anyway).
    let mut br = state.bits;
    let mut coef: Block = [[0; 8]; 8];
    let flags = entropy(dc, ac, q, &mut br, jpeg, pred, &mut coef)?;
    if br.overrun() {
        return Err(Error::Truncated);
    }
    state.bits = br;
    if flags & AC == 0 {
        *samples = [[dc_only(coef[0][0]); 8]; 8];
    } else if HALF && scale == Scale::Half {
        idct_4x4(&coef, samples);
    } else {
        let extent = Extent {
            hi_u: flags & HI_U != 0,
            any_u: flags & ANY_U != 0,
        };
        idct_8x8(&coef, extent, samples);
    }
    Ok(())
}

/// [`entropy`] flag: some AC coefficient is non-zero.
const AC: u8 = 1;
/// [`entropy`] flag: some coefficient has horizontal frequency `u >= 4`.
const HI_U: u8 = 2;
/// [`entropy`] flag: some coefficient has horizontal frequency `u >= 1`.
const ANY_U: u8 = 4;

/// The [`entropy`] flags of each zigzag position. (Indexing in a constant
/// initialiser is checked at compile time.)
#[allow(clippy::indexing_slicing)]
const ZIGZAG_FLAGS: [u8; 64] = {
    let mut t = [0u8; 64];
    let mut k = 1;
    while k < 64 {
        let z = ZIGZAG[k];
        let u = z % 8;
        t[k] = AC | if u >= 4 { HI_U } else { 0 } | if u >= 1 { ANY_U } else { 0 };
        k += 1;
    }
    t
};

/// Decodes the Huffman-coded coefficients of one block, dequantised, into
/// `coef` (transposed, see [`idct_8x8`]), updating the DC predictor.
/// Returns the [`AC`], [`HI_U`] and [`ANY_U`] flags of the non-zero AC
/// coefficients.
///
/// Every iteration of the AC loop decodes one symbol, so it runs at most 63
/// times. Errors: [`Malformed::HuffmanCode`], and
/// [`Malformed::Coefficient`] for a magnitude category above 11 (DC) or 10
/// (AC) or a run past the end of the block.
#[inline(always)]
fn entropy(
    dc: &HuffTable,
    ac: &HuffTable,
    q: &[u8; 64],
    br: &mut BitReader,
    data: &[u8],
    pred: &mut i32,
    coef: &mut Block,
) -> Result<u8, Malformed> {
    let flat = coef.as_flattened_mut();
    let t = u32::from(dc.decode(br, data)?);
    if t > 11 {
        return Err(Malformed::Coefficient);
    }
    let diff = if t == 0 {
        0
    } else {
        extend(br.bits(data, t), t)
    };
    *pred = pred.wrapping_add(diff).clamp(-32_768, 32_767);
    if let Some(c0) = flat.first_mut() {
        *c0 = *pred * i32::from(q[0]);
    }
    let mut flags = 0u8;
    let mut k = 1usize;
    for _ in 0..63 {
        let rs = ac.decode(br, data)?;
        let size = u32::from(rs & 15);
        k += usize::from(rs >> 4);
        if size == 0 {
            if rs != 0xF0 {
                break; // EOB
            }
            k += 1; // ZRL: sixteen zeros
        } else {
            let (Some(&pos), Some(&qk), Some(&f)) =
                (ZIGZAG_T.get(k), q.get(k), ZIGZAG_FLAGS.get(k))
            else {
                return Err(Malformed::Coefficient);
            };
            if size > 10 {
                return Err(Malformed::Coefficient);
            }
            let v = extend(br.bits(data, size), size);
            if let Some(dst) = flat.get_mut(usize::from(pos)) {
                *dst = v * i32::from(qk);
            }
            flags |= f;
            k += 1;
        }
        if k >= 64 {
            break;
        }
    }
    Ok(flags)
}

/// Where an MCU goes in the strip, in bytes and pixels.
#[derive(Debug, Clone, Copy)]
struct Area {
    /// Byte offset of the MCU's first pixel.
    start: usize,
    /// Bytes per strip line.
    stride: usize,
    /// Pixels per line to write.
    cols: usize,
    /// Lines to write.
    rows: usize,
    /// log2 of the samples per block side: 3 (8×8) or 2 (4×4, half scale).
    sh: u32,
}

impl Area {
    /// The bytes of line `r`, or `None` past the buffer (never, as the
    /// strip length is checked per call).
    #[inline(always)]
    fn line<'a, F: PixelFormat>(&self, out: &'a mut [u8], r: usize) -> Option<&'a mut [u8]> {
        let s = self.start + r * self.stride;
        out.get_mut(s..s + self.cols * F::BYTES)
    }

    /// Samples per block side.
    #[inline(always)]
    fn n(&self) -> usize {
        1 << self.sh
    }
}

/// A row of zeros, for out-of-range block lookups (never taken).
const ZERO_ROW: [u8; 8] = [0; 8];

/// Row `r` (masked to 0..8) of sample block `b`.
#[inline(always)]
fn row(blocks: &McuBlocks, b: usize, r: usize) -> &[u8; 8] {
    blocks
        .get(b)
        .and_then(|blk| blk.get(r & 7))
        .unwrap_or(&ZERO_ROW)
}

/// Sample `i` (masked to 0..8) of a block row.
#[inline(always)]
fn at(row: &[u8; 8], i: usize) -> u8 {
    row.get(i & 7).copied().unwrap_or(0)
}

/// Pixel `i` of `line` (`F::BYTES` bytes), or an empty slice.
#[inline(always)]
fn pixel<F: PixelFormat>(line: &mut [u8], i: usize) -> &mut [u8] {
    let o = i * F::BYTES;
    line.get_mut(o..o + F::BYTES).unwrap_or(&mut [])
}

/// Full-width grayscale MCU: one block.
#[inline(never)]
fn emit_gray<F: PixelFormat>(blocks: &McuBlocks, out: &mut [u8], area: Area) {
    let n = area.n();
    for r in 0..area.rows.min(n) {
        let Some(line) = area.line::<F>(out, r) else {
            return;
        };
        let ys = row(blocks, 0, r);
        for i in 0..n {
            let y = at(ys, i);
            F::put(pixel::<F>(line, i), [y; 3]);
        }
    }
}

/// Full-width 4:4:4 MCU: blocks Y Cb Cr.
#[inline(never)]
fn emit_444<F: PixelFormat>(blocks: &McuBlocks, out: &mut [u8], area: Area) {
    let n = area.n();
    for r in 0..area.rows.min(n) {
        let Some(line) = area.line::<F>(out, r) else {
            return;
        };
        let (ys, cb, cr) = (row(blocks, 0, r), row(blocks, 1, r), row(blocks, 2, r));
        for i in 0..n {
            let d = chroma(at(cb, i), at(cr, i));
            F::put(pixel::<F>(line, i), apply(at(ys, i), d));
        }
    }
}

/// Full-width 4:2:2 (`V = 0`, blocks Y Y Cb Cr) or 4:2:0 (`V = 1`, blocks
/// Y Y Y Y Cb Cr) MCU. Each chroma sample covers two horizontal
/// neighbours (and, for 4:2:0, two lines): its RGB offsets are computed
/// once per pair. Chroma is replicated, not interpolated.
#[inline(never)]
fn emit_h2<F: PixelFormat, const V: usize>(blocks: &McuBlocks, out: &mut [u8], area: Area) {
    let (n, sh) = (area.n(), area.sh);
    let l = 2 << V; // luma blocks
    for r in 0..area.rows.min(n << V) {
        let Some(line) = area.line::<F>(out, r) else {
            return;
        };
        let (yb, ry) = ((r >> sh) * 2, r & (n - 1));
        let (cb, cr) = (row(blocks, l, r >> V), row(blocks, l + 1, r >> V));
        for half in 0..2 {
            let ys = row(blocks, yb + half, ry);
            for j in 0..n / 2 {
                let c = half * (n / 2) + j;
                let d = chroma(at(cb, c), at(cr, c));
                let o = 2 * c * F::BYTES;
                let Some((p0, p1)) = line
                    .get_mut(o..o + 2 * F::BYTES)
                    .and_then(|pair| pair.split_at_mut_checked(F::BYTES))
                else {
                    return;
                };
                F::put(p0, apply(at(ys, 2 * j), d));
                F::put(p1, apply(at(ys, 2 * j + 1), d));
            }
        }
    }
}

/// An MCU cut by the right edge of the image, any layout: per pixel.
#[inline(never)]
fn emit_cropped<F: PixelFormat>(blocks: &McuBlocks, layout: Layout, out: &mut [u8], area: Area) {
    for r in 0..area.rows.min(16) {
        let Some(line) = area.line::<F>(out, r) else {
            return;
        };
        for c in 0..area.cols.min(16) {
            let [y, cb, cr] = ycc_at(blocks, layout, area.sh, c, r);
            F::put(pixel::<F>(line, c), to_rgb(layout, y, cb, cr));
        }
    }
}

/// RGB of one sample.
#[inline(always)]
fn to_rgb(layout: Layout, y: u8, cb: u8, cr: u8) -> [u8; 3] {
    if layout == Layout::Gray {
        [y; 3]
    } else {
        ycc_to_rgb(y, cb, cr)
    }
}

/// Extends a `size`-bit magnitude (1..=11) to a signed value (JPEG
/// `EXTEND`): values below `2^(size-1)` are negative.
#[inline(always)]
fn extend(v: u32, size: u32) -> i32 {
    let v = v as i32;
    let half = 1i32.wrapping_shl(size.wrapping_sub(1));
    if v < half { v - (half << 1) + 1 } else { v }
}

/// The Y, Cb, Cr samples at MCU-local pixel (`c`, `r`) with `1 << sh`
/// samples per block side; chroma is replicated (nearest neighbour) for
/// 4:2:2 and 4:2:0.
fn ycc_at(blocks: &McuBlocks, layout: Layout, sh: u32, c: usize, r: usize) -> [u8; 3] {
    let mask = (1 << sh) - 1;
    let get = |b: usize, x: usize, y: usize| at(row(blocks, b, y & mask), x & mask);
    let h = layout.luma_h();
    let y = get((r >> sh) * h + (c >> sh), c, r);
    if layout == Layout::Gray {
        return [y, 128, 128];
    }
    let l = layout.luma_blocks();
    let (cx, cy) = (c >> (h - 1), r >> (layout.luma_v() - 1));
    [y, get(l, cx, cy), get(l + 1, cx, cy)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extend_matches_spec() {
        assert_eq!(extend(0, 1), -1);
        assert_eq!(extend(1, 1), 1);
        assert_eq!(extend(0b010, 3), -5);
        assert_eq!(extend(0b110, 3), 6);
    }

    #[test]
    fn cursor_wraps_rows_and_counts_restarts() {
        let c = Cursor::start(3).advanced(2);
        assert_eq!((c.x, c.y, c.restart_left), (1, 0, 2));
        let c = c.advanced(2);
        assert_eq!((c.x, c.y, c.restart_left), (0, 1, 1));
        let c = c.advanced(2).advanced(2).restarted(3);
        assert_eq!((c.restart_left, c.next_rst), (3, 1));
    }
}
