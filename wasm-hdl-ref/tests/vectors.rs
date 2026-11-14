//! Golden vectors for the gateware simulation (CL4), checked in under
//! `gateware-5a75b/tests/vectors/`:
//!
//! * `frame.json` — datagrams with their [`serial::frame`] decoding (and
//!   the corrupt ones the decoder must drop), for the frame core;
//! * `bridge.json` — rule-table operations (install, de-duplicate, match,
//!   count, stop) with the [`BridgeTable`] answers, for the bridge engine;
//! * `loaderr.json` — modules the loader must refuse, with the error kind;
//! * `wasm/<name>.json` — a module, the image the loader must build from
//!   it (code words, function table, table 0, globals, exports, initial
//!   memory), and an event script with the board calls, results and final
//!   memory the interpreter must produce, all from the reference model
//!   with the 5A-75B's 16 KiB memory.
//!
//! The test fails when a checked-in file differs from what the model
//! produces now; `UPDATE_VECTORS=1 cargo test -p wasm-hdl-ref --test
//! vectors` rewrites them.

mod common;

use std::cell::Cell;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::rc::Rc;

use common::{Rec, Step};
use middleware::crc::{Crc32, crc16};
use middleware::serial::frame::{self, Frame};
use middleware::transport::HandlerError;
use middleware::{Key, RequestMeta};
use middleware::wasm_abi::bridge::{self, BridgeTable};
use wasm_hdl_ref::{Colorlight, Ticked};

fn hex(b: &[u8]) -> String {
    b.iter().fold(String::new(), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

fn q(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn list<T>(items: impl IntoIterator<Item = T>, f: impl Fn(T) -> String) -> String {
    let v: Vec<String> = items.into_iter().map(f).collect();
    format!("[{}]", v.join(", "))
}

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../gateware-5a75b/tests/vectors")
}

/// Writes (UPDATE_VECTORS=1) or checks one vector file.
fn golden(rel: &str, text: &str) {
    let p = dir().join(rel);
    if std::env::var_os("UPDATE_VECTORS").is_some() {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
        return;
    }
    let have = std::fs::read_to_string(&p)
        .unwrap_or_else(|_| panic!("{} missing: run with UPDATE_VECTORS=1", p.display()));
    assert!(
        have == text,
        "{} is stale: run with UPDATE_VECTORS=1",
        p.display()
    );
}

fn raw(f: &Frame<'_>) -> Vec<u8> {
    let mut b = [0u8; frame::MAX_RAW_FRAME];
    let n = frame::encode_raw(f, &mut b).unwrap();
    b[..n].to_vec()
}

fn with_crc(mut body: Vec<u8>) -> Vec<u8> {
    let c = crc16(&body).to_le_bytes();
    body.extend(c);
    body
}

fn frame_entry(name: &str, dgram: &[u8]) -> String {
    let d = match frame::decode(dgram) {
        Ok(Frame::Request { id, key, payload }) => format!(
            r#""ok": true, "kind": 1, "id": {id}, "key": {}, "payload": "{}""#,
            q(key.as_str()),
            hex(payload)
        ),
        Ok(Frame::Response { id, result }) => {
            let (status, payload) = match result {
                Ok(p) => (0, p),
                Err(e) => (e.code(), &[][..]),
            };
            format!(
                r#""ok": true, "kind": 2, "id": {id}, "status": {status}, "payload": "{}""#,
                hex(payload)
            )
        }
        Ok(Frame::Publish { key, payload }) => format!(
            r#""ok": true, "kind": 3, "id": 0, "key": {}, "payload": "{}""#,
            q(key.as_str()),
            hex(payload)
        ),
        Err(e) => format!(r#""ok": false, "error": {}"#, q(&format!("{e:?}"))),
    };
    format!(
        r#"    {{"name": {}, "datagram": "{}", {d}}}"#,
        q(name),
        hex(dgram)
    )
}

#[test]
fn cl4_frame_vectors() {
    let k = |s: &'static str| Key::from_static(s);
    let long_key = "a/".repeat(63) + "bc";
    let mut cases: Vec<(String, Vec<u8>)> = vec![
        ("info request".into(), raw(&Frame::Request { id: 1, key: k("sys/info"), payload: b"" })),
        ("chunk request".into(), raw(&Frame::Request {
            id: 200,
            key: k("sys/loader/chunk"),
            payload: &(0..=255u8).chain(0..=255).collect::<Vec<_>>(),
        })),
        ("publish".into(), raw(&Frame::Publish { key: k("demo/imu"), payload: &[1, 2, 3, 0, 0xFF] })),
        ("publish empty".into(), raw(&Frame::Publish { key: k("a"), payload: b"" })),
        ("publish 128-byte key".into(), raw(&Frame::Publish {
            key: Key::new(&long_key).unwrap(),
            payload: b"x",
        })),
        ("response ok".into(), raw(&Frame::Response { id: 9, result: Ok(b"ecp5") })),
        ("response not found".into(), raw(&Frame::Response { id: 10, result: Err(HandlerError::NotFound) })),
        ("max payload".into(), raw(&Frame::Publish {
            key: k("big"),
            payload: &[0x5A; frame::MAX_RAW_FRAME - 3 - 3 - 2],
        })),
    ];
    let mut bad = raw(&Frame::Publish { key: k("demo/x"), payload: b"hi" });
    bad[4] ^= 1;
    cases.push(("bad crc".into(), bad));
    cases.push(("bad kind".into(), with_crc(vec![9, 0, 0])));
    cases.push(("short".into(), vec![3, 0]));
    cases.push(("key past end".into(), with_crc(vec![3, 0, 9, b'a'])));
    cases.push(("key with space".into(), with_crc(vec![3, 0, 3, b'a', b' ', b'b'])));
    cases.push(("key with empty segment".into(), with_crc(vec![1, 4, 3, b'a', b'/', b'/'])));
    cases.push(("key starting with slash".into(), with_crc(vec![3, 0, 2, b'/', b'a'])));
    cases.push(("empty key".into(), with_crc(vec![3, 0, 0, 1, 2])));
    let mut k129 = vec![3u8, 0, 129];
    k129.extend(std::iter::repeat_n(b'k', 129));
    cases.push(("129-byte key".into(), with_crc(k129)));
    cases.push(("oversized".into(), with_crc(vec![3; frame::MAX_RAW_FRAME - 1])));
    let entries: Vec<String> = cases.iter().map(|(n, d)| frame_entry(n, d)).collect();
    let text = format!(
        "{{\"max_raw_frame\": {}, \"crc16_check\": {}, \"frames\": [\n{}\n]}}\n",
        frame::MAX_RAW_FRAME,
        crc16(b"123456789"),
        entries.join(",\n")
    );
    golden("frame.json", &text);
}

/// The templates with 1-byte pages so they fit the 16 KiB core.
fn byte_pages(src: &str) -> String {
    src.replace(
        r#"(memory (export "memory") 1 1)"#,
        r#"(memory (export "memory") 16384 16384 (pagesize 1))"#,
    )
}

fn module(name: &str) -> (String, Vec<u8>) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let src = std::fs::read_to_string(root.join(format!("payloads/templates/{name}.wat"))).unwrap();
    (name.to_string(), wat::parse_str(byte_pages(&src)).unwrap())
}

const OPS: &str = include_str!("ops.wat");

fn wasm_vector(name: &str, wasm: &[u8], steps: &[Step]) -> String {
    let clock = Rc::new(Cell::new(0));
    let mut rec = Rec::new(clock.clone());
    let mut c = Box::new(Colorlight::new());
    c.load(wasm).unwrap_or_else(|e| panic!("{name}: {e:?}"));
    let image_memory = c.memory().to_vec();
    let image = format!(
        concat!(
            "  \"image\": {{\n",
            "    \"code\": {},\n",
            "    \"funcs\": {},\n",
            "    \"table\": {},\n",
            "    \"globals\": {},\n",
            "    \"exports\": {{\"init\": {}, \"tick\": {}, \"mw_alloc\": {}, \"on_message\": {}, \"on_request\": {}}},\n",
            "    \"memory_len\": {},\n",
            "    \"memory_crc32\": {},\n",
            "    \"memory_nonzero\": {}\n",
            "  }}"
        ),
        list(c.code(), |w| w.to_string()),
        list(c.funcs(), |f| format!(
            "[{}, {}, {}, {}, {}]",
            f.import, f.entry, f.params, f.results, f.locals
        )),
        list(c.table(), |t| t.to_string()),
        list(c.globals(), |g| g.to_string()),
        opt(c.exports().init),
        opt(c.exports().tick),
        opt(c.exports().alloc),
        opt(c.exports().on_message),
        opt(c.exports().on_request),
        image_memory.len(),
        Crc32::checksum(&image_memory),
        list(
            image_memory.iter().enumerate().filter(|(_, b)| **b != 0),
            |(i, b)| format!("[{i}, {b}]")
        ),
    );
    let mut results = Vec::new();
    let mut step_json = Vec::new();
    let init = c.init(&mut rec);
    results.push(match init {
        Ok(()) => "ok".to_string(),
        Err(e) => format!("trap {e:?}"),
    });
    step_json.push(r#"{"op": "init"}"#.to_string());
    for st in steps {
        match st {
            Step::Tick(t) => {
                clock.set(*t);
                step_json.push(format!(r#"{{"op": "tick", "now": {t}}}"#));
                results.push(match c.tick(*t, &mut rec) {
                    Ok(Ticked::Ran) => "ran".into(),
                    Ok(Ticked::NotDue) => "notdue".into(),
                    Ok(Ticked::Disabled) => "disabled".into(),
                    Ok(Ticked::NoTickExport) => "notick".into(),
                    Err(e) => format!("trap {e:?}"),
                });
            }
            Step::Msg(k, d) => {
                step_json.push(format!(
                    r#"{{"op": "msg", "key": {}, "data": "{}"}}"#,
                    q(k),
                    hex(d)
                ));
                results.push(match c.deliver(Key::new(k).unwrap(), d, &mut rec) {
                    Ok(b) => b.to_string(),
                    Err(e) => format!("trap {e:?}"),
                });
            }
            Step::Req(k, d) => {
                step_json.push(format!(
                    r#"{{"op": "req", "key": {}, "data": "{}"}}"#,
                    q(k),
                    hex(d)
                ));
                let mut reply = [0u8; 64];
                results.push(
                    match c.request(Key::new(k).unwrap(), d, &RequestMeta::NONE, &mut reply, &mut rec) {
                        Ok(Ok(n)) => format!("reply {}", hex(&reply[..n])),
                        Ok(Err(e)) => format!("error {}", e.code()),
                        Err(e) => format!("trap {e:?}"),
                    },
                );
            }
        }
    }
    format!(
        "{{\n  \"name\": {},\n  \"memory_bytes\": {},\n  \"fuel_per_call\": {},\n  \"module\": \"{}\",\n{image},\n  \"steps\": {},\n  \"expect\": {{\n    \"trace\": {},\n    \"results\": {},\n    \"globals\": {},\n    \"memory_crc32\": {},\n    \"executed\": {}\n  }}\n}}\n",
        q(name),
        wasm_hdl_ref::COLORLIGHT_MEMORY,
        c.fuel_per_call,
        hex(wasm),
        list(&step_json, |s| s.clone()),
        list(&rec.trace, |s| q(s)),
        list(&results, |s| q(s)),
        list(c.globals(), |g| g.to_string()),
        Crc32::checksum(c.memory()),
        c.executed(),
    )
}

fn opt(v: Option<u16>) -> String {
    v.map_or("null".into(), |x| x.to_string())
}

fn ticks(ts: &[u32]) -> Vec<Step> {
    ts.iter().map(|t| Step::Tick(*t)).collect()
}

#[test]
fn cl4_wasm_vectors() {
    let msgs = |k: &str, bodies: &[&[u8]]| -> Vec<Step> {
        bodies.iter().map(|b| Step::Msg(k.into(), b.to_vec())).collect()
    };
    let mut all: Vec<(String, Vec<u8>, Vec<Step>)> = Vec::new();
    let (n, w) = module("bridge");
    let mut s = ticks(&[500, 1000]);
    s.extend(msgs("ctl/bridge", &[b"0", b""]));
    s.extend(ticks(&[2000]));
    s.extend(msgs("ctl/bridge", &[b"1"]));
    s.extend(ticks(&[3000, 3500]));
    all.push((n, w, s));
    let (n, w) = module("echo");
    let mut s = msgs("app/echo/in", &[b"", b"hello", &[0u8; 512], &[1u8; 513]]);
    s.extend(msgs("app/echo/other", &[b"x"]));
    all.push((n, w, s));
    for name in ["blink", "timer", "button"] {
        let (n, w) = module(name);
        all.push((n, w, ticks(&[100, 499, 500, 1000, 1500, 1501, 2600])));
    }
    all.push((
        "ops".into(),
        wat::parse_str(OPS).unwrap(),
        ticks(&[10]),
    ));
    let traps = [
        ("trap_unreachable", "(unreachable)"),
        ("trap_div0", "(drop (i32.div_u (i32.const 1) (i32.const 0)))"),
        ("trap_oob", "(drop (i32.load (i32.const 16382)))"),
        ("trap_fuel", "(loop $l (br $l))"),
        ("trap_recursion", "(call $rec)"),
    ];
    for (name, body) in traps {
        let src = format!(
            r#"(module
  (import "mw" "log" (func $log (param i32 i32)))
  (import "mw" "set_tick" (func $st (param i32) (result i32)))
  (memory (export "memory") 16384 16384 (pagesize 1))
  (data (i32.const 0) "before")
  (func $rec (call $rec))
  (func (export "init") (drop (call $st (i32.const 10))))
  (func (export "tick") (param i32)
    (call $log (i32.const 0) (i32.const 6))
    {body}))"#
        );
        all.push((name.into(), wat::parse_str(&src).unwrap(), ticks(&[10, 20])));
    }
    let mut names = Vec::new();
    for (name, wasm, steps) in &all {
        let text = wasm_vector(name, wasm, steps);
        golden(&format!("wasm/{name}.json"), &text);
        names.push(name.clone());
    }
    golden(
        "wasm/index.json",
        &format!("{}\n", list(&names, |n| q(n))),
    );
}

#[test]
fn cl4_vectors_use_the_reference_crc() {
    // The frame core's CRC-16 and the loader's CRC-32 check values.
    assert_eq!(crc16(b"123456789"), 0x29B1);
    assert_eq!(Crc32::checksum(b"123456789"), 0xCBF4_3926);
}

#[test]
fn cl4_bridge_vectors() {
    let mut t = BridgeTable::new();
    let mut ops: Vec<String> = Vec::new();
    let add = |t: &mut BridgeTable, ops: &mut Vec<String>, src: i32, key: &str, dst: i32| {
        let r = bridge::parse(src, key.as_bytes(), dst).and_then(|b| t.add(&b));
        let res = match r {
            Ok(h) => format!(r#""handle": {h}"#),
            Err(e) => format!(r#""error": {}"#, e.code()),
        };
        ops.push(format!(
            r#"{{"op": "add", "src": {src}, "key": {}, "dst": {dst}, {res}}}"#,
            q(key)
        ));
    };
    let route = |t: &mut BridgeTable, ops: &mut Vec<String>, src: u8, key: &str, sent: Option<u8>| {
        let f = t.route(src, Key::new(key).unwrap());
        let sent = sent.unwrap_or(f.links);
        t.account(f, sent);
        ops.push(format!(
            r#"{{"op": "route", "src": {src}, "key": {}, "links": {}, "rules": {}, "sent": {sent}}}"#,
            q(key),
            f.links,
            f.rules
        ));
    };
    let stat = |t: &BridgeTable, ops: &mut Vec<String>, h: u8| {
        let res = match t.get(h) {
            Some(r) => format!(r#""forwarded": {}, "dropped": {}"#, r.stats.forwarded, r.stats.dropped),
            None => r#""error": -7"#.to_string(),
        };
        ops.push(format!(r#"{{"op": "stat", "handle": {h}, {res}}}"#));
    };
    let remove = |t: &mut BridgeTable, ops: &mut Vec<String>, h: u8| {
        let res = match t.remove(h) {
            Ok(()) => "0".to_string(),
            Err(e) => e.code().to_string(),
        };
        ops.push(format!(r#"{{"op": "remove", "handle": {h}, "result": {res}}}"#));
    };
    add(&mut t, &mut ops, 0, "demo/**", 1);
    add(&mut t, &mut ops, 0, "demo/**", 1);
    add(&mut t, &mut ops, 0, "demo/x", 1);
    add(&mut t, &mut ops, 1, "back/ack", 0);
    add(&mut t, &mut ops, 0, "demo", 1);
    add(&mut t, &mut ops, 1, "demo/**", 0);
    for (src, key) in [
        (0, "demo"), (0, "demo/x"), (0, "demo/x/y"), (0, "demox"), (0, "dem"),
        (1, "demo/x"), (1, "back/ack"), (1, "back/ack/z"), (1, "back"), (0, "back/ack"),
    ] {
        route(&mut t, &mut ops, src, key, None);
    }
    route(&mut t, &mut ops, 0, "demo/x", Some(0));
    for h in 16..22 {
        stat(&t, &mut ops, h);
    }
    remove(&mut t, &mut ops, 17);
    remove(&mut t, &mut ops, 17);
    remove(&mut t, &mut ops, 40);
    route(&mut t, &mut ops, 0, "demo/x", None);
    add(&mut t, &mut ops, 0, "z/**", 1);
    let long = "a/".repeat(63) + "bc";
    add(&mut t, &mut ops, 0, &long, 1);
    route(&mut t, &mut ops, 0, &long, None);
    let long_prefix = "p/".repeat(62) + "q/**";
    add(&mut t, &mut ops, 0, &long_prefix, 1);
    route(&mut t, &mut ops, 0, &("p/".repeat(62) + "q/r"), None);
    for i in 0..12 {
        add(&mut t, &mut ops, 1, &format!("fill/{i}"), 0);
    }
    stat(&t, &mut ops, 16);
    golden(
        "bridge.json",
        &format!("{{\"ops\": [\n  {}\n]}}\n", ops.join(",\n  ")),
    );
}

#[test]
fn cl4_load_error_vectors() {
    let wat_cases: &[(&str, &str)] = &[
        ("unknown import", r#"(module (import "mw" "nope" (func)) (memory (export "memory") 1 (pagesize 1)))"#),
        ("other module", r#"(module (import "env" "log" (func (param i32 i32))) (memory (export "memory") 1 (pagesize 1)))"#),
        ("bad signature", r#"(module (import "mw" "log" (func (param i32))) (memory (export "memory") 1 (pagesize 1)))"#),
        ("bad export", r#"(module (memory (export "memory") 1 (pagesize 1)) (func (export "tick")))"#),
        ("bad export result", r#"(module (memory (export "memory") 1 (pagesize 1)) (func (export "mw_alloc") (param i32)))"#),
        ("missing memory", r#"(module (memory 1 (pagesize 1)))"#),
        ("no memory", r#"(module (func))"#),
        ("memory too large", r#"(module (memory (export "memory") 1))"#),
        ("memory too large bytes", r#"(module (memory (export "memory") 16385 (pagesize 1)))"#),
        ("float", r#"(module (memory (export "memory") 1 (pagesize 1)) (func (drop (f32.const 1))))"#),
        ("i64 add", r#"(module (memory (export "memory") 1 (pagesize 1)) (func (drop (i64.add (i64.const 1) (i64.const 2)))))"#),
        ("i64 local", r#"(module (memory (export "memory") 1 (pagesize 1)) (func (local i64)))"#),
        ("i64 param", r#"(module (memory (export "memory") 1 (pagesize 1)) (func (param i64)))"#),
        ("multi value", r#"(module (memory (export "memory") 1 (pagesize 1)) (func (result i32 i32) (i32.const 1) (i32.const 2)))"#),
        ("block i64", r#"(module (memory (export "memory") 1 (pagesize 1)) (func (drop (block (result i64) (i64.const 1)))))"#),
        ("start", r#"(module (memory (export "memory") 1 (pagesize 1)) (func $s) (start $s))"#),
        ("i64 global", r#"(module (memory (export "memory") 1 (pagesize 1)) (global i64 (i64.const 0)))"#),
        ("memory.init", r#"(module (memory (export "memory") 1 (pagesize 1)) (data "x") (func (memory.init 0 (i32.const 0) (i32.const 0) (i32.const 1))))"#),
        ("passive data", r#"(module (memory (export "memory") 1 (pagesize 1)) (data "x"))"#),
        ("data past memory", r#"(module (memory (export "memory") 4 (pagesize 1)) (data (i32.const 2) "xyz"))"#),
        ("imported memory", r#"(module (import "mw" "memory" (memory 1)))"#),
        ("two tables", r#"(module (memory (export "memory") 1 (pagesize 1)) (table 1 funcref) (table 1 funcref))"#),
        ("table too large", r#"(module (memory (export "memory") 1 (pagesize 1)) (table 65 funcref))"#),
        ("elem past table", r#"(module (memory (export "memory") 1 (pagesize 1)) (table 1 funcref) (func $f) (elem (i32.const 1) $f))"#),
        ("too many params", r#"(module (memory (export "memory") 1 (pagesize 1)) (func (param i32 i32 i32 i32 i32 i32 i32 i32 i32)))"#),
        ("too many globals", r#"(module (memory (export "memory") 1 (pagesize 1))
            (global i32 (i32.const 0)) (global i32 (i32.const 0)) (global i32 (i32.const 0)) (global i32 (i32.const 0))
            (global i32 (i32.const 0)) (global i32 (i32.const 0)) (global i32 (i32.const 0)) (global i32 (i32.const 0))
            (global i32 (i32.const 0)) (global i32 (i32.const 0)) (global i32 (i32.const 0)) (global i32 (i32.const 0))
            (global i32 (i32.const 0)) (global i32 (i32.const 0)) (global i32 (i32.const 0)) (global i32 (i32.const 0))
            (global i32 (i32.const 0)))"#),
        ("global from global", r#"(module (memory (export "memory") 1 (pagesize 1)) (global $a i32 (i32.const 0)) (global i32 (global.get $a)))"#),
        ("exported table", r#"(module (memory (export "memory") 1 (pagesize 1)) (table (export "t") 1 funcref))"#),
    ];
    let mut entries: Vec<String> = Vec::new();
    let mut push = |name: &str, wasm: &[u8]| {
        let mut c = Box::new(Colorlight::new());
        let e = match c.load(wasm) {
            Ok(()) => "Ok".to_string(),
            Err(wasm_hdl_ref::LoadError::Unsupported(_)) => "Unsupported".to_string(),
            Err(wasm_hdl_ref::LoadError::TooLarge(_)) => "TooLarge".to_string(),
            Err(wasm_hdl_ref::LoadError::MemoryTooLarge { .. }) => "MemoryTooLarge".to_string(),
            Err(e) => format!("{e:?}"),
        };
        entries.push(format!(
            r#"    {{"name": {}, "module": "{}", "error": {}}}"#,
            q(name),
            hex(wasm),
            q(&e)
        ));
    };
    for (name, src) in wat_cases {
        push(name, &wat::parse_str(src).unwrap());
    }
    // Truncations and corruptions of a valid module.
    let (_, good) = module("bridge");
    for cut in [0usize, 4, 8, 9, 20, good.len() / 2, good.len() - 1] {
        push(&format!("bridge cut at {cut}"), &good[..cut]);
    }
    let mut bad_magic = good.clone();
    bad_magic[1] = b'x';
    push("bad magic", &bad_magic);
    let mut bad_version = good.clone();
    bad_version[4] = 2;
    push("bad version", &bad_version);
    let mut trailing = good.clone();
    trailing.extend([0x0B, 0x00]);
    push("trailing junk", &trailing);
    let mut unknown_section = good.clone();
    unknown_section.extend([0x0D, 0x00]);
    push("unknown section", &unknown_section);
    golden(
        "loaderr.json",
        &format!("{{\"cases\": [\n{}\n]}}\n", entries.join(",\n")),
    );
}
