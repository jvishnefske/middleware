//! The Colorlight 5A-75B (chip `ecp5`) takes a compiled image, not a
//! `.wasm`: its gateware has no WASM compiler, only an installer
//! (design.md CL6). [`image_for_ecp5`] compiles the module on the host
//! with the gateware's reference model (`wasm-hdl-ref`, the same compiler
//! the gateware was tested against) and returns the image to upload.

use wasm_hdl_ref::image::EmitError;
use wasm_hdl_ref::{COLORLIGHT_MEMORY, Colorlight, LoadError};

use crate::plan::memory_misfit;

/// The chip string the 5A-75B reports in `sys/info`.
pub const CHIP: &str = "ecp5";

/// Whether `file` is a WebAssembly module (and so needs compiling for the
/// 5A-75B); anything else is uploaded as it is (an image already).
pub fn is_wasm(file: &[u8]) -> bool {
    file.starts_with(b"\0asm")
}

/// The image the 5A-75B installs for `wasm`, or why the module cannot run
/// there (the reason the gateware's loader used to log, now said before
/// anything is sent).
pub fn image_for_ecp5(wasm: &[u8]) -> Result<Vec<u8>, String> {
    if let Some(why) = memory_misfit(wasm, COLORLIGHT_MEMORY as u64) {
        return Err(why);
    }
    let mut core = Box::new(Colorlight::new());
    core.load(wasm).map_err(|e| {
        let what = match e {
            LoadError::Malformed => "not a well-formed module".to_string(),
            LoadError::Unsupported(w) => format!("outside the 5A-75B's subset: {w}"),
            LoadError::TooLarge(w) => format!("too large for the 5A-75B's core: {w}"),
            LoadError::UnknownImport => "an import that is not in the ABI".to_string(),
            LoadError::BadSignature => "an ABI import with the wrong signature".to_string(),
            LoadError::BadExport => "an export with the wrong signature".to_string(),
            LoadError::MissingMemory => "no exported memory".to_string(),
            LoadError::MemoryTooLarge { bytes, max } => {
                format!("initial memory {bytes} bytes, more than the device's {max}")
            }
        };
        format!("the 5A-75B cannot load it: {what}")
    })?;
    let mut out = vec![0u8; COLORLIGHT_MEMORY];
    match core.emit_image(&mut out) {
        Ok(n) => {
            out.truncate(n);
            Ok(out)
        }
        Err(EmitError::Full { need }) => Err(format!(
            "the compiled image is {need} bytes, more than the device's {COLORLIGHT_MEMORY}"
        )),
        Err(e) => Err(format!("cannot write the image: {e:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cl6_a_module_compiles_to_an_image_the_model_installs() {
        let wasm: &[u8] = &[
            0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // header
            0x05, 0x05, 0x01, 0x09, 0x10, 0x20, 0x00, // memory 16..32, 1-byte pages
            0x07, 0x0a, 0x01, 0x06, b'm', b'e', b'm', b'o', b'r', b'y', 0x02, 0x00,
        ];
        assert!(is_wasm(wasm));
        let img = image_for_ecp5(wasm).unwrap();
        assert!(!is_wasm(&img));
        assert_eq!(&img[..4], &wasm_hdl_ref::image::MAGIC);
        let mut c = Box::new(Colorlight::new());
        c.install(&img).unwrap();
        assert_eq!(c.memory().len(), 16);
    }

    #[test]
    fn cl6_a_module_the_core_refuses_is_refused_on_the_host() {
        let page64: &[u8] = &[
            0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x05, 0x03, 0x01, 0x00, 0x01, 0x07,
            0x0a, 0x01, 0x06, b'm', b'e', b'm', b'o', b'r', b'y', 0x02, 0x00,
        ];
        assert!(image_for_ecp5(page64).unwrap_err().contains("1-byte pages"));
        let no_memory: &[u8] = &[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
        assert!(
            image_for_ecp5(no_memory)
                .unwrap_err()
                .contains("no exported memory")
        );
        let cut: &[u8] = &[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x05, 0x09];
        assert!(image_for_ecp5(cut).unwrap_err().contains("well-formed"));
    }

    /// The hex string after `"<field>": "` in a golden vector file.
    fn field<'a>(text: &'a str, field: &str) -> &'a str {
        let tag = format!("\"{field}\": \"");
        let at = text.find(&tag).unwrap_or_else(|| panic!("no {field}")) + tag.len();
        &text[at..at + text[at..].find('"').unwrap()]
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// The bring-up payload (`bringup.sh upload`): `bridge.wat` as mwctl
    /// compiles it for a chip `ecp5` is byte for byte the image the
    /// gateware simulations install — `bridge.json`'s blob, which
    /// `tests/test_system.py` streams through the installer and runs, and
    /// the happy-path upload of `upload.json` (length and CRC-32 in its
    /// `sys/loader/begin`), which `tests/test_node.py` sends as frames on a
    /// link, commits and starts.
    #[test]
    fn cl9_bridge_wat_compiles_to_the_image_the_gateware_installs() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let wasm = wat::parse_file(root.join("payloads/templates/bridge.wat")).unwrap();
        assert!(is_wasm(&wasm));
        let img = image_for_ecp5(&wasm).unwrap();
        let vectors = root.join("gateware-5a75b/tests/vectors");
        let golden = std::fs::read_to_string(vectors.join("wasm/bridge.json")).unwrap();
        assert_eq!(field(&golden, "module"), hex(&wasm), "bridge.json is stale");
        assert_eq!(field(&golden, "blob"), hex(&img));
        let upload = std::fs::read_to_string(vectors.join("upload.json")).unwrap();
        let begin = upload.find("\"sys/loader/begin\"").unwrap();
        let mut body = (img.len() as u32).to_le_bytes().to_vec();
        body.extend(middleware::crc::Crc32::checksum(&img).to_le_bytes());
        assert_eq!(field(&upload[begin..], "body"), hex(&body));
        let mut c = Box::new(Colorlight::new());
        c.install(&img).unwrap();
        assert_eq!(c.memory().len(), 4096);
    }
}
