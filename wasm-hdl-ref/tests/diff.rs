//! Differential tests (CL3): every payload runs through the reference
//! model and through wasmi (`wasm-host`) against the same recording board
//! and the same event script; the board calls (in order), every result the
//! host sees, and the final linear memory must match.
//!
//! Payloads: the WAT templates (`payloads/templates/*.wat`, `bridge.wat`
//! included), the generated phase payloads
//! (`tools/payload-sim/tests/fixtures/phase`), hand-written modules for
//! the opcodes the payloads rarely reach, and the Rust SDK examples when
//! `payloads/target/wasm32-unknown-unknown/release` holds a build.

mod common;

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use common::{Model, Rec, Step, script};
use middleware::{Key, RequestMeta};
use wasm_hdl_ref::{LoadError, Ticked};
use wasm_host::{Limits, Payload};

#[derive(Debug, PartialEq)]
struct Outcome {
    trace: Vec<String>,
    results: Vec<String>,
    memory: Vec<u8>,
}

fn run_wasmi(wasm: &[u8], steps: &[Step]) -> Result<Outcome, String> {
    let clock = Rc::new(Cell::new(0));
    let mut p = Payload::load(wasm, Rec::new(clock.clone()), Limits::default())
        .map_err(|e| format!("{e}"))?;
    let mut results = Vec::new();
    for st in steps {
        match st {
            Step::Tick(t) => {
                clock.set(*t);
                results.push(match p.tick(*t) {
                    Ok(wasm_host::Ticked::Ran) => "ran".into(),
                    Ok(wasm_host::Ticked::NotDue) => "notdue".into(),
                    Ok(wasm_host::Ticked::Disabled) => "disabled".into(),
                    Ok(wasm_host::Ticked::NoTickExport) => "notick".into(),
                    Err(e) => trap_name(&format!("{e:?}")),
                });
            }
            Step::Msg(k, d) => results.push(match p.deliver(Key::new(k).unwrap(), d) {
                Ok(b) => format!("{b}"),
                Err(e) => trap_name(&format!("{e:?}")),
            }),
            Step::Req(k, d) => {
                let mut reply = [0u8; 64];
                results.push(
                    match p.request(Key::new(k).unwrap(), d, &RequestMeta::NONE, &mut reply) {
                        Ok(Ok(n)) => format!("reply {:02x?}", &reply[..n]),
                        Ok(Err(e)) => format!("error {e:?}"),
                        Err(e) => trap_name(&format!("{e:?}")),
                    },
                );
            }
        }
    }
    let memory = p.memory().to_vec();
    Ok(Outcome {
        trace: std::mem::take(&mut p.device().trace),
        results,
        memory,
    })
}

/// Traps compare by class: fuel, poisoned, or any other trap.
fn trap_name(dbg: &str) -> String {
    if dbg.contains("OutOfFuel") {
        "trap fuel".into()
    } else if dbg.contains("Poisoned") {
        "trap poisoned".into()
    } else {
        "trap".into()
    }
}

fn run_model(wasm: &[u8], steps: &[Step]) -> Result<Outcome, String> {
    let clock = Rc::new(Cell::new(0));
    let mut rec = Rec::new(clock.clone());
    let mut c = Box::new(Model::new());
    c.load(wasm).map_err(|e| format!("{e:?}"))?;
    c.init(&mut rec).map_err(|e| format!("init {e:?}"))?;
    let mut results = Vec::new();
    for st in steps {
        match st {
            Step::Tick(t) => {
                clock.set(*t);
                results.push(match c.tick(*t, &mut rec) {
                    Ok(Ticked::Ran) => "ran".into(),
                    Ok(Ticked::NotDue) => "notdue".into(),
                    Ok(Ticked::Disabled) => "disabled".into(),
                    Ok(Ticked::NoTickExport) => "notick".into(),
                    Err(e) => trap_name(&format!("{e:?}")),
                });
            }
            Step::Msg(k, d) => results.push(match c.deliver(Key::new(k).unwrap(), d, &mut rec) {
                Ok(b) => format!("{b}"),
                Err(e) => trap_name(&format!("{e:?}")),
            }),
            Step::Req(k, d) => {
                let mut reply = [0u8; 64];
                results.push(
                    match c.request(
                        Key::new(k).unwrap(),
                        d,
                        &RequestMeta::NONE,
                        &mut reply,
                        &mut rec,
                    ) {
                        Ok(Ok(n)) => format!("reply {:02x?}", &reply[..n]),
                        Ok(Err(e)) => format!("error {e:?}"),
                        Err(e) => trap_name(&format!("{e:?}")),
                    },
                );
            }
        }
    }
    Ok(Outcome {
        trace: rec.trace,
        results,
        memory: c.memory().to_vec(),
    })
}

/// Loads with the model first to learn the keys, builds the script, runs
/// both, compares.
fn differential(name: &str, wasm: &[u8]) {
    let mut probe = Box::new(Model::new());
    probe
        .load(wasm)
        .unwrap_or_else(|e| panic!("{name}: model refused: {e:?}"));
    let mut rec = Rec::new(Rc::new(Cell::new(0)));
    probe
        .init(&mut rec)
        .unwrap_or_else(|e| panic!("{name}: init {e:?}"));
    let subs: Vec<String> = probe.subscriptions().map(String::from).collect();
    let serves: Vec<String> = probe.serves().map(String::from).collect();
    let steps = script(&subs, &serves);
    let a = run_wasmi(wasm, &steps).unwrap_or_else(|e| panic!("{name}: wasmi: {e}"));
    let b = run_model(wasm, &steps).unwrap_or_else(|e| panic!("{name}: model: {e}"));
    assert_eq!(a.trace, b.trace, "{name}: board calls differ");
    assert_eq!(a.results, b.results, "{name}: results differ");
    assert!(a.memory == b.memory, "{name}: final memory differs");
    assert!(
        !a.trace.is_empty() || !a.results.is_empty(),
        "{name}: did nothing"
    );
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

#[test]
fn cl3_wat_templates_match_wasmi() {
    let dir = root().join("payloads/templates");
    let mut n = 0;
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "wat") {
            let wasm = wat::parse_file(&p).unwrap();
            differential(&p.display().to_string(), &wasm);
            n += 1;
        }
    }
    assert!(n >= 5, "templates found: {n}");
}

#[test]
fn cl3_phase_payloads_match_wasmi() {
    let dir = root().join("tools/payload-sim/tests/fixtures/phase");
    let mut n = 0;
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        let wasm = std::fs::read(&p).unwrap();
        differential(&p.display().to_string(), &wasm);
        n += 1;
    }
    assert_eq!(n, 3);
}

#[test]
fn cl3_rust_sdk_builds_match_wasmi_when_built() {
    let dir = root().join("payloads/target/wasm32-unknown-unknown/release");
    for name in ["echo", "dimmer", "blink", "button"] {
        let p = dir.join(format!("{name}.wasm"));
        let Ok(wasm) = std::fs::read(&p) else {
            eprintln!("skipping {name}: no build at {}", p.display());
            continue;
        };
        differential(name, &wasm);
    }
}

/// Exercises every arithmetic op, branches with values, `br_table`,
/// `call_indirect`, `select`, memory ops and `memory.grow`; publishes the
/// results so the trace carries them.
const OPS: &str = include_str!("ops.wat");

#[test]
fn cl3_every_op_matches_wasmi() {
    differential("ops", &wat::parse_str(OPS).unwrap());
}

#[test]
fn cl3_traps_match_wasmi() {
    for (what, body) in [
        ("unreachable", "(unreachable)"),
        ("div0", "(drop (i32.div_u (i32.const 1) (i32.const 0)))"),
        (
            "overflow",
            "(drop (i32.div_s (i32.const 0x80000000) (i32.const -1)))",
        ),
        ("oob load", "(drop (i32.load (i32.const 65533)))"),
        (
            "oob fill",
            "(memory.fill (i32.const 65530) (i32.const 0) (i32.const 7))",
        ),
        (
            "null",
            "(drop (call_indirect (param i32) (result i32) (i32.const 1) (i32.const 2)))",
        ),
        ("type", "(call_indirect (i32.const 0))"),
        (
            "table",
            "(drop (call_indirect (param i32) (result i32) (i32.const 1) (i32.const 9)))",
        ),
        ("fuel", "(loop $l (br $l))"),
        ("recursion", "(call $rec)"),
    ] {
        let src = format!(
            r#"(module
  (import "mw" "log" (func $log (param i32 i32)))
  (import "mw" "set_tick" (func $st (param i32) (result i32)))
  (memory (export "memory") 1 1)
  (table 3 funcref)
  (elem (i32.const 0) $id)
  (data (i32.const 0) "before")
  (func $id (param i32) (result i32) (local.get 0))
  (func $rec (call $rec))
  (func (export "init") (drop (call $st (i32.const 10))))
  (func (export "tick") (param i32)
    (call $log (i32.const 0) (i32.const 6))
    {body}))"#
        );
        let wasm = wat::parse_str(&src).unwrap();
        let steps = [Step::Tick(10), Step::Tick(20)];
        let a = run_wasmi(&wasm, &steps).unwrap();
        let b = run_model(&wasm, &steps).unwrap();
        assert_eq!(a.trace, b.trace, "{what}");
        assert_eq!(a.results, b.results, "{what}");
        let want = if what == "fuel" { "trap fuel" } else { "trap" };
        assert_eq!(b.results, [want, "trap poisoned"], "{what}");
    }
}

#[test]
fn cl3_load_errors_match_wasm_host_classes() {
    type Check = fn(&LoadError) -> bool;
    let cases: [(&str, Check); 5] = [
        (
            r#"(module (import "mw" "nope" (func)) (memory (export "memory") 1))"#,
            |e| *e == LoadError::UnknownImport,
        ),
        (
            r#"(module (import "mw" "log" (func (param i32))) (memory (export "memory") 1))"#,
            |e| *e == LoadError::BadSignature,
        ),
        (
            r#"(module (memory (export "memory") 1) (func (export "tick")))"#,
            |e| *e == LoadError::BadExport,
        ),
        (r#"(module (memory 1))"#, |e| *e == LoadError::MissingMemory),
        (r#"(module (memory (export "memory") 3))"#, |e| {
            matches!(e, LoadError::MemoryTooLarge { .. })
        }),
    ];
    for (src, ok) in cases {
        let wasm = wat::parse_str(src).unwrap();
        let e = Model::new().load(&wasm).unwrap_err();
        assert!(ok(&e), "{src}: {e:?}");
        assert!(Payload::load(&wasm, Rec::new(Rc::new(Cell::new(0))), Limits::default()).is_err());
    }
    // Outside the subset: floats, i64 arithmetic.
    for src in [
        r#"(module (memory (export "memory") 1) (func (drop (f32.const 1))))"#,
        r#"(module (memory (export "memory") 1) (func (drop (i64.add (i64.const 1) (i64.const 2)))))"#,
        r#"(module (memory (export "memory") 1) (func (local i64)))"#,
    ] {
        let e = Model::new()
            .load(&wat::parse_str(src).unwrap())
            .unwrap_err();
        assert!(matches!(e, LoadError::Unsupported(_)), "{src}: {e:?}");
    }
}

#[test]
fn cl3_bridge_template_fits_the_colorlight_core() {
    let wasm = wat::parse_file(root().join("payloads/templates/bridge.wat")).unwrap();
    let mut c = Box::new(wasm_hdl_ref::Colorlight::new());
    c.load(&wasm).unwrap();
    let mut rec = Rec::new(Rc::new(Cell::new(0)));
    c.init(&mut rec).unwrap();
    assert_eq!(c.memory().len(), 4096);
    assert_eq!(rec.bridges.iter().count(), 2);
    assert!(c.code().len() < 256, "{} words", c.code().len());
    // A 64 KiB-page template does not fit 16 KiB.
    let echo = wat::parse_file(root().join("payloads/templates/echo.wat")).unwrap();
    assert!(matches!(
        wasm_hdl_ref::Colorlight::new().load(&echo),
        Err(LoadError::MemoryTooLarge {
            bytes: 65536,
            max: 16384
        })
    ));
}
