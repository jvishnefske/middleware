//! Marker parsing: SOI up to and including SOS.
//!
//! [`parse`] walks the marker segments once, validating every field, fills
//! the quantisation and Huffman [`Tables`], and returns the frame geometry
//! ([`Header`]), the per-component table selectors and the offset of the
//! first entropy-coded byte. It accepts:
//!
//! * SOI first; APPn, COM and other unknown segments skipped by length;
//!   fill bytes (`FF FF …`) before a marker; standalone TEM.
//! * DQT with 8-bit tables 0..=3 (16-bit tables are
//!   [`Unsupported::QuantPrecision`]).
//! * DHT with DC/AC tables 0..=1.
//! * SOF0 (baseline) or SOF1 (extended sequential, which is the same
//!   decoding process when it obeys the same limits), 8-bit, one or three
//!   components, sampling 4:4:4, 4:2:2 (h2v1) or 4:2:0 (h2v2).
//! * DRI.
//! * One SOS with every frame component, in frame order, `Ss = 0`,
//!   `Se = 63`, `Ah = Al = 0`.
//!
//! Every other SOFn, DAC, DHP and EXP are [`Unsupported`]; EOI, SOI or RSTn
//! before the scan are [`Malformed::UnexpectedMarker`]. The walk is bounded:
//! each step consumes at least two bytes and the loop runs at most
//! `data.len()` times.

use crate::MAX_DIMENSION;
use crate::error::{Error, Malformed, Unsupported};
use crate::huffman::HuffTable;

/// Colour components and chroma subsampling of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// One component, output as grey (R = G = B = Y). MCU: one 8×8 block.
    Gray,
    /// YCbCr, no subsampling. MCU: 8×8, blocks Y Cb Cr.
    YCbCr444,
    /// YCbCr, chroma halved horizontally (h2v1). MCU: 16×8, blocks Y Y Cb Cr.
    YCbCr422,
    /// YCbCr, chroma halved both ways (h2v2). MCU: 16×16, blocks Y Y Y Y Cb
    /// Cr.
    YCbCr420,
}

impl Layout {
    /// Luma blocks across one MCU.
    pub const fn luma_h(self) -> usize {
        match self {
            Layout::Gray | Layout::YCbCr444 => 1,
            Layout::YCbCr422 | Layout::YCbCr420 => 2,
        }
    }

    /// Luma blocks down one MCU.
    pub const fn luma_v(self) -> usize {
        match self {
            Layout::YCbCr420 => 2,
            _ => 1,
        }
    }

    /// Luma blocks per MCU.
    pub const fn luma_blocks(self) -> usize {
        self.luma_h() * self.luma_v()
    }

    /// Blocks per MCU (luma, then one Cb and one Cr for colour).
    pub const fn blocks(self) -> usize {
        match self {
            Layout::Gray => 1,
            _ => self.luma_blocks() + 2,
        }
    }

    /// MCU width in pixels (8 or 16).
    pub const fn mcu_width(self) -> u16 {
        8 * self.luma_h() as u16
    }

    /// MCU height in pixels (8 or 16).
    pub const fn mcu_height(self) -> u16 {
        8 * self.luma_v() as u16
    }

    /// Frame component that block `b` of an MCU belongs to.
    pub(crate) const fn component_of_block(self, b: usize) -> usize {
        let l = self.luma_blocks();
        if b < l { 0 } else { b - l + 1 }
    }
}

/// Frame geometry and coding parameters from the markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Image width in pixels (1..=[`MAX_DIMENSION`]).
    pub width: u16,
    /// Image height in pixels (1..=[`MAX_DIMENSION`]).
    pub height: u16,
    /// Components and subsampling.
    pub layout: Layout,
    /// MCUs per restart interval (DRI), 0 for none.
    pub restart_interval: u16,
}

/// Table selectors of one frame component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Component {
    /// Quantisation table (0..=3).
    pub(crate) quant: u8,
    /// DC Huffman table (0..=1).
    pub(crate) dc: u8,
    /// AC Huffman table (0..=1).
    pub(crate) ac: u8,
}

/// The quantisation and Huffman tables defined by DQT and DHT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tables {
    /// Quantisation tables in zigzag order (as stored in DQT).
    pub(crate) quant: [[u8; 64]; 4],
    /// Which quantisation tables were defined.
    pub(crate) quant_defined: [bool; 4],
    /// DC Huffman tables.
    pub(crate) dc: [HuffTable; 2],
    /// AC Huffman tables.
    pub(crate) ac: [HuffTable; 2],
}

impl Tables {
    /// No tables defined.
    pub(crate) const EMPTY: Tables = Tables {
        quant: [[0; 64]; 4],
        quant_defined: [false; 4],
        dc: [HuffTable::EMPTY; 2],
        ac: [HuffTable::EMPTY; 2],
    };
}

/// What [`parse`] returns besides the tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Parsed {
    /// Frame geometry.
    pub(crate) header: Header,
    /// Table selectors of components 0..=2 (only the first is used for
    /// grayscale).
    pub(crate) comps: [Component; 3],
    /// Offset of the first entropy-coded byte.
    pub(crate) entropy: usize,
}

/// A SOF segment, before the scan assigns Huffman tables.
#[derive(Debug, Clone, Copy)]
struct Frame {
    /// Geometry (restart interval filled in at SOS).
    header: Header,
    /// Component ids in frame order.
    ids: [u8; 3],
    /// Component count (1 or 3).
    count: usize,
    /// Quantisation table of each component.
    quant: [u8; 3],
}

/// Big-endian `u16` at `i`.
fn be16(seg: &[u8], i: usize) -> Result<u16, Error> {
    match seg.get(i..i.saturating_add(2)) {
        Some(&[a, b]) => Ok(u16::from_be_bytes([a, b])),
        _ => Err(Malformed::SegmentLength.into()),
    }
}

/// Byte at `i` of a segment.
fn byte(seg: &[u8], i: usize) -> Result<u8, Error> {
    seg.get(i).copied().ok_or(Malformed::SegmentLength.into())
}

/// Parses the markers of `data` up to the end of the SOS segment, filling
/// `tables`.
#[inline(never)]
pub(crate) fn parse(data: &[u8], tables: &mut Tables) -> Result<Parsed, Error> {
    if data.get(0..2) != Some(&[0xFF, 0xD8]) {
        return Err(Malformed::NoSoi.into());
    }
    let mut pos = 2usize;
    let mut frame: Option<Frame> = None;
    let mut restart_interval = 0u16;
    for _ in 0..data.len() {
        if *data.get(pos).ok_or(Error::Truncated)? != 0xFF {
            return Err(Malformed::UnexpectedMarker.into());
        }
        // Skip fill bytes: the marker code is the first byte after the FFs.
        let rest = data.get(pos..).unwrap_or(&[]);
        let fill = rest.iter().take_while(|&&b| b == 0xFF).count();
        let code = *rest.get(fill).ok_or(Error::Truncated)?;
        pos = pos.saturating_add(fill).saturating_add(1);
        match code {
            0x01 => continue, // TEM: no length
            0xD0..=0xD9 | 0x00 => return Err(Malformed::UnexpectedMarker.into()),
            _ => {}
        }
        let len = usize::from(be16(data, pos).map_err(|_| Error::Truncated)?);
        if len < 2 {
            return Err(Malformed::SegmentLength.into());
        }
        let seg = data
            .get(pos.saturating_add(2)..pos.saturating_add(len))
            .ok_or(Error::Truncated)?;
        pos = pos.saturating_add(len);
        match code {
            0xDB => parse_dqt(seg, tables)?,
            0xC4 => parse_dht(seg, tables)?,
            0xC0 | 0xC1 => {
                if frame.is_some() {
                    return Err(Malformed::UnexpectedMarker.into());
                }
                frame = Some(parse_sof(seg)?);
            }
            0xC2 => return Err(Unsupported::Progressive.into()),
            0xC3 => return Err(Unsupported::Lossless.into()),
            0xC5..=0xC7 | 0xDE | 0xDF => return Err(Unsupported::Hierarchical.into()),
            0xC9..=0xCF => return Err(Unsupported::Arithmetic.into()),
            0xC8 => return Err(Malformed::UnexpectedMarker.into()),
            0xDD => {
                if seg.len() != 2 {
                    return Err(Malformed::SegmentLength.into());
                }
                restart_interval = be16(seg, 0)?;
            }
            0xDA => {
                let f = frame.ok_or(Malformed::UnexpectedMarker)?;
                let comps = parse_sos(seg, &f, tables)?;
                let header = Header {
                    restart_interval,
                    ..f.header
                };
                return Ok(Parsed {
                    header,
                    comps,
                    entropy: pos,
                });
            }
            _ => {} // APPn, COM, DNL and reserved: skipped
        }
    }
    Err(Error::Truncated)
}

/// DQT: one or more 8-bit tables.
#[inline(never)]
fn parse_dqt(seg: &[u8], tables: &mut Tables) -> Result<(), Error> {
    let mut rest = seg;
    for _ in 0..seg.len() {
        let Some((&pq_tq, body)) = rest.split_first() else {
            return Ok(());
        };
        let (pq, tq) = (pq_tq >> 4, usize::from(pq_tq & 15));
        match pq {
            0 => {}
            1 => return Err(Unsupported::QuantPrecision.into()),
            _ => return Err(Malformed::Quantization.into()),
        }
        let values: &[u8; 64] = body.first_chunk().ok_or(Malformed::SegmentLength)?;
        if values.contains(&0) {
            return Err(Malformed::Quantization.into());
        }
        let dst = tables.quant.get_mut(tq).ok_or(Malformed::Quantization)?;
        *dst = *values;
        if let Some(d) = tables.quant_defined.get_mut(tq) {
            *d = true;
        }
        rest = body.get(64..).unwrap_or(&[]);
    }
    Ok(())
}

/// DHT: one or more tables.
#[inline(never)]
fn parse_dht(seg: &[u8], tables: &mut Tables) -> Result<(), Error> {
    let mut rest = seg;
    for _ in 0..seg.len() {
        let Some((&tc_th, body)) = rest.split_first() else {
            return Ok(());
        };
        let (tc, th) = (tc_th >> 4, usize::from(tc_th & 15));
        let mut counts = [0u8; 16];
        let lengths = body.get(..16).ok_or(Malformed::SegmentLength)?;
        for (d, &v) in counts.iter_mut().zip(lengths) {
            *d = v;
        }
        let n: usize = counts.iter().map(|&c| usize::from(c)).sum();
        let symbols = body
            .get(16..16usize.saturating_add(n))
            .ok_or(Malformed::SegmentLength)?;
        let table = HuffTable::build(&counts, symbols)?;
        let class = match tc {
            0 => &mut tables.dc,
            1 => &mut tables.ac,
            _ => return Err(Malformed::HuffmanTable.into()),
        };
        match class.get_mut(th) {
            Some(slot) => *slot = table,
            None if th <= 3 => return Err(Unsupported::TableSlot.into()),
            None => return Err(Malformed::HuffmanTable.into()),
        }
        rest = body.get(16usize.saturating_add(n)..).unwrap_or(&[]);
    }
    Ok(())
}

/// SOF0/SOF1: precision, size and components.
#[inline(never)]
fn parse_sof(seg: &[u8]) -> Result<Frame, Error> {
    if byte(seg, 0)? != 8 {
        return Err(Unsupported::Precision.into());
    }
    let height = be16(seg, 1)?;
    let width = be16(seg, 3)?;
    let count = usize::from(byte(seg, 5)?);
    if height == 0 {
        return Err(Unsupported::Dnl.into());
    }
    if width == 0 || count == 0 {
        return Err(Malformed::Frame.into());
    }
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(Unsupported::TooLarge.into());
    }
    if count != 1 && count != 3 {
        return Err(Unsupported::Components.into());
    }
    if seg.len() != 6 + 3 * count {
        return Err(Malformed::SegmentLength.into());
    }
    let mut ids = [0u8; 3];
    let mut quant = [0u8; 3];
    let mut sampling = [(0u8, 0u8); 3];
    let fields = seg.get(6..).unwrap_or(&[]).chunks_exact(3);
    for (i, f) in fields.enumerate() {
        let &[id, hv, tq] = f else {
            return Err(Malformed::Frame.into());
        };
        let (h, v) = (hv >> 4, hv & 15);
        if !(1..=4).contains(&h) || !(1..=4).contains(&v) || tq > 3 {
            return Err(Malformed::Frame.into());
        }
        if ids.iter().take(i).any(|&p| p == id) {
            return Err(Malformed::Frame.into());
        }
        if let (Some(d), Some(q), Some(s)) = (ids.get_mut(i), quant.get_mut(i), sampling.get_mut(i))
        {
            *d = id;
            *q = tq;
            *s = (h, v);
        }
    }
    let layout = if count == 1 {
        // A single component is one non-interleaved block per MCU whatever
        // its sampling factors say.
        Layout::Gray
    } else {
        let [luma, cb, cr] = sampling;
        if cb != (1, 1) || cr != (1, 1) {
            return Err(Unsupported::Sampling.into());
        }
        match luma {
            (1, 1) => Layout::YCbCr444,
            (2, 1) => Layout::YCbCr422,
            (2, 2) => Layout::YCbCr420,
            _ => return Err(Unsupported::Sampling.into()),
        }
    };
    Ok(Frame {
        header: Header {
            width,
            height,
            layout,
            restart_interval: 0,
        },
        ids,
        count,
        quant,
    })
}

/// SOS: must cover every component of `frame` in order, with defined
/// tables.
#[inline(never)]
fn parse_sos(seg: &[u8], frame: &Frame, tables: &Tables) -> Result<[Component; 3], Error> {
    let ns = usize::from(byte(seg, 0)?);
    if ns == 0 || ns > frame.count {
        return Err(Malformed::Scan.into());
    }
    if ns < frame.count {
        return Err(Unsupported::MultiScan.into());
    }
    if seg.len() != 1 + 2 * ns + 3 {
        return Err(Malformed::SegmentLength.into());
    }
    let mut comps = [Component::default(); 3];
    let selectors = seg.get(1..1 + 2 * ns).unwrap_or(&[]).chunks_exact(2);
    for (i, s) in selectors.enumerate() {
        let &[cs, td_ta] = s else {
            return Err(Malformed::Scan.into());
        };
        if frame.ids.get(i) != Some(&cs) {
            return Err(Malformed::Scan.into());
        }
        let (td, ta) = (td_ta >> 4, td_ta & 15);
        if td > 1 || ta > 1 {
            return Err(if td <= 3 && ta <= 3 {
                Unsupported::TableSlot.into()
            } else {
                Malformed::Scan.into()
            });
        }
        let quant = frame.quant.get(i).copied().unwrap_or(0);
        let defined = tables
            .quant_defined
            .get(usize::from(quant))
            .copied()
            .unwrap_or(false)
            && tables
                .dc
                .get(usize::from(td))
                .is_some_and(HuffTable::is_defined)
            && tables
                .ac
                .get(usize::from(ta))
                .is_some_and(HuffTable::is_defined);
        if !defined {
            return Err(Malformed::MissingTable.into());
        }
        if let Some(c) = comps.get_mut(i) {
            *c = Component {
                quant,
                dc: td,
                ac: ta,
            };
        }
    }
    let tail = seg.get(1 + 2 * ns..).unwrap_or(&[]);
    if tail != [0, 63, 0] {
        return Err(Malformed::Scan.into());
    }
    Ok(comps)
}
