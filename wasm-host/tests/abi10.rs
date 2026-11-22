//! End-to-end tests of the ABI 10 `manifest` import with a WAT payload: the
//! payload's `init` asks for the node's manifest into a buffer at 256 with
//! a given capacity, reports the result on `r` and publishes the 64 bytes at
//! 256 on `m`. A capacity that fits gives the manifest's bytes; a short one
//! is refused and leaves the buffer untouched; a device without a manifest
//! answers `Unsupported`.

use middleware::Key;
use middleware::SenderId;
use middleware::loader::manifest::Manifest;
use middleware::wasm_abi::{ABI_VERSION, import, import_in};
use wasm_host::{AbiError, Device, Limits, Payload, Pull};

const NODE: SenderId = SenderId([0x24, 0x6f, 0x28, 0xaa, 0x01, 0xfe, 0, 0]);

#[derive(Default)]
struct Board {
    manifest: Option<Vec<u8>>,
    published: Vec<(String, Vec<u8>)>,
}

impl Board {
    fn with_manifest() -> Self {
        let m = Manifest::new(ABI_VERSION).with_node(NODE);
        let mut out = [0u8; middleware::loader::manifest::MANIFEST_MAX];
        let n = m.encode(&mut out).expect("fits");
        Board {
            manifest: Some(out[..n].to_vec()),
            published: Vec::new(),
        }
    }
    fn word(&self, key: &str) -> i32 {
        let d = self.bytes(key);
        i32::from_le_bytes(d.try_into().expect("4 bytes"))
    }
    fn bytes(&self, key: &str) -> Vec<u8> {
        self.published
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, d)| d.clone())
            .expect("published")
    }
}

impl Device for Board {
    fn log(&mut self, _: &str) {}
    fn publish(&mut self, key: Key<'_>, data: &[u8]) -> Result<(), AbiError> {
        self.published
            .push((key.as_str().to_string(), data.to_vec()));
        Ok(())
    }
    fn now_ms(&mut self) -> u32 {
        0
    }
    fn gpio_output(&mut self, _: u8) -> Result<(), AbiError> {
        Ok(())
    }
    fn gpio_input(&mut self, _: u8, _: Pull) -> Result<(), AbiError> {
        Ok(())
    }
    fn gpio_write(&mut self, _: u8, _: bool) -> Result<(), AbiError> {
        Ok(())
    }
    fn gpio_read(&mut self, _: u8) -> Result<bool, AbiError> {
        Ok(false)
    }
    fn manifest(&mut self, out: &mut [u8]) -> Result<usize, AbiError> {
        let m = self.manifest.as_ref().ok_or(AbiError::Unsupported)?;
        out.get_mut(..m.len())
            .ok_or(AbiError::NoResources)?
            .copy_from_slice(m);
        Ok(m.len())
    }
}

/// A payload whose `init` calls `manifest(256, cap)`, publishes the result
/// on `r` and the 64 bytes at 256 on `m`. The buffer starts zeroed.
fn module(cap: i32) -> Vec<u8> {
    let src = format!(
        r#"(module
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "manifest" (func $manifest (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 64) "r")
  (data (i32.const 96) "m")
  (func (export "init")
    (i32.store (i32.const 80) (call $manifest (i32.const 256) (i32.const {cap})))
    (drop (call $publish (i32.const 64) (i32.const 1) (i32.const 80) (i32.const 4)))
    (drop (call $publish (i32.const 96) (i32.const 1) (i32.const 256) (i32.const 64)))))"#
    );
    wat::parse_str(src).expect("WAT")
}

fn run(cap: i32, board: Board) -> Board {
    let wasm = module(cap);
    let p = Payload::load(&wasm, board, Limits::default()).expect("loads");
    p.into_device()
}

#[test]
fn abi10_the_import_is_in_the_table() {
    assert_eq!(ABI_VERSION, 10);
    assert_eq!(
        import("manifest").map(|i| (i.params, i.returns)),
        Some((2, true))
    );
    assert_eq!(import_in(9, "manifest"), None);
    assert!(import_in(10, "manifest").is_some());
}

#[test]
fn abi10_a_cap_that_fits_gets_the_sys_manifest_bytes() {
    let expected = Board::with_manifest().manifest.expect("manifest");
    let b = run(64, Board::with_manifest());
    assert_eq!(b.word("r"), expected.len() as i32);
    let got = b.bytes("m");
    assert_eq!(&got[..expected.len()], &expected[..]);
    assert_eq!(
        Manifest::decode(&expected).expect("decodes").node,
        Some(NODE)
    );
}

#[test]
fn abi10_a_short_cap_is_refused_and_writes_nothing() {
    let b = run(8, Board::with_manifest());
    assert_eq!(b.word("r"), AbiError::InvalidArgument.code());
    assert!(b.bytes("m").iter().all(|&x| x == 0));
}

#[test]
fn abi10_a_device_without_a_manifest_answers_unsupported() {
    let b = run(64, Board::default());
    assert_eq!(b.word("r"), AbiError::Unsupported.code());
    assert!(b.bytes("m").iter().all(|&x| x == 0));
}
