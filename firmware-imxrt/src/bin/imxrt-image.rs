//! Host tool: turns the firmware's raw binary into a slot image.
//!
//! `imxrt-image IN.bin MAJOR.MINOR OUT.bin` prepends the 256-byte slot
//! header (`imxrt_base::ab::header::SlotHeader`: magic, header version,
//! image version, length, SHA-256 over the header fields and the image),
//! which the bootloader checks against the SDRAM copy. `build.sh` runs it.

use std::process::ExitCode;

use imxrt_base::ab::header::SlotHeader;
use middleware::loader::trust::Version;

fn version(text: &str) -> Option<Version> {
    let (major, minor) = text.split_once('.')?;
    let major: u16 = major.parse().ok()?;
    let minor: u16 = minor.parse().ok()?;
    Some(Version((u32::from(major) << 16) | u32::from(minor)))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let [_, input, ver, output] = args.as_slice() else {
        eprintln!("usage: imxrt-image IN.bin MAJOR.MINOR OUT.bin");
        return ExitCode::from(2);
    };
    let Some(ver) = version(ver) else {
        eprintln!("imxrt-image: version must be MAJOR.MINOR, got {ver}");
        return ExitCode::from(2);
    };
    let image = match std::fs::read(input) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("imxrt-image: {input}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let Some(header) = SlotHeader::for_image(ver, &image) else {
        eprintln!("imxrt-image: {input}: empty or too large for a slot image");
        return ExitCode::FAILURE;
    };
    let mut out = header.encode().to_vec();
    out.extend_from_slice(&image);
    if let Err(e) = std::fs::write(output, &out) {
        eprintln!("imxrt-image: {output}: {e}");
        return ExitCode::FAILURE;
    }
    println!(
        "{output}: v{}.{}, {} + {} bytes",
        ver.0 >> 16,
        ver.0 & 0xFFFF,
        out.len() - image.len(),
        image.len()
    );
    ExitCode::SUCCESS
}
