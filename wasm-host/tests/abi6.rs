//! End-to-end tests of the ABI 6 bridge import (`route_bridge`) with WAT
//! payloads: the rule reaches the device with its links and filter, the
//! handle works with `route_stop` and `route_stat`, arguments are checked
//! links-then-pointers-then-key, a single-link board answers
//! `Unsupported`, ABI 1-5 payloads load unchanged, and an ABI 5 host does
//! not know the import.

use middleware::wasm_abi::bridge::{BRIDGE_BASE, Bridge, BridgeTable};
use middleware::wasm_abi::route::RouteStat;
use middleware::wasm_abi::{ABI5_IMPORTS, ABI6_IMPORTS, IMPORTS, import_in};
use middleware::{Filter, Key};
use wasm_host::{AbiError, Device, Limits, LoadError, Payload, Pull};

/// A two-link board keeping a real [`BridgeTable`].
#[derive(Default)]
struct TwoPort {
    table: BridgeTable,
    published: Vec<(String, Vec<u8>)>,
    logs: Vec<String>,
}

impl TwoPort {
    fn results(&self) -> Vec<i32> {
        self.published
            .iter()
            .filter(|(k, _)| k == "r")
            .map(|(_, d)| i32::from_le_bytes(d.as_slice().try_into().expect("4 bytes")))
            .collect()
    }
}

impl Device for TwoPort {
    fn log(&mut self, line: &str) {
        self.logs.push(line.to_string());
    }
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
    fn route_bridge(&mut self, b: &Bridge<'_>) -> Result<u8, AbiError> {
        if b.src > 1 || b.dst > 1 {
            return Err(AbiError::Unsupported);
        }
        self.table.add(b)
    }
    fn route_stop(&mut self, handle: u8) -> Result<(), AbiError> {
        self.table.remove(handle)
    }
    fn route_stat(&mut self, handle: u8, stat: RouteStat) -> Result<u32, AbiError> {
        let r = self.table.get(handle).ok_or(AbiError::Busy)?;
        Ok(match stat {
            RouteStat::Frames | RouteStat::Chunks => r.stats.forwarded,
            RouteStat::FramesDropped | RouteStat::ChunksDropped => r.stats.dropped,
        })
    }
}

/// A module whose `init` runs `body`; `$r` publishes an i32 on `r`. Data:
/// `demo/**` at 32 (7 bytes), `sys/**` at 48 (6), `back/ack` at 64 (8).
fn module(body: &str) -> Vec<u8> {
    wat::parse_str(format!(
        r#"(module
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "route_bridge" (func $bridge (param i32 i32 i32 i32) (result i32)))
  (import "mw" "route_stop" (func $stop (param i32) (result i32)))
  (import "mw" "route_stat" (func $stat (param i32 i32) (result i32)))
  (memory (export "memory") 4096 4096 (pagesize 1))
  (data (i32.const 16) "r")
  (data (i32.const 32) "demo/**")
  (data (i32.const 48) "sys/**")
  (data (i32.const 64) "back/ack")
  (func $r (param $v i32)
    (i32.store (i32.const 96) (local.get $v))
    (drop (call $publish (i32.const 16) (i32.const 1) (i32.const 96) (i32.const 4))))
  (func (export "init") {body}))"#
    ))
    .expect("WAT")
}

#[test]
fn cl2_route_bridge_installs_rules_and_the_handle_drives_stop_and_stat() {
    let wasm = module(
        r#"
    (call $r (call $bridge (i32.const 0) (i32.const 32) (i32.const 7) (i32.const 1)))
    (call $r (call $bridge (i32.const 1) (i32.const 64) (i32.const 8) (i32.const 0)))
    (call $r (call $bridge (i32.const 0) (i32.const 32) (i32.const 7) (i32.const 1)))
    (call $r (call $stat (i32.const 16) (i32.const 0)))
    (call $r (call $stop (i32.const 17)))
    (call $r (call $stop (i32.const 17)))
    (call $r (call $stat (i32.const 17) (i32.const 0)))"#,
    );
    let p = Payload::load(&wasm, TwoPort::default(), Limits::default()).expect("loads");
    let mut p = p;
    let base = i32::from(BRIDGE_BASE);
    let busy = AbiError::Busy.code();
    assert_eq!(
        p.device().results(),
        [base, base + 1, base, 0, 0, busy, busy]
    );
    let rules: Vec<_> = p
        .device()
        .table
        .iter()
        .map(|(h, r)| (h, r.src, r.filter.as_filter(), r.dst))
        .collect();
    assert_eq!(
        rules,
        [(BRIDGE_BASE, 0, Filter::Prefix(Key::from_static("demo")), 1)]
    );
}

#[test]
fn cl2_arguments_are_checked_links_then_pointers_then_key() {
    let wasm = module(
        r#"
    (call $r (call $bridge (i32.const 0) (i32.const 32) (i32.const 7) (i32.const 0)))
    (call $r (call $bridge (i32.const 9) (i32.const 32) (i32.const 7) (i32.const 0)))
    (call $r (call $bridge (i32.const 0) (i32.const 4095) (i32.const 7) (i32.const 1)))
    (call $r (call $bridge (i32.const 0) (i32.const 48) (i32.const 6) (i32.const 1)))
    (call $r (call $bridge (i32.const 0) (i32.const 32) (i32.const 6) (i32.const 1)))
    (call $r (call $bridge (i32.const 0) (i32.const 32) (i32.const 7) (i32.const 3)))
    (call $r (call $stat (i32.const 40) (i32.const 0)))"#,
    );
    let mut p = Payload::load(&wasm, TwoPort::default(), Limits::default()).expect("loads");
    let (arg, oob, key, uns) = (
        AbiError::InvalidArgument.code(),
        AbiError::OutOfBounds.code(),
        AbiError::InvalidKey.code(),
        AbiError::Unsupported.code(),
    );
    // "demo/*" is a malformed key (no `*` outside the suffix); link 3 is
    // valid ABI-wise but the board has two.
    assert_eq!(p.device().results(), [arg, arg, oob, key, key, uns, arg]);
    assert_eq!(p.device().table.iter().count(), 0);
}

#[test]
fn cl2_a_single_link_board_answers_unsupported() {
    let wasm = module(
        r#"(call $r (call $bridge (i32.const 0) (i32.const 32) (i32.const 7) (i32.const 1)))
           (call $r (call $stop (i32.const 16)))"#,
    );
    // A board with the required methods only; it records what `$r` publishes.
    struct Probe(Vec<i32>);
    impl Device for Probe {
        fn log(&mut self, _: &str) {}
        fn publish(&mut self, _: Key<'_>, d: &[u8]) -> Result<(), AbiError> {
            self.0
                .push(i32::from_le_bytes(d.try_into().expect("4 bytes")));
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
    }
    let mut p = Payload::load(&wasm, Probe(Vec::new()), Limits::default()).expect("loads");
    let u = AbiError::Unsupported.code();
    assert_eq!(p.device().0, [u, u]);
}

#[test]
fn cl2_the_bridge_template_loads_and_installs_both_rules() {
    let wasm = wat::parse_str(include_str!("../../payloads/templates/bridge.wat")).expect("WAT");
    let limits = Limits::bytes(16 * 1024);
    let mut p = Payload::load(&wasm, TwoPort::default(), limits).expect("loads in 16 KiB");
    assert_eq!(p.device().logs, ["bridge: up"]);
    assert_eq!(p.device().table.iter().count(), 2);
    assert_eq!(p.subscriptions().collect::<Vec<_>>(), ["ctl/bridge"]);
    // Stop the demo rule from ctl/bridge, then tick: the count is Busy.
    assert_eq!(p.deliver(Key::from_static("ctl/bridge"), b"0"), Ok(true));
    assert_eq!(p.device().table.iter().count(), 1);
    p.tick(1000).expect("tick");
    let last = p.device().published.last().cloned().expect("stats");
    assert_eq!(last.0, "bridge/stats");
    assert_eq!(last.1, AbiError::Busy.code().to_le_bytes());
    // Re-install: the freed slot comes back with the same handle.
    assert_eq!(p.deliver(Key::from_static("ctl/bridge"), b"1"), Ok(true));
    assert_eq!(p.device().table.iter().count(), 2);
    p.tick(2000).expect("tick");
    assert_eq!(
        p.device().published.last().expect("stats").1,
        0i32.to_le_bytes()
    );
}

#[test]
fn cl2_abi5_hosts_do_not_know_route_bridge_and_older_payloads_still_load() {
    assert_eq!(import_in(5, "route_bridge"), None);
    assert_eq!(ABI6_IMPORTS, ABI5_IMPORTS + 1);
    assert_eq!(IMPORTS[ABI6_IMPORTS - 1].name, "route_bridge");
    // An ABI 1 payload (log/publish/subscribe, 64 KiB page) and an ABI 5
    // one load on this host unchanged.
    for wat in [
        include_str!("../../payloads/templates/echo.wat"),
        include_str!("../../payloads/templates/blink.wat"),
        include_str!("../../payloads/templates/timer.wat"),
        r#"(module (import "mw" "serve" (func (param i32 i32) (result i32)))
                   (memory (export "memory") 1))"#,
    ] {
        let wasm = wat::parse_str(wat).expect("WAT");
        assert!(
            Payload::load(&wasm, TwoPort::default(), Limits::default()).is_ok(),
            "{wat:.60}"
        );
    }
    // The wrong signature is refused.
    let bad = wat::parse_str(
        r#"(module (import "mw" "route_bridge" (func (param i32 i32 i32) (result i32)))
                   (memory (export "memory") 1))"#,
    )
    .expect("WAT");
    assert!(matches!(
        Payload::load(&bad, TwoPort::default(), Limits::default()),
        Err(LoadError::BadSignature { .. })
    ));
}
