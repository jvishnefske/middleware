//! The web probe's wire fixtures (`web/test/fixtures/kd240*.wire.json`):
//! what a KD240's posix-node answers the browser's probe over Zenoh,
//! recorded from the real controller. The browser's golden test decodes
//! them, so a change to `sys/info`, the manifest or `sys/manifest/cm` fails
//! here until the fixtures are blessed (`MW_BLESS_FIXTURES=1`), and the
//! web's golden plant then shows what the change does to a probe. (U50)
//!
//! The facts are fixed (no device tree, `CHIP` or crate version), so the
//! fixtures are the same on every machine.

use std::fmt::Write as _;

use super::*;
use crate::driver::sim::{SimConfig, SimDriver};
use crate::driver::{Driver, PointSpec, control_summary, serve_page};
use middleware::SenderId;
use middleware::loader::control::{Flow, Unit};
use middleware::loader::manifest::BoardName;
use middleware::wasm_abi::channels::DataKind;

/// The fixture node's id.
const NODE_ID: SenderId = SenderId(*b"KD240\0\0\x01");
/// The browser, as the requester in the query attachment.
const BROWSER: Option<SenderId> = Some(SenderId([0xb5; 8]));
const BOARD: &str = "ZynqMP KD240 revA/B/1";
const FIRMWARE: &str = "0.0.0-fixture";
const CHIP: &str = "linux-aarch64";
const GENERATOR: &str = "MW_BLESS_FIXTURES=1 cargo test -p mw-posix-node --lib web_fixtures";

fn fixture_facts(control: Option<middleware::loader::manifest::ControlModules>) -> Facts {
    Facts { chip: CHIP, firmware: FIRMWARE, node: NODE_ID, board: BoardName::new(BOARD), control }
}

fn hex(b: &[u8]) -> String {
    b.iter().fold(String::new(), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

/// The fixture file: keys in a fixed order, two-space indents.
fn render(info: &[u8], manifest: &[u8], pages: &[Vec<u8>]) -> String {
    let pages: Vec<String> = pages.iter().map(|p| format!("      \"{}\"", hex(p))).collect();
    format!(
        "{{\n  \"format\": \"mw-probe-fixture\",\n  \"version\": 1,\n  \"generator\": \"{GENERATOR}\",\n  \"facts\": {{\n    \"chip\": \"{CHIP}\",\n    \"firmware\": \"{FIRMWARE}\",\n    \"node\": \"{node}\",\n    \"board\": \"{BOARD}\"\n  }},\n  \"responses\": {{\n    \"sys/info\": \"{info}\",\n    \"sys/manifest\": \"{manifest}\",\n    \"sys/manifest/cm\": [\n{pages}\n    ]\n  }}\n}}\n",
        node = hex(&NODE_ID.0),
        info = hex(info),
        manifest = hex(manifest),
        pages = pages.join(",\n"),
    )
}

/// Writes `want` to the fixture `name` with `MW_BLESS_FIXTURES=1`, else
/// asserts the checked-in file is `want`.
fn bless_or_check(name: &str, want: &str) {
    let path = format!("{}/../web/test/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    if std::env::var("MW_BLESS_FIXTURES").as_deref() == Ok("1") {
        std::fs::write(&path, want).expect("write fixture");
        return;
    }
    let have = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}; run {GENERATOR}"));
    assert!(have == want, "{path} is out of date; run {GENERATOR} and review the diff");
}

fn open_with(disk: &mut Disk, facts: Facts, driver: Box<dyn Driver>) -> Ctl<'_> {
    let flash = SectorFlash::open(MemSectors::new(disk), SECTORS).expect("flash");
    let (mut c, boot) =
        Controller::open_with_driver(flash, facts, crate::limits(1024 * 1024), Manual::default(), driver);
    c.boot(&boot);
    c.drain();
    c
}

/// What the browser asks over Zenoh, checked against the same request on
/// the local link.
fn probe(c: &mut Ctl<'_>, key: &str, body: &[u8]) -> Result<Vec<u8>, HandlerError> {
    let zenoh = c.mesh_request(Key::new(key).expect("key"), BROWSER, body);
    assert_eq!(zenoh, req(c, key, body), "{key} over Zenoh and the local link");
    zenoh
}

/// Every page from 0 until NotFound, as the probe reads them.
fn pages(c: &mut Ctl<'_>) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for p in 0..=u8::MAX {
        match probe(c, "sys/manifest/cm", &[p]) {
            Ok(page) => out.push(page),
            Err(HandlerError::NotFound) => break,
            Err(e) => panic!("page {p}: {e:?}"),
        }
    }
    out
}

#[test]
fn u50_the_kd240_probe_fixture_is_what_the_simulated_lathe_answers() {
    let driver = SimDriver::new(SimConfig::pc_turn_50());
    let control = control_summary(driver.points());
    assert_eq!(control.map(|c| (c.points, c.pages)), Some((10, 1)));
    let mut disk = Disk::default();
    let mut c = open_with(&mut disk, fixture_facts(control), Box::new(driver));
    let info = probe(&mut c, "sys/info", &[]).expect("info");
    let manifest = probe(&mut c, "sys/manifest", &[]).expect("manifest");
    let pages = pages(&mut c);
    assert_eq!(pages.len(), 1);
    // The probe asks for page 0 as [0]; the empty body means page 0 too.
    assert_eq!(probe(&mut c, "sys/manifest/cm", &[]).as_ref(), Ok(&pages[0]));
    bless_or_check("kd240.wire.json", &render(&info, &manifest, &pages));
}

#[test]
fn u50_the_paged_fixture_lists_sixty_four_points_over_several_pages() {
    let points: Vec<PointSpec> = (0..64)
        .map(|i| PointSpec::new(&format!("point_with_long_name_{i:02}"), DataKind::Digital, Flow::In, Unit::Bool, 0, 1))
        .collect();
    let control = control_summary(&points).expect("64 points encode");
    assert!(control.pages >= 2, "{control:?}");
    let mut disk = Disk::default();
    let mut c = open_with(&mut disk, fixture_facts(Some(control)), Box::new(crate::driver::NoDriver));
    let info = probe(&mut c, "sys/info", &[]).expect("info");
    let manifest = probe(&mut c, "sys/manifest", &[]).expect("manifest");
    let pages: Vec<Vec<u8>> = (0..control.pages)
        .map(|p| {
            let mut out = [0u8; middleware::loader::control::PAGE_MAX];
            let n = serve_page(&points, &[p], &mut out).expect("page");
            out[..n].to_vec()
        })
        .collect();
    let mut past = [0u8; middleware::loader::control::PAGE_MAX];
    assert_eq!(serve_page(&points, &[control.pages], &mut past), Err(HandlerError::NotFound));
    bless_or_check("kd240-paged.wire.json", &render(&info, &manifest, &pages));
}
