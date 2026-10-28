//! Robustness: thousands of deterministic random mutations and truncations
//! of valid JPEGs must never panic (this test runs with overflow checks in
//! the default test profile, so arithmetic overflow would panic too) and
//! must finish within the documented number of calls.
//!
//! The seeds come from a fixed xorshift generator, so a failure reproduces
//! exactly; the panic message names the case.

// The reference images are not needed here.
#[allow(dead_code)]
#[path = "codec/encoder.rs"]
mod encoder;

use encoder::{Sampling, encode, image};
use mw_jpeg::{Decoder, Error, Rgb565Be, Rgb888, Scale, Step};

/// xorshift32.
struct Rng(u32);

impl Rng {
    fn next(&mut self) -> u32 {
        let mut s = self.0;
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        self.0 = s;
        s
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() as usize) % n.max(1)
    }
}

/// Valid seed images: every layout, with and without restart intervals,
/// plus the libjpeg fixtures.
fn seeds() -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for (i, sampling) in [
        Sampling::Gray,
        Sampling::S444,
        Sampling::S422,
        Sampling::S420,
    ]
    .into_iter()
    .enumerate()
    {
        for restart in [0, 2] {
            let (w, h) = (23 + 8 * i, 19);
            out.push(encode(&image(w, h, 12, i as u32 + 1), w, h, sampling, 60, restart).jpeg);
        }
    }
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/");
    for name in ["pil_420", "pil_422", "pil_444", "pil_gray", "pil_420_rst"] {
        out.push(std::fs::read(format!("{dir}{name}.jpg")).unwrap());
    }
    out
}

/// One random corruption of `jpeg`.
fn mutate(rng: &mut Rng, jpeg: &[u8]) -> Vec<u8> {
    let mut j = jpeg.to_vec();
    match rng.below(6) {
        // Flip a few random bytes anywhere.
        0 => {
            for _ in 0..1 + rng.below(8) {
                let i = rng.below(j.len());
                j[i] = rng.next() as u8;
            }
        }
        // Flip bytes in the headers only (the first ~600 bytes).
        1 => {
            for _ in 0..1 + rng.below(3) {
                let i = rng.below(j.len().min(600));
                j[i] ^= 1 << rng.below(8);
            }
        }
        // Truncate.
        2 => j.truncate(rng.below(j.len() + 1)),
        // Insert random bytes (markers included).
        3 => {
            let i = rng.below(j.len());
            for _ in 0..1 + rng.below(4) {
                let b = if rng.below(2) == 0 {
                    0xFF
                } else {
                    rng.next() as u8
                };
                j.insert(i, b);
            }
        }
        // Delete a run.
        4 => {
            let i = rng.below(j.len());
            let n = rng.below(16).min(j.len() - i);
            j.drain(i..i + n);
        }
        // Overwrite a run with 0xFF or zeros.
        _ => {
            let i = rng.below(j.len());
            let v = if rng.below(2) == 0 { 0xFF } else { 0 };
            for b in j.iter_mut().skip(i).take(1 + rng.below(32)) {
                *b = v;
            }
        }
    }
    j
}

/// Decodes `jpeg` (if its header parses) with a random scale, format and
/// budget, asserting it ends (`Done` or an error) within one call per block.
fn run(rng: &mut Rng, jpeg: &[u8]) -> Result<(), Error> {
    let scale = if rng.below(2) == 0 || !cfg!(feature = "half") {
        Scale::Full
    } else {
        Scale::Half
    };
    let mut dec = Decoder::new_scaled(jpeg, scale)?;
    let rgb888 = rng.below(2) == 0;
    let bytes = if rgb888 {
        dec.strip_bytes::<Rgb888>()
    } else {
        dec.strip_bytes::<Rgb565Be>()
    };
    let mut strip = vec![0u8; bytes];
    let blocks = usize::from(dec.mcus_per_row()) * usize::from(dec.mcu_rows()) * 6;
    let budget = 1 + rng.below(7) as u16;
    for _ in 0..blocks + 2 {
        let step = if rgb888 {
            dec.decode_blocks::<Rgb888>(jpeg, &mut strip, budget)?
        } else {
            dec.decode_blocks::<Rgb565Be>(jpeg, &mut strip, budget)?
        };
        if step == Step::Done {
            return Ok(());
        }
    }
    panic!("no end after {} calls", blocks + 2);
}

#[test]
fn random_corruptions_never_panic_or_hang() {
    let seeds = seeds();
    let mut rng = Rng(0x2545_F491);
    let (mut ok, mut errors) = (0, 0);
    for case in 0..10_000 {
        let base = &seeds[case % seeds.len()];
        let mut jpeg = mutate(&mut rng, base);
        if rng.below(4) == 0 {
            jpeg = mutate(&mut rng, &jpeg);
        }
        let state = rng.0;
        let result = std::panic::catch_unwind(|| run(&mut Rng(state), &jpeg));
        match result {
            Ok(Ok(())) => ok += 1,
            Ok(Err(_)) => errors += 1,
            Err(_) => panic!("case {case} (seed {state:#x}) panicked"),
        }
    }
    // Both outcomes occur: the mutations are neither all fatal nor all
    // harmless.
    eprintln!("decoded {ok}, rejected {errors}");
    assert!(ok > 100 && errors > 100, "ok {ok} errors {errors}");
}

#[test]
fn every_truncation_of_a_frame_is_handled() {
    let (w, h) = (40, 32);
    let jpeg = encode(&image(w, h, 10, 3), w, h, Sampling::S420, 75, 0).jpeg;
    let mut rng = Rng(7);
    for len in 0..jpeg.len() {
        let cut = &jpeg[..len];
        let result = run(&mut rng, cut);
        // Without the EOI (the last two bytes) the image still decodes;
        // a cut into the headers or the entropy-coded data must fail (the
        // last data byte may hold only padding, hence the margin of one).
        if len + 3 < jpeg.len() {
            assert!(result.is_err(), "len {len}: {result:?}");
        } else if len + 2 == jpeg.len() {
            assert_eq!(result, Ok(()), "len {len}");
        }
    }
}
