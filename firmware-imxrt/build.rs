//! Link layouts of the two binaries, and the boot header object.
//!
//! Host builds (no `boot` or `app` feature) do nothing.
//!
//! * `imxrt-boot` (feature `boot`): linked by `imxrt-rt` for a FlexSPI boot
//!   of the RT1050 into HyperFlash block 0 (256 KiB, the flash map's boot
//!   region, so a bootloader that does not fit fails to link: RT9), its
//!   code copied to ITCM at startup. An object file this script writes
//!   gives the boot header's two sections: `.fcb` (the board's factory
//!   FCB, `imxrt_base::boot_header::fcb`) and `.dcd` (the SDK's SDRAM setup,
//!   `boot_header::evkb_sdram`), which `imxrt-rt` places into its header
//!   and points the IVT at, so no `unsafe` `#[link_section]` static is
//!   needed. `boot-image.x` puts the bootloader's `APP_IMAGE` (the SDRAM
//!   image region it copies a slot into) at the firmware's link address
//!   and fails the link otherwise.
//! * `imxrt-base` (feature `app`): cortex-m-rt's `link.x` with
//!   `memory-app.x`: code, read-only data and the data image in the SDRAM
//!   image region (0x8000_0000, 4 MiB, the slot size), the stack and
//!   statics in DTCM.
//!
//! Both use the same FlexRAM banks (`FLEXRAM`), so the firmware's startup
//! leaves the banking the bootloader set unchanged.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

// The pure boot header core, compiled into this script as well (an inline
// module rooted at `src/` so their submodules resolve as in the crate, and
// `crate::hyperflash` inside them still names the HyperFlash module).
// The crate's own lints already run on these files; here they are a second
// compilation, private to the script.
#[allow(dead_code, unused_imports, clippy::all)]
#[path = "src"]
mod core_src {
    pub mod boot_header;
    pub mod hyperflash;
}
use core_src::{boot_header, hyperflash};

/// FlexRAM banks of both binaries: imxrt-rt's default for the RT1050, 8 OCRAM
/// (256 KiB, the boot ROM keeps using bank 0), 4 DTCM (128 KiB), 4 ITCM
/// (128 KiB).
const FLEXRAM: (u32, u32, u32) = (8, 4, 4);

/// GPR17 for `FLEXRAM`, as imxrt-rt computes it: OCRAM banks first, then
/// DTCM, then ITCM, two bits each (01 OCRAM, 10 DTCM, 11 ITCM).
fn gpr17((ocram, dtcm, itcm): (u32, u32, u32)) -> u32 {
    let mask = |n: u32| 1u32.checked_shl(n * 2).map(|b| b - 1).unwrap_or(u32::MAX);
    (0x5555_5555 & mask(ocram))
        | (0xAAAA_AAAA & (mask(dtcm) << (ocram * 2)))
        | mask(itcm).checked_shl((ocram + dtcm) * 2).unwrap_or(0)
}

/// Writes an ELF object holding `.fcb` and `.dcd` and returns its path.
fn boot_header_object(out: &Path) -> PathBuf {
    use object::write::Object;
    use object::{Architecture, BinaryFormat, Endianness, FileFlags, SectionKind};
    let fcb = boot_header::fcb::Fcb::evkb_factory().to_bytes();
    let dcd = boot_header::evkb_sdram::dcd().expect("the SDK's SDRAM DCD encodes");
    let mut obj = Object::new(BinaryFormat::Elf, Architecture::Arm, Endianness::Little);
    // EABI version 5, hard-float, as rustc's objects for thumbv7em-none-eabihf.
    obj.flags = FileFlags::Elf {
        os_abi: 0,
        abi_version: 0,
        e_flags: 0x0500_0400,
    };
    let s = obj.add_section(Vec::new(), b".fcb".to_vec(), SectionKind::ReadOnlyData);
    obj.append_section_data(s, &fcb, 4);
    let s = obj.add_section(Vec::new(), b".dcd".to_vec(), SectionKind::ReadOnlyData);
    obj.append_section_data(s, dcd.as_bytes(), 4);
    let path = out.join("imxrt-boot-header.o");
    fs::write(&path, obj.write().expect("object writes")).expect("object file");
    path
}

/// Splices `boot-image.x` (the SDRAM image region and `APP_IMAGE`'s output
/// section) into imxrt-rt's generated script: the section must come before
/// imxrt-rt's `.bss`, whose `*(.bss .bss.*)` would claim `APP_IMAGE` first.
fn splice_app_image(script: &Path) {
    let frag = fs::read_to_string("boot-image.x").expect("boot-image.x");
    let part = |from: &str, to: Option<&str>| {
        let a = frag.find(from).expect("boot-image.x marker") + from.len();
        let b = to.map_or(frag.len(), |t| frag.find(t).expect("boot-image.x marker"));
        frag[a..b].to_string()
    };
    let memory = part("/* @MEMORY */", Some("/* @SECTION */"));
    let section = part("/* @SECTION */", Some("/* @ASSERT */"));
    let asserts = part("/* @ASSERT */", None);
    let text = fs::read_to_string(script).expect("imxrt-boot.x");
    let anchor = "  .bss (NOLOAD) : ALIGN(4)";
    assert_eq!(
        text.matches(anchor).count(),
        1,
        "imxrt-rt's .bss rule changed; update splice_app_image"
    );
    let text = text.replace(anchor, &format!("{section}\n{anchor}"));
    fs::write(script, format!("{memory}\n{text}\n{asserts}")).expect("imxrt-boot.x");
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/boot_header.rs");
    println!("cargo:rerun-if-changed=src/boot_header");
    println!("cargo:rerun-if-changed=src/hyperflash.rs");
    println!("cargo:rerun-if-changed=src/hyperflash");
    println!("cargo:rerun-if-changed=boot-image.x");
    println!("cargo:rerun-if-changed=memory-app.x");
    let boot = env::var_os("CARGO_FEATURE_BOOT").is_some();
    let app = env::var_os("CARGO_FEATURE_APP").is_some();
    if !(boot || app) {
        return;
    }
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    println!("cargo:rustc-link-search={}", out.display());
    let config = gpr17(FLEXRAM);

    if boot {
        let (ocram, dtcm, itcm) = FLEXRAM;
        imxrt_rt::RuntimeBuilder::from_flexspi(imxrt_rt::Family::Imxrt1050, 256 * 1024)
            .flexram_banks(imxrt_rt::FlexRamBanks { ocram, itcm, dtcm })
            .stack_size(32 * 1024)
            .linker_script_name("imxrt-boot.x")
            .build()
            .expect("imxrt-rt layout");
        splice_app_image(&out.join("imxrt-boot.x"));
        let obj = boot_header_object(&out);
        println!("cargo:rustc-link-arg-bin=imxrt-boot=--nmagic");
        println!("cargo:rustc-link-arg-bin=imxrt-boot={}", obj.display());
        println!("cargo:rustc-link-arg-bin=imxrt-boot=-Timxrt-boot.x");
    }
    if app {
        let memory = fs::read_to_string("memory-app.x")
            .expect("memory-app.x")
            .replace("@FLEXRAM_CONFIG@", &format!("{config:#010x}"));
        fs::write(out.join("memory.x"), memory).expect("memory.x");
        println!("cargo:rustc-link-arg-bin=imxrt-base=--nmagic");
        println!("cargo:rustc-link-arg-bin=imxrt-base=-Tlink.x");
    }
}
