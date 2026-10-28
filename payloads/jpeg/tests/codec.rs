//! Round trips through the test encoder (`codec/encoder.rs`): every
//! supported layout, several sizes (including ones that are not a multiple
//! of the MCU), qualities and restart intervals, at both scales, against
//! the encoder's floating point reference decode; plus the step APIs, the
//! pixel formats and every error the decoder reports.
//!
//! Observed against the reference (all cases): mean absolute error at
//! most 0.17 levels per channel and 3 at worst at full scale, 0.22 and 2 at
//! half scale. The bounds below leave a little room.

#[path = "codec/encoder.rs"]
mod encoder;

use encoder::{Sampling, encode, image};
use mw_jpeg::{
    Decoder, Error, Layout, Malformed, PixelFormat, Rgb565Be, Rgb888, Scale, Step, Unsupported,
};

/// How a test drives the decoder.
#[derive(Debug, Clone, Copy)]
enum Drive {
    /// `decode_mcu_row`.
    Rows,
    /// `decode_mcus(n)`.
    Mcus(u16),
    /// `decode_blocks(n)`.
    Blocks(u16),
}

/// Decodes `jpeg` completely with pixel format `F`, checking each strip's
/// geometry, and returns the image.
fn decode<F: PixelFormat>(jpeg: &[u8], scale: Scale, drive: Drive) -> Result<Vec<u8>, Error> {
    let mut dec = Decoder::new_scaled(jpeg, scale)?;
    let (w, h) = (
        usize::from(dec.output_width()),
        usize::from(dec.output_height()),
    );
    let mut strip = vec![0u8; dec.strip_bytes::<F>()];
    let mut img = Vec::new();
    // Generous bound: one call per block, plus the final `Done`.
    let calls = usize::from(dec.mcus_per_row()) * usize::from(dec.mcu_rows()) * 6 + 2;
    for _ in 0..calls {
        let step = match drive {
            Drive::Rows => dec.decode_mcu_row::<F>(jpeg, &mut strip)?,
            Drive::Mcus(n) => dec.decode_mcus::<F>(jpeg, &mut strip, n)?,
            Drive::Blocks(n) => dec.decode_blocks::<F>(jpeg, &mut strip, n)?,
        };
        match step {
            Step::Partial => {}
            Step::Row(s) => {
                assert_eq!(usize::from(s.y) * w * F::BYTES, img.len());
                assert_eq!(usize::from(s.width), w);
                assert!(s.height <= dec.strip_height());
                img.extend_from_slice(&strip[..s.bytes(F::BYTES)]);
            }
            Step::Done => {
                assert_eq!(img.len(), w * h * F::BYTES);
                return Ok(img);
            }
        }
    }
    panic!("decoder did not finish within {calls} calls");
}

/// Mean and maximum absolute difference per channel.
fn errors(a: &[u8], b: &[u8]) -> ([f64; 3], [u8; 3]) {
    assert_eq!(a.len(), b.len());
    let mut sum = [0u64; 3];
    let mut max = [0u8; 3];
    for (pa, pb) in a.chunks_exact(3).zip(b.chunks_exact(3)) {
        for c in 0..3 {
            let d = pa[c].abs_diff(pb[c]);
            sum[c] += u64::from(d);
            max[c] = max[c].max(d);
        }
    }
    let n = (a.len() / 3).max(1) as f64;
    (sum.map(|s| s as f64 / n), max)
}

/// Asserts the per-channel errors are within bounds.
fn assert_close(what: &str, got: &[u8], want: &[u8], mean: f64, max: u8) {
    let (m, x) = errors(got, want);
    for c in 0..3 {
        assert!(m[c] <= mean && x[c] <= max, "{what}: mean {m:?} max {x:?}");
    }
}

const ALL: [Sampling; 4] = [
    Sampling::Gray,
    Sampling::S444,
    Sampling::S422,
    Sampling::S420,
];

/// Sizes: a single pixel, sizes below, at and above one MCU in each
/// direction, and the OV2640's QVGA.
const SIZES: [(usize, usize); 8] = [
    (1, 1),
    (7, 5),
    (8, 8),
    (16, 16),
    (17, 9),
    (33, 47),
    (64, 48),
    (320, 240),
];

#[test]
fn full_scale_matches_reference_for_every_layout_size_and_quality() {
    for sampling in ALL {
        for (w, h) in SIZES {
            for quality in [30, 75, 95] {
                if (w, h) == (320, 240) && quality != 75 {
                    continue; // the big image once per layout keeps the test quick
                }
                let rgb = image(w, h, 6, (w * 31 + h) as u32);
                let e = encode(&rgb, w, h, sampling, quality, 0);
                let got = decode::<Rgb888>(&e.jpeg, Scale::Full, Drive::Rows).unwrap();
                let what = format!("{sampling:?} {w}x{h} q{quality}");
                assert_close(&what, &got, &e.full, 0.3, 3);
            }
        }
    }
}

#[test]
fn restart_intervals_decode_like_the_reference() {
    for sampling in ALL {
        for restart in [1, 2, 5] {
            let (w, h) = (45, 37);
            let rgb = image(w, h, 4, 7);
            let e = encode(&rgb, w, h, sampling, 80, restart);
            let dec = Decoder::new(&e.jpeg).unwrap();
            assert_eq!(dec.header().restart_interval, restart);
            let got = decode::<Rgb888>(&e.jpeg, Scale::Full, Drive::Blocks(1)).unwrap();
            let what = format!("{sampling:?} restart {restart}");
            assert_close(&what, &got, &e.full, 0.3, 3);
        }
    }
}

#[test]
fn half_scale_matches_reference() {
    for sampling in ALL {
        for (w, h) in [(1, 1), (17, 9), (33, 47), (240, 240)] {
            let rgb = image(w, h, 3, 11);
            let e = encode(&rgb, w, h, sampling, 85, 0);
            let got = decode::<Rgb888>(&e.jpeg, Scale::Half, Drive::Rows).unwrap();
            let what = format!("half {sampling:?} {w}x{h}");
            assert_close(&what, &got, &e.half, 0.5, 4);
        }
    }
}

#[test]
fn every_step_size_gives_the_same_image() {
    for sampling in ALL {
        let (w, h) = (50, 35);
        let e = encode(&image(w, h, 8, 3), w, h, sampling, 70, 3);
        for scale in [Scale::Full, Scale::Half] {
            let rows = decode::<Rgb565Be>(&e.jpeg, scale, Drive::Rows).unwrap();
            for drive in [
                Drive::Mcus(1),
                Drive::Mcus(3),
                Drive::Blocks(1),
                Drive::Blocks(5),
                Drive::Blocks(100),
            ] {
                let got = decode::<Rgb565Be>(&e.jpeg, scale, drive).unwrap();
                assert!(got == rows, "{sampling:?} {scale:?} {drive:?}");
            }
        }
    }
}

#[test]
fn rgb565_is_rgb888_truncated_and_big_endian() {
    let (w, h) = (40, 24);
    let e = encode(&image(w, h, 5, 9), w, h, Sampling::S420, 75, 0);
    let rgb = decode::<Rgb888>(&e.jpeg, Scale::Full, Drive::Rows).unwrap();
    let px = decode::<Rgb565Be>(&e.jpeg, Scale::Full, Drive::Rows).unwrap();
    for (c, p) in rgb.chunks_exact(3).zip(px.chunks_exact(2)) {
        let v = (u16::from(c[0] >> 3) << 11) | (u16::from(c[1] >> 2) << 5) | u16::from(c[2] >> 3);
        assert_eq!(p, v.to_be_bytes());
    }
}

#[test]
fn header_reports_geometry() {
    for (sampling, layout, mcu) in [
        (Sampling::Gray, Layout::Gray, (8, 8)),
        (Sampling::S444, Layout::YCbCr444, (8, 8)),
        (Sampling::S422, Layout::YCbCr422, (16, 8)),
        (Sampling::S420, Layout::YCbCr420, (16, 16)),
    ] {
        let e = encode(&image(35, 21, 0, 1), 35, 21, sampling, 75, 0);
        let dec = Decoder::new(&e.jpeg).unwrap();
        assert_eq!(dec.header().layout, layout);
        assert_eq!((dec.header().width, dec.header().height), (35, 21));
        assert_eq!((layout.mcu_width(), layout.mcu_height()), mcu);
        assert_eq!(dec.mcus_per_row(), 35u16.div_ceil(mcu.0));
        assert_eq!(dec.mcu_rows(), 21u16.div_ceil(mcu.1));
        assert_eq!(dec.strip_height(), mcu.1);
        assert_eq!(dec.strip_bytes::<Rgb565Be>(), 35 * usize::from(mcu.1) * 2);
        let half = Decoder::new_scaled(&e.jpeg, Scale::Half).unwrap();
        assert_eq!((half.output_width(), half.output_height()), (18, 11));
        assert_eq!(half.strip_height(), mcu.1 / 2);
    }
}

#[test]
fn idle_decoder_is_done_and_restarts_in_place() {
    let e = encode(&image(20, 20, 2, 5), 20, 20, Sampling::S420, 75, 0);
    let mut strip = vec![0u8; 20 * 16 * 2];
    let mut dec = Decoder::IDLE;
    assert_eq!(
        dec.decode_mcu_row::<Rgb565Be>(&e.jpeg, &mut strip),
        Ok(Step::Done)
    );
    dec.start(&e.jpeg, Scale::Full).unwrap();
    assert!(matches!(
        dec.decode_mcu_row::<Rgb565Be>(&e.jpeg, &mut strip),
        Ok(Step::Row(_))
    ));
    // Restarting mid-image begins again at the top.
    dec.start(&e.jpeg, Scale::Full).unwrap();
    let Ok(Step::Row(s)) = dec.decode_mcu_row::<Rgb565Be>(&e.jpeg, &mut strip) else {
        panic!("expected a row");
    };
    assert_eq!(s.y, 0);
    // A failed start leaves it idle.
    assert!(dec.start(&[0xFF, 0xD8], Scale::Full).is_err());
    assert_eq!(
        dec.decode_mcu_row::<Rgb565Be>(&e.jpeg, &mut strip),
        Ok(Step::Done)
    );
}

#[test]
fn misuse_is_reported_without_losing_state() {
    let e = encode(&image(32, 32, 2, 5), 32, 32, Sampling::S420, 75, 0);
    let mut dec = Decoder::new(&e.jpeg).unwrap();
    let mut small = vec![0u8; 100];
    assert_eq!(
        dec.decode_mcu_row::<Rgb565Be>(&e.jpeg, &mut small),
        Err(Error::BufferTooSmall {
            needed: 32 * 16 * 2
        })
    );
    let mut strip = vec![0u8; 32 * 16 * 2];
    let other = [&e.jpeg[..], &[0]].concat();
    assert_eq!(
        dec.decode_mcu_row::<Rgb565Be>(&other, &mut strip),
        Err(Error::InputChanged)
    );
    // Neither error touched the decoder.
    assert!(matches!(
        dec.decode_mcu_row::<Rgb565Be>(&e.jpeg, &mut strip),
        Ok(Step::Row(_))
    ));
    assert_eq!(
        dec.decode_mcus::<Rgb565Be>(&e.jpeg, &mut strip, 0),
        Ok(Step::Partial)
    );
    assert_eq!(
        dec.decode_blocks::<Rgb565Be>(&e.jpeg, &mut strip, 0),
        Ok(Step::Partial)
    );
}

#[test]
fn truncated_data_is_an_error_that_sticks() {
    let e = encode(&image(64, 64, 10, 5), 64, 64, Sampling::S420, 90, 0);
    // Cut in the middle of the entropy-coded data.
    let cut = &e.jpeg[..e.jpeg.len() * 2 / 3];
    let mut dec = Decoder::new(cut).unwrap();
    let mut strip = vec![0u8; dec.strip_bytes::<Rgb565Be>()];
    let mut result = Ok(Step::Partial);
    for _ in 0..10 {
        result = dec.decode_mcu_row::<Rgb565Be>(cut, &mut strip);
        if result.is_err() {
            break;
        }
    }
    assert_eq!(result, Err(Error::Truncated));
    assert_eq!(
        dec.decode_mcu_row::<Rgb565Be>(cut, &mut strip),
        Err(Error::Truncated)
    );
    // Cut inside the headers.
    for len in [0, 1, 2, 3, 10, 20, 100] {
        assert!(Decoder::new(&e.jpeg[..len]).is_err(), "{len}");
    }
}

#[test]
fn missing_eoi_is_accepted() {
    let e = encode(&image(24, 24, 3, 5), 24, 24, Sampling::S422, 75, 0);
    let jpeg = &e.jpeg[..e.jpeg.len() - 2];
    let got = decode::<Rgb888>(jpeg, Scale::Full, Drive::Rows).unwrap();
    assert_close("no EOI", &got, &e.full, 0.6, 3);
}

/// Index of the first `FF <marker>` in `jpeg`.
fn find_marker(jpeg: &[u8], marker: u8) -> usize {
    jpeg.windows(2).position(|w| w == [0xFF, marker]).unwrap()
}

#[test]
fn wrong_restart_marker_is_malformed() {
    let e = encode(&image(48, 16, 3, 5), 48, 16, Sampling::S420, 75, 1);
    let mut jpeg = e.jpeg.clone();
    let sos = find_marker(&jpeg, 0xDA);
    let rst0 = sos
        + jpeg[sos..]
            .windows(2)
            .position(|w| w == [0xFF, 0xD0])
            .unwrap();
    jpeg[rst0 + 1] = 0xD3;
    let mut dec = Decoder::new(&jpeg).unwrap();
    let mut strip = vec![0u8; dec.strip_bytes::<Rgb565Be>()];
    assert_eq!(
        dec.decode_mcu_row::<Rgb565Be>(&jpeg, &mut strip),
        Err(Error::Malformed(Malformed::Restart))
    );
}

#[test]
fn unsupported_and_malformed_headers() {
    let e = encode(&image(16, 16, 0, 5), 16, 16, Sampling::S420, 75, 0);
    let sof = find_marker(&e.jpeg, 0xC0);
    let patched = |offset: usize, value: u8| {
        let mut j = e.jpeg.clone();
        j[offset] = value;
        Decoder::new(&j).map(|_| ())
    };
    let unsupported = |u| Err(Error::Unsupported(u));
    let malformed = |m| Err(Error::Malformed(m));
    // SOF type.
    assert_eq!(
        patched(sof + 1, 0xC2),
        unsupported(Unsupported::Progressive)
    );
    assert_eq!(patched(sof + 1, 0xC3), unsupported(Unsupported::Lossless));
    assert_eq!(
        patched(sof + 1, 0xC5),
        unsupported(Unsupported::Hierarchical)
    );
    assert_eq!(patched(sof + 1, 0xC9), unsupported(Unsupported::Arithmetic));
    assert_eq!(patched(sof + 1, 0xC1), Ok(())); // extended sequential, same limits
    // Precision (12-bit), height 0 (DNL), width 0, too large.
    assert_eq!(patched(sof + 4, 12), unsupported(Unsupported::Precision));
    let mut dnl = e.jpeg.clone();
    dnl[sof + 5..sof + 7].copy_from_slice(&[0, 0]);
    assert_eq!(
        Decoder::new(&dnl).map(|_| ()),
        unsupported(Unsupported::Dnl)
    );
    let mut zero_w = e.jpeg.clone();
    zero_w[sof + 7..sof + 9].copy_from_slice(&[0, 0]);
    assert_eq!(
        Decoder::new(&zero_w).map(|_| ()),
        malformed(Malformed::Frame)
    );
    let mut big = e.jpeg.clone();
    big[sof + 7..sof + 9].copy_from_slice(&4096u16.to_be_bytes());
    assert_eq!(
        Decoder::new(&big).map(|_| ()),
        unsupported(Unsupported::TooLarge)
    );
    // Sampling: luma 1x2 (4:4:0), chroma 2x2.
    assert_eq!(patched(sof + 11, 0x12), unsupported(Unsupported::Sampling));
    assert_eq!(patched(sof + 14, 0x22), unsupported(Unsupported::Sampling));
    // Not SOI.
    assert_eq!(patched(1, 0xD9), malformed(Malformed::NoSoi));
    // 16-bit quantisation table.
    let dqt = find_marker(&e.jpeg, 0xDB);
    assert_eq!(
        patched(dqt + 4, 0x10),
        unsupported(Unsupported::QuantPrecision)
    );
    // A zero quantiser.
    assert_eq!(patched(dqt + 5, 0), malformed(Malformed::Quantization));
    // Huffman table slot 2.
    let dht = find_marker(&e.jpeg, 0xC4);
    assert_eq!(patched(dht + 4, 0x02), unsupported(Unsupported::TableSlot));
    // A scan referring to an undefined Huffman table... slot 1 of a gray
    // image has none.
    let g = encode(&image(8, 8, 0, 5), 8, 8, Sampling::Gray, 75, 0);
    let sos = find_marker(&g.jpeg, 0xDA);
    let mut j = g.jpeg.clone();
    j[sos + 6] = 0x11;
    assert_eq!(
        Decoder::new(&j).map(|_| ()),
        malformed(Malformed::MissingTable)
    );
    // A scan with only some of the components.
    let sos = find_marker(&e.jpeg, 0xDA);
    let mut j = e.jpeg.clone();
    j[sos + 4] = 1;
    assert_eq!(
        Decoder::new(&j).map(|_| ()),
        unsupported(Unsupported::MultiScan)
    );
    // Spectral selection of a progressive scan.
    let mut j = e.jpeg.clone();
    let tail = sos + 2 + usize::from(u16::from_be_bytes([j[sos + 2], j[sos + 3]])) - 3;
    j[tail + 1] = 5;
    assert_eq!(Decoder::new(&j).map(|_| ()), malformed(Malformed::Scan));
}

#[test]
fn four_components_are_unsupported() {
    // A minimal CMYK-style SOF0 with four components.
    let mut j = vec![0xFF, 0xD8, 0xFF, 0xC0, 0, 20, 8, 0, 8, 0, 8, 4];
    for id in 1..=4 {
        j.extend_from_slice(&[id, 0x11, 0]);
    }
    j.extend_from_slice(&[0xFF, 0xD9]);
    assert_eq!(
        Decoder::new(&j).map(|_| ()),
        Err(Error::Unsupported(Unsupported::Components))
    );
}

#[test]
fn arithmetic_conditioning_segment_is_unsupported() {
    let j = [0xFF, 0xD8, 0xFF, 0xCC, 0, 4, 0, 0];
    assert_eq!(
        Decoder::new(&j).map(|_| ()),
        Err(Error::Unsupported(Unsupported::Arithmetic))
    );
}
