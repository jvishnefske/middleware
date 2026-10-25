//! End-to-end tests of the payload runtime with WAT payloads and a fake
//! device that records every host call.

use middleware::Key;
use wasm_host::{AbiError, Device, Limits, LoadError, Payload, Pull, Trap};

/// A pin the fake board reserves (like a flash or console pin).
const RESERVED_PIN: u8 = 13;

#[derive(Default)]
struct Fake {
    now: u32,
    logs: Vec<String>,
    published: Vec<(String, Vec<u8>)>,
    gpio: Vec<String>,
}

impl Fake {
    /// Payloads in these tests report import results as 4-byte LE values on
    /// the key `r`.
    fn results(&self) -> Vec<i32> {
        self.published
            .iter()
            .filter(|(k, _)| k == "r")
            .map(|(_, d)| i32::from_le_bytes(d.as_slice().try_into().expect("4 bytes")))
            .collect()
    }
}

impl Device for Fake {
    fn log(&mut self, line: &str) {
        self.logs.push(line.to_string());
    }
    fn publish(&mut self, key: Key<'_>, data: &[u8]) -> Result<(), AbiError> {
        self.published
            .push((key.as_str().to_string(), data.to_vec()));
        Ok(())
    }
    fn now_ms(&mut self) -> u32 {
        self.now
    }
    fn gpio_output(&mut self, pin: u8) -> Result<(), AbiError> {
        if pin == RESERVED_PIN {
            return Err(AbiError::Unsupported);
        }
        self.gpio.push(format!("out {pin}"));
        Ok(())
    }
    fn gpio_input(&mut self, pin: u8, pull: Pull) -> Result<(), AbiError> {
        self.gpio.push(format!("in {pin} {pull:?}"));
        Ok(())
    }
    fn gpio_write(&mut self, pin: u8, high: bool) -> Result<(), AbiError> {
        self.gpio.push(format!("write {pin} {high}"));
        Ok(())
    }
    fn gpio_read(&mut self, pin: u8) -> Result<bool, AbiError> {
        Ok(pin % 2 == 1)
    }
}

/// Imports and helpers shared by the WAT payloads. `$report` publishes an
/// i32 on key `r` (stored at offset 0; data scratch at 4).
const PRELUDE: &str = r#"
  (import "mw" "log" (func $log (param i32 i32)))
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "subscribe" (func $subscribe (param i32 i32) (result i32)))
  (import "mw" "now_ms" (func $now_ms (result i32)))
  (import "mw" "set_tick" (func $set_tick (param i32) (result i32)))
  (import "mw" "gpio_output" (func $gpio_output (param i32) (result i32)))
  (import "mw" "gpio_input" (func $gpio_input (param i32 i32) (result i32)))
  (import "mw" "gpio_write" (func $gpio_write (param i32 i32) (result i32)))
  (import "mw" "gpio_read" (func $gpio_read (param i32) (result i32)))
  (data (i32.const 0) "r")
  (func $report (param $v i32)
    (i32.store (i32.const 4) (local.get $v))
    (drop (call $publish (i32.const 0) (i32.const 1) (i32.const 4) (i32.const 4))))
"#;

fn module(body: &str) -> Vec<u8> {
    let src = format!("(module {PRELUDE} {body})");
    wat::parse_str(&src).expect("valid WAT")
}

fn load(body: &str) -> Result<Payload<Fake>, LoadError> {
    Payload::load(&module(body), Fake::default(), Limits::default())
}

fn loaded(body: &str) -> Payload<Fake> {
    load(body).expect("payload loads")
}

const MEM: &str = r#"(memory (export "memory") 1)"#;

// ---------------------------------------------------------------- W1

#[test]
fn w1_accepts_module_using_every_import() {
    let p = loaded(MEM);
    assert!(!p.is_poisoned());
    assert_eq!(p.next_tick(), None);
}

#[test]
fn w1_rejects_unknown_import() {
    let wasm =
        wat::parse_str(r#"(module (import "mw" "fopen" (func)) (memory (export "memory") 1))"#)
            .unwrap();
    let err = Payload::load(&wasm, Fake::default(), Limits::default()).err();
    assert_eq!(
        err,
        Some(LoadError::UnknownImport {
            module: "mw".into(),
            name: "fopen".into()
        })
    );

    let wasm = wat::parse_str(
        r#"(module (import "env" "log" (func (param i32 i32))) (memory (export "memory") 1))"#,
    )
    .unwrap();
    let err = Payload::load(&wasm, Fake::default(), Limits::default()).err();
    assert!(matches!(err, Some(LoadError::UnknownImport { .. })));

    let wasm = wat::parse_str(r#"(module (import "mw" "memory" (memory 1)))"#).unwrap();
    let err = Payload::load(&wasm, Fake::default(), Limits::default()).err();
    assert!(matches!(err, Some(LoadError::UnknownImport { .. })));
}

#[test]
fn w1_rejects_wrong_import_signature() {
    let wasm =
        wat::parse_str(r#"(module (import "mw" "publish" (func (param i32 i32 i32 i32))) (memory (export "memory") 1))"#)
            .unwrap();
    let err = Payload::load(&wasm, Fake::default(), Limits::default()).err();
    assert_eq!(
        err,
        Some(LoadError::BadSignature {
            name: "publish".into()
        })
    );

    let wasm = wat::parse_str(
        r#"(module (import "mw" "log" (func (param i64 i32))) (memory (export "memory") 1))"#,
    )
    .unwrap();
    let err = Payload::load(&wasm, Fake::default(), Limits::default()).err();
    assert_eq!(err, Some(LoadError::BadSignature { name: "log".into() }));
}

#[test]
fn w1_rejects_missing_memory() {
    let load_raw = |src: &str| {
        Payload::load(
            &wat::parse_str(src).unwrap(),
            Fake::default(),
            Limits::default(),
        )
        .err()
    };
    assert_eq!(load_raw("(module)"), Some(LoadError::MissingMemory));
    assert_eq!(
        load_raw(r#"(module (memory (export "heap") 1))"#),
        Some(LoadError::MissingMemory)
    );
}

#[test]
fn w1_rejects_bad_export_signature_and_garbage() {
    let err = load(&format!(r#"{MEM} (func (export "tick"))"#)).err();
    assert_eq!(
        err,
        Some(LoadError::BadExport {
            name: "tick".into()
        })
    );
    let err = Payload::load(b"\0asm junk", Fake::default(), Limits::default()).err();
    assert!(matches!(err, Some(LoadError::Invalid(_))));
}

// ---------------------------------------------------------------- W2

#[test]
fn w2_init_runs_once_on_load() {
    let mut p = loaded(&format!(
        r#"{MEM} (data (i32.const 16) "hello")
           (func (export "init") (call $log (i32.const 16) (i32.const 5)))"#
    ));
    assert_eq!(p.device().logs, vec!["hello"]);
}

#[test]
fn w2_tick_runs_on_requested_period() {
    let body = format!(
        r#"{MEM}
           (func (export "init") (call $report (call $set_tick (i32.const 100))))
           (func (export "tick") (param $now i32) (call $report (local.get $now)))"#
    );
    let fake = Fake {
        now: 1000,
        ..Fake::default()
    };
    let mut p = Payload::load(&module(&body), fake, Limits::default()).unwrap();
    assert_eq!(p.device().results(), vec![0]);
    assert_eq!(p.next_tick(), Some(1100));
    p.tick(1050).unwrap(); // not due
    assert_eq!(p.device().results(), vec![0]);
    p.tick(1100).unwrap();
    assert_eq!(p.next_tick(), Some(1200));
    p.tick(1230).unwrap(); // late: next stays on the grid
    assert_eq!(p.next_tick(), Some(1300));
    p.tick(2000).unwrap(); // far behind: missed ticks are skipped
    assert_eq!(p.next_tick(), Some(2100));
    assert_eq!(p.device().results(), vec![0, 1100, 1230, 2000]);
}

#[test]
fn w2_set_tick_zero_disables_and_tick_without_schedule_is_noop() {
    let body = format!(
        r#"{MEM}
           (func (export "init") (drop (call $set_tick (i32.const 50))))
           (func (export "tick") (param i32) (call $report (call $set_tick (i32.const 0))))"#
    );
    let mut p = loaded(&body);
    assert_eq!(p.next_tick(), Some(50));
    p.tick(50).unwrap();
    assert_eq!(p.next_tick(), None);
    p.tick(1000).unwrap();
    assert_eq!(p.device().results(), vec![0]);
}

/// Echo payload: subscribes to `in/a` and `in/b` and republishes each
/// message on `out` prefixed by its slot digit. `mw_alloc` hands out a
/// fixed buffer at 1024.
const ECHO: &str = r#"
  (memory (export "memory") 1)
  (data (i32.const 16) "in/a")
  (data (i32.const 24) "in/b")
  (data (i32.const 32) "out")
  (func (export "init")
    (call $report (call $subscribe (i32.const 16) (i32.const 4)))
    (call $report (call $subscribe (i32.const 24) (i32.const 4)))
    (call $report (call $subscribe (i32.const 16) (i32.const 4))))
  (func (export "mw_alloc") (param $len i32) (result i32) (i32.const 1024))
  (func (export "on_message") (param $slot i32) (param $ptr i32) (param $len i32)
    (i32.store8 (i32.const 1023) (i32.add (i32.const 48) (local.get $slot)))
    (drop (call $publish (i32.const 32) (i32.const 3)
                         (i32.const 1023) (i32.add (local.get $len) (i32.const 1)))))
"#;

#[test]
fn w2_deliver_runs_on_message_for_exact_subscribed_keys() {
    let mut p = loaded(ECHO);
    assert_eq!(p.device().results(), vec![0, 1, 0]);
    assert_eq!(p.subscriptions().collect::<Vec<_>>(), vec!["in/a", "in/b"]);
    assert_eq!(p.deliver(Key::new("in/b").unwrap(), b"hi"), Ok(true));
    assert_eq!(p.deliver(Key::new("in/a").unwrap(), b"yo"), Ok(true));
    assert_eq!(p.deliver(Key::new("in").unwrap(), b"x"), Ok(false));
    assert_eq!(p.deliver(Key::new("in/a/b").unwrap(), b"x"), Ok(false));
    assert_eq!(p.deliver(Key::new("in/a").unwrap(), &[0; 513]), Ok(false));
    let outs: Vec<_> = p
        .device()
        .published
        .iter()
        .filter(|(k, _)| k == "out")
        .map(|(_, d)| d.clone())
        .collect();
    assert_eq!(outs, vec![b"1hi".to_vec(), b"0yo".to_vec()]);
}

#[test]
fn w2_alloc_refusal_means_not_delivered() {
    // mw_alloc refuses with 0 for "ab", returns an out-of-memory buffer for
    // "abc", and accepts anything else; on_message logs what it got.
    let body = r#"
      (memory (export "memory") 1)
      (data (i32.const 16) "k")
      (func (export "init") (drop (call $subscribe (i32.const 16) (i32.const 1))))
      (func (export "mw_alloc") (param $len i32) (result i32)
        (if (result i32) (i32.eq (local.get $len) (i32.const 2))
          (then (i32.const 0))
          (else (if (result i32) (i32.eq (local.get $len) (i32.const 3))
            (then (i32.const 65535))
            (else (i32.const 1024))))))
      (func (export "on_message") (param i32) (param $p i32) (param $n i32)
        (call $log (local.get $p) (local.get $n)))"#;
    let mut p = loaded(body);
    assert_eq!(p.deliver(Key::new("k").unwrap(), b"ab"), Ok(false));
    assert_eq!(p.deliver(Key::new("k").unwrap(), b"abc"), Ok(false));
    assert!(!p.is_poisoned());
    assert_eq!(p.deliver(Key::new("k").unwrap(), b"abcd"), Ok(true));
    assert_eq!(p.device().logs, vec!["abcd"]);
}

#[test]
fn w1_extra_exports_and_start_function_are_accepted() {
    // Rust guests export `__data_end`/`__heap_base`; AssemblyScript runs a
    // start function during instantiation.
    let body = format!(
        r#"{MEM}
           (global $g (mut i32) (i32.const 0))
           (global (export "__data_end") i32 (i32.const 1024))
           (global (export "__heap_base") i32 (i32.const 2048))
           (func $start (global.set $g (i32.const 7)))
           (start $start)
           (func (export "helper") (param i64) (result f32) (f32.const 0))
           (func (export "init") (call $report (global.get $g)))"#
    );
    let mut p = loaded(&body);
    assert_eq!(p.device().results(), vec![7]);
}

// ---------------------------------------------------------------- W3

#[test]
fn w3_imports_return_error_codes_instead_of_trapping() {
    let body = format!(
        r#"{MEM}
        (data (i32.const 16) "sys/log")
        (data (i32.const 32) "bad//key")
        (data (i32.const 48) "ok/key")
        (func (export "init")
          ;; log with an out-of-bounds pointer is dropped, not a trap
          (call $log (i32.const 65530) (i32.const 100))
          (call $report (call $publish (i32.const 65530) (i32.const 10) (i32.const 0) (i32.const 1)))
          (call $report (call $publish (i32.const 48) (i32.const 6) (i32.const -1) (i32.const 1)))
          (call $report (call $publish (i32.const 16) (i32.const 7) (i32.const 0) (i32.const 1)))
          (call $report (call $publish (i32.const 32) (i32.const 8) (i32.const 0) (i32.const 1)))
          (call $report (call $publish (i32.const 48) (i32.const 6) (i32.const 0) (i32.const 513)))
          (call $report (call $publish (i32.const 48) (i32.const 6) (i32.const 0) (i32.const 512)))
          (call $report (call $subscribe (i32.const 16) (i32.const 7)))
          (call $report (call $subscribe (i32.const 48) (i32.const 6)))
          (call $report (call $gpio_output (i32.const 300)))
          (call $report (call $gpio_output (i32.const {RESERVED_PIN})))
          (call $report (call $gpio_output (i32.const 2)))
          (call $report (call $gpio_write (i32.const 2) (i32.const 5)))
          (call $report (call $gpio_write (i32.const 2) (i32.const 1)))
          (call $report (call $gpio_input (i32.const 3) (i32.const 7)))
          (call $report (call $gpio_input (i32.const 3) (i32.const 1)))
          (call $report (call $gpio_read (i32.const 3)))
          (call $report (call $gpio_read (i32.const -3)))
          (call $report (call $set_tick (i32.const 5)))
          (call $report (call $set_tick (i32.const 100)))
          (call $report (call $now_ms)))"#
    );
    let fake = Fake {
        now: 77,
        ..Fake::default()
    };
    let mut p = Payload::load(&module(&body), fake, Limits::default()).unwrap();
    use AbiError::*;
    let expect = vec![
        OutOfBounds.code(),
        OutOfBounds.code(),
        InvalidKey.code(),
        InvalidKey.code(),
        InvalidArgument.code(),
        0,
        InvalidKey.code(),
        Unsupported.code(), // no mw_alloc/on_message exports
        InvalidArgument.code(),
        Unsupported.code(),
        0,
        InvalidArgument.code(),
        0,
        InvalidArgument.code(),
        0,
        1,
        InvalidArgument.code(),
        InvalidArgument.code(),
        Unsupported.code(), // no tick export
        77,
    ];
    assert_eq!(p.device().results(), expect);
    assert!(p.device().logs.is_empty());
    assert_eq!(p.device().gpio, vec!["out 2", "write 2 true", "in 3 Up"]);
    assert!(!p.is_poisoned());
}

#[test]
fn w3_subscription_limit_returns_no_resources() {
    // Subscribes to k0..k8 (nine keys); the ninth must fail.
    let body = r#"
      (memory (export "memory") 1)
      (data (i32.const 16) "k0")
      (func (export "init") (local $i i32)
        (loop $next
          (i32.store8 (i32.const 17) (i32.add (i32.const 48) (local.get $i)))
          (call $report (call $subscribe (i32.const 16) (i32.const 2)))
          (local.set $i (i32.add (local.get $i) (i32.const 1)))
          (br_if $next (i32.lt_u (local.get $i) (i32.const 9)))))
      (func (export "mw_alloc") (param i32) (result i32) (i32.const 1024))
      (func (export "on_message") (param i32 i32 i32))"#;
    let mut p = loaded(body);
    assert_eq!(
        p.device().results(),
        vec![0, 1, 2, 3, 4, 5, 6, 7, AbiError::NoResources.code()]
    );
    assert_eq!(
        p.subscriptions().count(),
        middleware::wasm_abi::MAX_SUBSCRIPTIONS
    );
}

#[test]
fn w3_log_truncates_at_max_log() {
    let body = format!(
        r#"{MEM} (func (export "init") (call $log (i32.const 4096) (i32.const 1000)))
           (data (i32.const 4096) "{}")"#,
        "x".repeat(1000)
    );
    let mut p = loaded(&body);
    assert_eq!(p.device().logs[0].len(), middleware::wasm_abi::MAX_LOG);
}

// ---------------------------------------------------------------- W4

#[test]
fn w4_infinite_loop_runs_out_of_fuel_and_poisons_payload() {
    let body = format!(
        r#"{MEM} (data (i32.const 16) "k")
           (func (export "init")
             (drop (call $set_tick (i32.const 10)))
             (drop (call $subscribe (i32.const 16) (i32.const 1))))
           (func (export "tick") (param i32) (loop $forever (br $forever)))
           (func (export "mw_alloc") (param i32) (result i32) (i32.const 1024))
           (func (export "on_message") (param i32 i32 i32))"#
    );
    let mut p = loaded(&body);
    assert_eq!(p.tick(10), Err(Trap::OutOfFuel));
    assert!(p.is_poisoned());
    assert_eq!(p.next_tick(), None);
    assert_eq!(p.tick(20), Err(Trap::Poisoned));
    assert_eq!(p.deliver(Key::new("k").unwrap(), b"x"), Err(Trap::Poisoned));
}

#[test]
fn w4_infinite_loop_in_init_fails_load() {
    let err = load(&format!(
        r#"{MEM} (func (export "init") (loop $l (br $l)))"#
    ))
    .err();
    assert_eq!(err, Some(LoadError::Init(Trap::OutOfFuel)));
}

#[test]
fn w4_fuel_is_refilled_before_every_call() {
    // Each tick burns a large share of a small budget; without refuelling
    // the third call would run dry.
    let body = format!(
        r#"{MEM}
           (func (export "init") (drop (call $set_tick (i32.const 10))))
           (func (export "tick") (param i32) (local $i i32)
             (loop $l
               (local.set $i (i32.add (local.get $i) (i32.const 1)))
               (br_if $l (i32.lt_u (local.get $i) (i32.const 1000)))))"#
    );
    let mut fuel_needed = None;
    for fuel in [1_000u64, 2_000, 4_000, 8_000, 16_000] {
        let limits = Limits {
            fuel_per_call: fuel,
            ..Limits::default()
        };
        let mut p = Payload::load(&module(&body), Fake::default(), limits).unwrap();
        if p.tick(10).is_ok() {
            fuel_needed = Some(fuel);
            for t in 1..20 {
                assert_eq!(p.tick(10 + 10 * t), Ok(()), "call {t} with fuel {fuel}");
            }
            break;
        }
        assert!(p.is_poisoned());
    }
    // Fails at 1000 fuel (the loop is >1000 instructions), so 20 calls
    // succeeding means fuel did not accumulate across calls.
    assert!(matches!(fuel_needed, Some(f) if f > 1_000));
}

#[test]
fn w4_other_traps_are_reported() {
    let err = load(&format!(r#"{MEM} (func (export "init") unreachable)"#)).err();
    assert!(matches!(err, Some(LoadError::Init(Trap::Wasm(_)))));
}

// ---------------------------------------------------------------- W5

const KIB64: u32 = 65_536;

#[test]
fn w5_limits_constructors_are_byte_based() {
    assert_eq!(Limits::pages(2).max_memory_bytes, 2 * KIB64);
    assert_eq!(Limits::bytes(12_288).max_memory_bytes, 12_288);
    assert_eq!(Limits::pages(u32::MAX).max_memory_bytes, u32::MAX);
    assert_eq!(Limits::default(), Limits::pages(2));
    assert_eq!(
        Limits::default().fuel_per_call,
        middleware::wasm_abi::FUEL_PER_CALL
    );
    assert_eq!(Limits::bytes(1).with_fuel(7).fuel_per_call, 7);
}

#[test]
fn w5_memory_grow_beyond_cap_fails() {
    let body = r#"
      (memory (export "memory") 1)
      (func (export "init")
        (call $report (memory.grow (i32.const 1)))
        (call $report (memory.grow (i32.const 1)))
        (call $report (memory.size)))"#;
    let mut p = loaded(body);
    assert_eq!(p.device().results(), vec![1, -1, 2]);

    let mut p = Payload::load(&module(body), Fake::default(), Limits::pages(4)).unwrap();
    assert_eq!(p.device().results(), vec![1, 2, 3]);

    // A byte cap between page multiples rounds down: 2.5 pages allows 2.
    let limits = Limits::bytes(2 * KIB64 + KIB64 / 2);
    let mut p = Payload::load(&module(body), Fake::default(), limits).unwrap();
    assert_eq!(p.device().results(), vec![1, -1, 2]);
}

#[test]
fn w5_initial_memory_over_cap_is_rejected() {
    let err = load(r#"(memory (export "memory") 3)"#).err();
    assert_eq!(
        err,
        Some(LoadError::MemoryTooLarge {
            bytes: 3 * u64::from(KIB64),
            max: 2 * KIB64
        })
    );
    assert!(
        Payload::load(
            &module(r#"(memory (export "memory") 3)"#),
            Fake::default(),
            Limits::pages(3)
        )
        .is_ok()
    );
}

/// A module with 1-byte pages: `bytes` of initial memory. The prelude's data
/// segment and `$report` scratch sit in the first 8 bytes.
fn byte_page_module(bytes: u32, init: &str) -> Vec<u8> {
    module(&format!(
        r#"(memory (export "memory") {bytes} (pagesize 1)) (func (export "init") {init})"#
    ))
}

#[test]
fn w5_one_byte_page_module_within_cap_loads_and_grows_to_it() {
    // 12 KiB initial under a 16 KiB cap: grows by bytes, up to the cap.
    let wasm = byte_page_module(
        12_288,
        r#"(call $report (memory.size))
           (call $report (memory.grow (i32.const 4096)))
           (call $report (memory.grow (i32.const 1)))
           (call $report (memory.size))"#,
    );
    let mut p = Payload::load(&wasm, Fake::default(), Limits::bytes(16_384)).unwrap();
    assert_eq!(p.device().results(), vec![12_288, 12_288, -1, 16_384]);
    // Exactly at the cap is accepted.
    assert!(
        Payload::load(
            &byte_page_module(16_384, ""),
            Fake::default(),
            Limits::bytes(16_384)
        )
        .is_ok()
    );
}

#[test]
fn w5_one_byte_page_module_over_cap_is_rejected() {
    let err = Payload::load(
        &byte_page_module(16_385, ""),
        Fake::default(),
        Limits::bytes(16_384),
    )
    .err();
    assert_eq!(
        err,
        Some(LoadError::MemoryTooLarge {
            bytes: 16_385,
            max: 16_384
        })
    );
    // The same module is far below a one-page cap, which a 64 KiB-page
    // module could not get under with any non-empty memory.
    assert!(
        Payload::load(
            &byte_page_module(16_385, ""),
            Fake::default(),
            Limits::pages(1)
        )
        .is_ok()
    );
}

#[test]
fn into_device_returns_state() {
    let p = loaded(ECHO);
    assert_eq!(p.into_device().results(), vec![0, 1, 0]);
}
