//! `rp-image`: the RP2350 build's post-link step (host only, feature
//! `image-tool`; run by `build.sh rp2350`).
//!
//! ```text
//! rp-image <IMAGE.uf2|IMAGE.bin> <OUT_DIR>
//! ```
//!
//! Reads the linked update image (the UF2 picotool converted from the ELF,
//! or a raw binary), checks it as `mwctl fw update` and the device do
//! (`mwctl::fw::load_image`, `rp_base::ota::factory::check_update`), and
//! writes, named after the image's version `V`:
//!
//! * `rp-base-rp2350-V-update.bin` and `-update.uf2`: for `mwctl fw
//!   update` (TBYB image, flat from the partition start);
//! * `rp-base-rp2350-V-factory.uf2`: for the first flashing over BOOTSEL
//!   (partition table, the image bought in slot A, slot B and the update
//!   record erased; absolute addresses);
//! * `rp-base-rp2350-V-factory.bin`: the same flash contents from offset
//!   0, for inspection.
//!
//! All the byte layout is `rp_base::ota` (pure, unit tested); this is file
//! input and output only. Exit status 1 with a message when the image is
//! refused.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rp_base::layout::RP2350;
use rp_base::ota::factory::{check_update, factory_flash, factory_len};
use rp_base::ota::image::{SENTINEL_OFFSET, START_BLOCK_OFFSET};
use rp_base::ota::uf2;

fn run(input: &Path, out_dir: &Path) -> Result<(), String> {
    let file = std::fs::read(input).map_err(|e| format!("{}: {e}", input.display()))?;
    let img = mwctl::fw::load_image(&file, None).map_err(|e| format!("{}: {e}", input.display()))?;
    let ab = RP2350.ab().ok_or("the RP2350 map has no A/B slots")?;
    let def = check_update(&img.bytes, ab.a.size).map_err(|e| format!("{}: {e:?}", input.display()))?;
    let v = mwctl::fw::format_version(img.version);
    let name = |suffix: &str| -> PathBuf { out_dir.join(format!("rp-base-rp2350-{v}-{suffix}")) };
    let write = |path: PathBuf, bytes: &[u8]| -> Result<PathBuf, String> {
        std::fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(path)
    };

    let mut factory = vec![0xFFu8; factory_len(&RP2350, img.bytes.len()).ok_or("image too large")?];
    factory_flash(&img.bytes, &RP2350, &mut factory).map_err(|e| format!("factory image: {e:?}"))?;
    let to_uf2 = |pages: Vec<uf2::Page>, family: u32| -> Result<Vec<u8>, String> {
        let mut out = vec![0u8; pages.len() * uf2::BLOCK];
        let n = uf2::write(pages.into_iter(), family, &mut out).ok_or("UF2 buffer")?;
        out.truncate(n);
        Ok(out)
    };
    let update_uf2 = to_uf2(uf2::update_pages(&img.bytes).collect(), uf2::FAMILY_RP2350_ARM_S)?;
    let factory_uf2 = to_uf2(uf2::factory_pages(&factory, &RP2350).collect(), uf2::FAMILY_ABSOLUTE)?;

    let written = [
        write(name("update.bin"), &img.bytes)?,
        write(name("update.uf2"), &update_uf2)?,
        write(name("factory.bin"), &factory)?,
        write(name("factory.uf2"), &factory_uf2)?,
    ];
    println!(
        "image {v}: {} bytes (slot {} bytes), IMAGE_DEF at {START_BLOCK_OFFSET:#x} flags {:#06x}, sentinel at {SENTINEL_OFFSET:#x}",
        img.bytes.len(),
        ab.a.size,
        def.flags
    );
    println!(
        "partition table at 0x0; slot A {:#x}..{:#x}, slot B {:#x}..{:#x}; update record {:#x}..{:#x}",
        ab.a.offset,
        ab.a.end(),
        ab.b.offset,
        ab.b.end(),
        ab.record.offset,
        ab.record.end()
    );
    for p in written {
        println!("  {}", p.display());
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [input, out_dir] = args.as_slice() else {
        eprintln!("usage: rp-image <IMAGE.uf2|IMAGE.bin> <OUT_DIR>");
        return ExitCode::from(2);
    };
    match run(Path::new(input), Path::new(out_dir)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rp-image: {e}");
            ExitCode::FAILURE
        }
    }
}
