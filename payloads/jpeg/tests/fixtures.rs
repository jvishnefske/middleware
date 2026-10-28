//! Interoperability with libjpeg: decodes the files in `tests/fixtures`
//! (written by Pillow's libjpeg encoder, see `gen.py`) and compares them
//! with libjpeg's own decode (`<name>.rgb`).
//!
//! 4:4:4 and grayscale use libjpeg's IDCT and colour conversion and must
//! match almost exactly. libjpeg upsamples chroma with a triangle filter
//! ("fancy upsampling") where this decoder replicates, so the subsampled
//! files are checked tightly against `<name>.rep.rgb` (ffmpeg's planes with
//! replicated chroma, see `gen.py`) and only loosely against libjpeg.

use mw_jpeg::{Decoder, Error, Layout, Rgb565Be, Rgb888, Step, Unsupported};

/// Decodes `jpeg` completely as RGB888, checking every strip's geometry.
fn decode_rgb(jpeg: &[u8]) -> Result<(u16, u16, Vec<u8>), Error> {
    let mut dec = Decoder::new(jpeg)?;
    let (w, h) = (dec.output_width(), dec.output_height());
    let mut strip = vec![0u8; dec.strip_bytes::<Rgb888>()];
    let mut img = Vec::new();
    for _ in 0..=dec.mcu_rows() {
        match dec.decode_mcu_row::<Rgb888>(jpeg, &mut strip)? {
            Step::Row(s) => {
                assert_eq!(usize::from(s.y) * usize::from(w) * 3, img.len());
                assert_eq!(s.width, w);
                img.extend_from_slice(&strip[..s.bytes(3)]);
            }
            Step::Done => break,
            Step::Partial => unreachable!("a whole row was requested"),
        }
    }
    assert_eq!(img.len(), usize::from(w) * usize::from(h) * 3);
    Ok((w, h, img))
}

/// Mean and maximum absolute difference per channel.
fn errors(a: &[u8], b: &[u8]) -> ([f64; 3], [u8; 3]) {
    let mut sum = [0u64; 3];
    let mut max = [0u8; 3];
    for (pa, pb) in a.chunks_exact(3).zip(b.chunks_exact(3)) {
        for c in 0..3 {
            let d = pa[c].abs_diff(pb[c]);
            sum[c] += u64::from(d);
            max[c] = max[c].max(d);
        }
    }
    let n = (a.len() / 3) as f64;
    (sum.map(|s| s as f64 / n), max)
}

/// Decodes fixture `name` and checks it against reference `<name><ext>`
/// within `mean` and `max` levels per channel.
fn check(name: &str, ext: &str, layout: Layout, mean: f64, max: u8) {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/");
    let jpeg = std::fs::read(format!("{dir}{name}.jpg")).unwrap();
    let want = std::fs::read(format!("{dir}{name}{ext}")).unwrap();
    assert_eq!(Decoder::new(&jpeg).unwrap().header().layout, layout);
    let (_, _, got) = decode_rgb(&jpeg).unwrap();
    let (m, x) = errors(&got, &want);
    eprintln!("{name}{ext}: mean {m:?} max {x:?}");
    for c in 0..3 {
        assert!(
            m[c] <= mean && x[c] <= max,
            "{name}{ext}: mean {m:?} max {x:?}"
        );
    }
}

#[test]
fn libjpeg_444_matches() {
    check("pil_444", ".rgb", Layout::YCbCr444, 0.1, 1);
}

#[test]
fn libjpeg_gray_matches() {
    check("pil_gray", ".rgb", Layout::Gray, 0.1, 1);
}

#[test]
fn libjpeg_422_matches() {
    check("pil_422", ".rep.rgb", Layout::YCbCr422, 0.5, 4);
    check("pil_422", ".rgb", Layout::YCbCr422, 9.0, 96);
}

#[test]
fn libjpeg_420_matches() {
    check("pil_420", ".rep.rgb", Layout::YCbCr420, 0.5, 4);
    check("pil_420", ".rgb", Layout::YCbCr420, 9.0, 96);
}

#[test]
fn libjpeg_420_with_restart_markers_matches() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/");
    let jpeg = std::fs::read(format!("{dir}pil_420_rst.jpg")).unwrap();
    assert_ne!(Decoder::new(&jpeg).unwrap().header().restart_interval, 0);
    check("pil_420_rst", ".rep.rgb", Layout::YCbCr420, 0.5, 4);
    check("pil_420_rst", ".rgb", Layout::YCbCr420, 9.0, 96);
}

#[test]
fn libjpeg_progressive_is_unsupported() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/");
    let jpeg = std::fs::read(format!("{dir}pil_progressive.jpg")).unwrap();
    assert_eq!(
        Decoder::new(&jpeg).unwrap_err(),
        Error::Unsupported(Unsupported::Progressive)
    );
}

#[test]
fn camera_frame_rgb565_matches_rgb888() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/");
    let jpeg = std::fs::read(format!("{dir}fuel.jpg")).unwrap();
    let (w, h, rgb) = decode_rgb(&jpeg).unwrap();
    assert_eq!((w, h), (240, 240));
    let mut dec = Decoder::new(&jpeg).unwrap();
    assert_eq!(dec.strip_bytes::<Rgb565Be>(), 240 * 16 * 2);
    let mut strip = vec![0u8; dec.strip_bytes::<Rgb565Be>()];
    let mut rows = 0;
    // Two MCUs per call, as a fuel-limited payload would.
    for _ in 0..1000 {
        match dec.decode_mcus::<Rgb565Be>(&jpeg, &mut strip, 2).unwrap() {
            Step::Partial => {}
            Step::Row(s) => {
                for (i, px) in strip[..s.bytes(2)].chunks_exact(2).enumerate() {
                    let o = (usize::from(s.y) * 240 + i) * 3;
                    let [r, g, b] = [rgb[o], rgb[o + 1], rgb[o + 2]];
                    let v =
                        (u16::from(r >> 3) << 11) | (u16::from(g >> 2) << 5) | u16::from(b >> 3);
                    assert_eq!(px, v.to_be_bytes());
                }
                rows += 1;
            }
            Step::Done => break,
        }
    }
    assert_eq!(rows, 15);
}

#[test]
fn baked_camera_frames_decode() {
    // The fake camera's frames (`fixtures/cam` in the parent repository,
    // written by `tools/jpeg-fixtures`): what the badge receives today.
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/cam/");
    let mut decoded = 0;
    for (dir, size) in [("jpeg240", 240), ("jpeg120", 120)] {
        for n in 0..8 {
            let path = format!("{root}{dir}/frame_{n:02}.jpg");
            let Ok(jpeg) = std::fs::read(&path) else {
                continue; // a checkout without the parent repository
            };
            let dec = Decoder::new(&jpeg).unwrap();
            assert_eq!(dec.header().layout, Layout::YCbCr420, "{path}");
            let (w, h, _) = decode_rgb(&jpeg).unwrap();
            assert_eq!((w, h), (size, size), "{path}");
            decoded += 1;
        }
    }
    assert!(decoded == 0 || decoded == 16, "{decoded}");
}
