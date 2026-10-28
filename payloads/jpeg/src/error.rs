//! The decoder's error type.
//!
//! Every failure is a value of [`Error`]: the decoder never panics on bad
//! input. [`Error::Unsupported`] means the file is (probably) a valid JPEG
//! using a feature this decoder leaves out; [`Error::Malformed`] means the
//! file breaks the JPEG syntax; [`Error::Truncated`] means the data ends
//! before the image does (an incomplete ESP-NOW reassembly, for example).
//! The remaining variants are misuse by the caller.

/// Why a JPEG could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// A JPEG feature outside baseline 8-bit Huffman decoding.
    Unsupported(Unsupported),
    /// The data violates the JPEG syntax.
    Malformed(Malformed),
    /// The data ends inside a marker segment or inside the entropy-coded
    /// image data.
    Truncated,
    /// The output buffer is shorter than [`Decoder::strip_bytes`]; `needed`
    /// is that length.
    ///
    /// [`Decoder::strip_bytes`]: crate::Decoder::strip_bytes
    BufferTooSmall {
        /// Bytes the strip needs.
        needed: usize,
    },
    /// The `jpeg` slice passed to a decode call is not the length of the
    /// one the decoder was created from. The decoder borrows the input per
    /// call instead of holding it, so the caller must pass the same bytes
    /// every time; the length is the check that catches a swapped buffer.
    InputChanged,
}

/// A JPEG feature this decoder does not implement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsupported {
    /// Progressive DCT (SOF2, SOF6, SOF10, SOF14).
    Progressive,
    /// Lossless (SOF3, SOF7, SOF11, SOF15).
    Lossless,
    /// Hierarchical (differential) frames (SOF5..SOF7, SOF13..SOF15).
    Hierarchical,
    /// Arithmetic coding (SOF9..SOF15 or a DAC segment).
    Arithmetic,
    /// Sample precision other than 8 bits (12-bit JPEG).
    Precision,
    /// A component count other than 1 (grayscale) or 3 (YCbCr); 4 is CMYK.
    Components,
    /// Sampling factors other than 4:4:4, 4:2:2 (h2v1) and 4:2:0 (h2v2) with
    /// luma as the first component.
    Sampling,
    /// A scan that does not hold every component interleaved (a multi-scan
    /// sequential file would need whole-image coefficient storage).
    MultiScan,
    /// 16-bit quantisation tables (`Pq = 1`), not allowed with 8-bit
    /// samples in baseline.
    QuantPrecision,
    /// A Huffman table slot other than 0 or 1 (baseline allows two of each
    /// class).
    TableSlot,
    /// A frame height of 0 (height defined later by a DNL marker).
    Dnl,
    /// Width or height above [`MAX_DIMENSION`](crate::MAX_DIMENSION).
    TooLarge,
    /// [`Scale::Half`](crate::Scale::Half) in a build without the `half`
    /// feature.
    Scale,
}

/// The way the data breaks the JPEG syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Malformed {
    /// The data does not start with SOI (`FF D8`).
    NoSoi,
    /// A byte where a marker (`FF xx`) was expected, or a marker that may
    /// not appear there (RSTn or EOI before the scan, a second SOF).
    UnexpectedMarker,
    /// A segment length shorter than its contents or than 2.
    SegmentLength,
    /// A DQT segment with a bad table id or a zero entry.
    Quantization,
    /// A DHT segment with a bad class, too many codes or an over-full code
    /// space.
    HuffmanTable,
    /// A SOF segment with a zero width, bad component fields or a
    /// duplicate component id.
    Frame,
    /// A SOS segment that does not match the frame, or with non-baseline
    /// spectral selection or approximation.
    Scan,
    /// A component refers to a quantisation or Huffman table that was never
    /// defined.
    MissingTable,
    /// Entropy-coded data with a bit pattern that is no code of the table.
    HuffmanCode,
    /// A decoded coefficient that cannot occur in 8-bit baseline data (a
    /// magnitude category above 11 for DC or 10 for AC, or a run past the
    /// end of the block).
    Coefficient,
    /// The expected restart marker (`RSTn` with the right `n`) is missing
    /// at the end of a restart interval.
    Restart,
}

impl From<Unsupported> for Error {
    fn from(u: Unsupported) -> Self {
        Error::Unsupported(u)
    }
}

impl From<Malformed> for Error {
    fn from(m: Malformed) -> Self {
        Error::Malformed(m)
    }
}
