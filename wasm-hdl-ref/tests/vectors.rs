//! Golden vectors for the gateware simulation (CL4), checked in under
//! `gateware-5a75b/tests/vectors/`:
//!
//! * `frame.json` — datagrams with their [`serial::frame`] decoding (and
//!   the corrupt ones the decoder must drop), for the frame core;
//! * `bridge.json` — rule-table operations (install, de-duplicate, match,
//!   count, stop) with the [`BridgeTable`] answers, for the bridge engine;
//! * `imgerr.json` — images the installer must refuse, with the error kind;
//! * `wasm/<name>.json` — a module, its compiled image as the host uploads
//!   it (`blob`, [`wasm_hdl_ref::image`]), what the installer must build
//!   from that (code words, function table, table 0, globals, exports,
//!   initial memory), and an event script with the board calls, results
//!   and final memory the interpreter must produce, all from the
//!   reference model with the 5A-75B's 16 KiB memory;
//! * `upload.json` — the upload path's request sequences (with images).
//!
//! The script runs on a core that installed the image, not on the one
//! that compiled the module, so the vectors also check the round trip.
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
use middleware::wasm_abi::bridge::{self, BridgeTable};
use middleware::{Key, RequestMeta};
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

fn list<T>(items: impl IntoIterator<Item = T>, f: impl FnMut(T) -> String) -> String {
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
        (
            "info request".into(),
            raw(&Frame::Request {
                id: 1,
                key: k("sys/info"),
                payload: b"",
            }),
        ),
        (
            "chunk request".into(),
            raw(&Frame::Request {
                id: 200,
                key: k("sys/loader/chunk"),
                payload: &(0..=255u8).chain(0..=255).collect::<Vec<_>>(),
            }),
        ),
        (
            "publish".into(),
            raw(&Frame::Publish {
                key: k("demo/imu"),
                payload: &[1, 2, 3, 0, 0xFF],
            }),
        ),
        (
            "publish empty".into(),
            raw(&Frame::Publish {
                key: k("a"),
                payload: b"",
            }),
        ),
        (
            "publish 128-byte key".into(),
            raw(&Frame::Publish {
                key: Key::new(&long_key).unwrap(),
                payload: b"x",
            }),
        ),
        (
            "response ok".into(),
            raw(&Frame::Response {
                id: 9,
                result: Ok(b"ecp5"),
            }),
        ),
        (
            "response not found".into(),
            raw(&Frame::Response {
                id: 10,
                result: Err(HandlerError::NotFound),
            }),
        ),
        (
            "max payload".into(),
            raw(&Frame::Publish {
                key: k("big"),
                payload: &[0x5A; frame::MAX_RAW_FRAME - 3 - 3 - 2],
            }),
        ),
    ];
    let mut bad = raw(&Frame::Publish {
        key: k("demo/x"),
        payload: b"hi",
    });
    bad[4] ^= 1;
    cases.push(("bad crc".into(), bad));
    cases.push(("bad kind".into(), with_crc(vec![9, 0, 0])));
    cases.push(("short".into(), vec![3, 0]));
    cases.push(("key past end".into(), with_crc(vec![3, 0, 9, b'a'])));
    cases.push((
        "key with space".into(),
        with_crc(vec![3, 0, 3, b'a', b' ', b'b']),
    ));
    cases.push((
        "key with empty segment".into(),
        with_crc(vec![1, 4, 3, b'a', b'/', b'/']),
    ));
    cases.push((
        "key starting with slash".into(),
        with_crc(vec![3, 0, 2, b'/', b'a']),
    ));
    cases.push(("empty key".into(), with_crc(vec![3, 0, 0, 1, 2])));
    let mut k129 = vec![3u8, 0, 129];
    k129.extend(std::iter::repeat_n(b'k', 129));
    cases.push(("129-byte key".into(), with_crc(k129)));
    cases.push((
        "oversized".into(),
        with_crc(vec![3; frame::MAX_RAW_FRAME - 1]),
    ));
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

/// The image the host uploads for `wasm` (compiled by the model).
fn image_of(wasm: &[u8]) -> Vec<u8> {
    let mut c = Box::new(Colorlight::new());
    c.load(wasm).unwrap_or_else(|e| panic!("load: {e:?}"));
    let mut out = vec![0u8; wasm_hdl_ref::COLORLIGHT_MEMORY];
    let n = c.emit_image(&mut out).unwrap();
    out.truncate(n);
    // The round trip: installing the image gives the compiled core.
    let mut d = Box::new(Colorlight::new());
    d.install(&out).unwrap();
    assert_eq!(c.code(), d.code());
    assert_eq!(c.funcs(), d.funcs());
    assert_eq!(c.table(), d.table());
    assert_eq!(c.globals(), d.globals());
    assert_eq!(c.memory(), d.memory());
    assert_eq!(c.exports(), d.exports());
    out
}

fn wasm_vector(name: &str, wasm: &[u8], steps: &[Step]) -> String {
    let clock = Rc::new(Cell::new(0));
    let mut rec = Rec::new(clock.clone());
    let blob = image_of(wasm);
    let mut c = Box::new(Colorlight::new());
    c.install(&blob)
        .unwrap_or_else(|e| panic!("{name}: install: {e:?}"));
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
                    match c.request(
                        Key::new(k).unwrap(),
                        d,
                        &RequestMeta::NONE,
                        &mut reply,
                        &mut rec,
                    ) {
                        Ok(Ok(n)) => format!("reply {}", hex(&reply[..n])),
                        Ok(Err(e)) => format!("error {}", e.code()),
                        Err(e) => format!("trap {e:?}"),
                    },
                );
            }
        }
    }
    format!(
        "{{\n  \"name\": {},\n  \"memory_bytes\": {},\n  \"fuel_per_call\": {},\n  \"module\": \"{}\",\n  \"blob\": \"{}\",\n{image},\n  \"steps\": {},\n  \"expect\": {{\n    \"trace\": {},\n    \"results\": {},\n    \"globals\": {},\n    \"memory_crc32\": {},\n    \"executed\": {}\n  }}\n}}\n",
        q(name),
        wasm_hdl_ref::COLORLIGHT_MEMORY,
        c.fuel_per_call,
        hex(wasm),
        hex(&blob),
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
        bodies
            .iter()
            .map(|b| Step::Msg(k.into(), b.to_vec()))
            .collect()
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
    all.push(("ops".into(), wat::parse_str(OPS).unwrap(), ticks(&[10])));
    let traps = [
        ("trap_unreachable", "(unreachable)"),
        (
            "trap_div0",
            "(drop (i32.div_u (i32.const 1) (i32.const 0)))",
        ),
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
    golden("wasm/index.json", &format!("{}\n", list(&names, |n| q(n))));
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
    let route = |t: &mut BridgeTable,
                 ops: &mut Vec<String>,
                 src: u8,
                 key: &str,
                 sent: Option<u8>| {
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
            Some(r) => format!(
                r#""forwarded": {}, "dropped": {}"#,
                r.stats.forwarded, r.stats.dropped
            ),
            None => r#""error": -7"#.to_string(),
        };
        ops.push(format!(r#"{{"op": "stat", "handle": {h}, {res}}}"#));
    };
    let remove = |t: &mut BridgeTable, ops: &mut Vec<String>, h: u8| {
        let res = match t.remove(h) {
            Ok(()) => "0".to_string(),
            Err(e) => e.code().to_string(),
        };
        ops.push(format!(
            r#"{{"op": "remove", "handle": {h}, "result": {res}}}"#
        ));
    };
    add(&mut t, &mut ops, 0, "demo/**", 1);
    add(&mut t, &mut ops, 0, "demo/**", 1);
    add(&mut t, &mut ops, 0, "demo/x", 1);
    add(&mut t, &mut ops, 1, "back/ack", 0);
    add(&mut t, &mut ops, 0, "demo", 1);
    add(&mut t, &mut ops, 1, "demo/**", 0);
    for (src, key) in [
        (0, "demo"),
        (0, "demo/x"),
        (0, "demo/x/y"),
        (0, "demox"),
        (0, "dem"),
        (1, "demo/x"),
        (1, "back/ack"),
        (1, "back/ack/z"),
        (1, "back"),
        (0, "back/ack"),
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

fn error_kind(e: wasm_hdl_ref::LoadError) -> String {
    use wasm_hdl_ref::LoadError::*;
    match e {
        Unsupported(_) => "Unsupported".into(),
        TooLarge(_) => "TooLarge".into(),
        MemoryTooLarge { .. } => "MemoryTooLarge".into(),
        e => format!("{e:?}"),
    }
}

/// Images the installer must refuse (the host's compiler never writes
/// them): truncations, trailing bytes, a bad magic or version, counts past
/// the core's tables, a bad page size, memory past 16 KiB or a maximum
/// under the initial length, exports and table entries naming no
/// function, a data segment past the initial memory.
#[test]
fn cl4_image_error_vectors() {
    let wasm = wat::parse_str(
        r#"(module
  (import "mw" "log" (func $log (param i32 i32)))
  (memory (export "memory") 64 128 (pagesize 1))
  (table 2 funcref)
  (elem (i32.const 0) $f)
  (global (mut i32) (i32.const 5))
  (data (i32.const 8) "abc")
  (func $f (export "init") (call $log (i32.const 8) (i32.const 3))))"#,
    )
    .unwrap();
    let good = image_of(&wasm);
    use wasm_hdl_ref::image::{FUNC_BYTES, HEADER};
    let code = u16::from_le_bytes([good[4], good[5]]) as usize;
    let tab = HEADER + 4 * code + 2 * FUNC_BYTES;
    let seg = tab + 2 * 2 + 4;
    let set = |at: usize, v: &[u8]| {
        let mut b = good.clone();
        b[at..at + v.len()].copy_from_slice(v);
        b
    };
    let mut cases: Vec<(String, Vec<u8>)> = vec![("good".into(), good.clone())];
    for cut in [
        0,
        2,
        4,
        15,
        HEADER - 1,
        HEADER,
        HEADER + 3,
        tab,
        seg + 3,
        good.len() - 1,
    ] {
        cases.push((format!("cut at {cut}"), good[..cut].to_vec()));
    }
    let mut long = good.clone();
    long.push(0);
    cases.push(("trailing byte".into(), long));
    cases.push(("bad magic".into(), set(0, b"MWX")));
    cases.push(("bad version".into(), set(3, &[2])));
    cases.push(("wasm module".into(), wasm.clone()));
    let u16le = |v: u16| v.to_le_bytes();
    cases.push(("code too long".into(), set(4, &u16le(4097))));
    cases.push(("too many functions".into(), set(6, &[129])));
    cases.push(("table too large".into(), set(7, &[65])));
    cases.push(("too many globals".into(), set(8, &[17])));
    cases.push(("page size".into(), set(9, &[1])));
    cases.push(("memory too large".into(), set(12, &u16le(16385))));
    cases.push(("maximum too large".into(), set(14, &u16le(16385))));
    cases.push(("maximum under initial".into(), set(14, &u16le(63))));
    cases.push(("export past the functions".into(), set(16, &u16le(2))));
    cases.push((
        "last export past the functions".into(),
        set(26, &u16le(200)),
    ));
    cases.push((
        "table entry past the functions".into(),
        set(tab + 2, &u16le(2)),
    ));
    cases.push(("segment past memory".into(), set(seg, &u16le(62))));
    cases.push((
        "segment length past memory".into(),
        set(seg + 2, &u16le(57)),
    ));
    cases.push(("segment count too high".into(), set(10, &u16le(2))));
    let mut entries = Vec::new();
    for (name, img) in &cases {
        let mut c = Box::new(Colorlight::new());
        let e = c.install(img).map_or_else(error_kind, |()| "Ok".into());
        assert_eq!(e == "Ok", name == "good", "{name}: {e}");
        entries.push(format!(
            r#"    {{"name": {}, "image": "{}", "error": {}}}"#,
            q(name),
            hex(img),
            q(&e)
        ));
    }
    golden(
        "imgerr.json",
        &format!("{{\"cases\": [\n{}\n]}}\n", entries.join(",\n")),
    );
}

/// The 5A-75B's system-request shell for the upload path (CL6): what the
/// gateware's `upload.py` must answer, request for request. Transfers
/// follow [`Loader::step`] with the 16 KiB capacity and the payload status
/// follows the base firmware's dispatch (`firmware/src/dispatch.rs`):
/// `begin` stops the payload (Loading), a failed transfer returns to the
/// resting status, a commit installs (Stopped), `start` runs `init`
/// (Running, or Trapped when it traps), `stop` stops. Unlike the
/// microcontrollers, the board takes an image the host compiled
/// ([`wasm_hdl_ref::image`]) and installs it at commit: an image the
/// installer refuses is answered `InvalidRequest` and the previous payload
/// stays installed. Commits are unsigned (the board is unclaimed; a
/// signature body is parsed and ignored).
struct Shell {
    loader: middleware::loader::Loader,
    status: middleware::loader::PayloadStatus,
    staging: Vec<u8>,
    installed: Option<Vec<u8>>,
}

impl Shell {
    fn new() -> Self {
        Shell {
            loader: middleware::loader::Loader::Idle,
            status: middleware::loader::PayloadStatus::Empty,
            staging: Vec::new(),
            installed: None,
        }
    }

    fn resting(&self) -> middleware::loader::PayloadStatus {
        use middleware::loader::PayloadStatus::*;
        if self.installed.is_some() {
            Stopped
        } else {
            Empty
        }
    }

    fn installs(img: &[u8]) -> bool {
        let mut c = Box::new(Colorlight::new());
        c.install(img).is_ok()
    }

    fn init_traps(img: &[u8]) -> bool {
        let mut c = Box::new(Colorlight::new());
        c.install(img).unwrap();
        let mut rec = Rec::new(Rc::new(Cell::new(0)));
        c.init(&mut rec).is_err()
    }

    /// Answers one request: the response status code (0 = Ok).
    fn handle(&mut self, key: &str, body: &[u8]) -> u8 {
        use middleware::loader::{Command, Effect, PayloadStatus::*};
        let cmd = match Command::parse(Key::new(key).unwrap(), body) {
            Ok(Some(c)) => c,
            Ok(None) => return HandlerError::NotFound.code(),
            Err(e) => return e.code(),
        };
        match cmd {
            Command::Begin { .. } | Command::Chunk { .. } | Command::Commit { .. } => {}
            Command::Start => {
                return match self.status {
                    Empty | Loading => HandlerError::InvalidRequest.code(),
                    Running => 0,
                    Stopped | Trapped => {
                        let img = self.installed.as_ref().unwrap();
                        self.status = if Self::init_traps(img) {
                            Trapped
                        } else {
                            Running
                        };
                        0
                    }
                };
            }
            Command::Stop => {
                if matches!(self.status, Running | Trapped) {
                    self.status = Stopped;
                }
                return 0;
            }
            Command::Info | Command::TrustInfo => return 0,
            Command::Board | Command::TrustClaim { .. } => return HandlerError::NotFound.code(),
        }
        let (next, r) = self
            .loader
            .step(&cmd, wasm_hdl_ref::COLORLIGHT_MEMORY as u32);
        self.loader = next;
        match r {
            Err(e) => {
                if self.status == Loading {
                    self.status = self.resting();
                }
                e.code()
            }
            Ok(Effect::Erase { len }) => {
                self.staging = vec![0; len as usize];
                self.status = Loading;
                0
            }
            Ok(Effect::Write { offset, data }) => {
                let o = offset as usize;
                self.staging[o..o + data.len()].copy_from_slice(data);
                0
            }
            Ok(Effect::Activate { .. }) => {
                if Self::installs(&self.staging) {
                    self.installed = Some(std::mem::take(&mut self.staging));
                    self.status = Stopped;
                    0
                } else {
                    self.status = self.resting();
                    HandlerError::InvalidRequest.code()
                }
            }
            Ok(Effect::None) => 0,
        }
    }
}

fn begin_body(len: u32, crc: u32) -> Vec<u8> {
    let mut b = len.to_le_bytes().to_vec();
    b.extend(crc.to_le_bytes());
    b
}

fn chunk_body(offset: u32, data: &[u8]) -> Vec<u8> {
    let mut b = offset.to_le_bytes().to_vec();
    b.extend(data);
    b
}

/// Request keys and bodies.
type Reqs = Vec<(&'static str, Vec<u8>)>;

/// The upload of `img` in chunks of `size`, as mwctl's plan sends it.
fn upload_reqs(img: &[u8], size: usize) -> Reqs {
    let mut v = vec![(
        "sys/loader/begin",
        begin_body(img.len() as u32, Crc32::checksum(img)),
    )];
    for (i, c) in img.chunks(size).enumerate() {
        v.push(("sys/loader/chunk", chunk_body((i * size) as u32, c)));
    }
    v.push(("sys/loader/commit", Vec::new()));
    v
}

#[test]
fn cl6_upload_vectors() {
    use middleware::loader::sha256;
    let good = image_of(&module("bridge").1);
    let blink = image_of(&module("blink").1);
    let traps = image_of(
        &wat::parse_str(
            r#"(module (memory (export "memory") 16 (pagesize 1)) (func (export "init") unreachable))"#,
        )
        .unwrap(),
    );
    // A module uploaded as it is: not an image.
    let refused = wat::parse_str(r#"(module (memory (export "memory") 1 (pagesize 1)))"#).unwrap();
    let cut = good[..good.len() / 2].to_vec();
    let n = good.len() as u32;
    let crc = Crc32::checksum(&good);
    let s = |k: &'static str, b: Vec<u8>| vec![(k, b)];

    let mut cases: Vec<(&str, Reqs)> = Vec::new();
    let mut happy = s("sys/info", vec![]);
    happy.extend(upload_reqs(&good, 512));
    happy.extend(s("sys/payload/start", vec![]));
    happy.extend(s("sys/payload/start", vec![]));
    happy.extend(s("sys/payload/stop", vec![]));
    happy.extend(s("sys/payload/stop", vec![]));
    happy.extend(s("sys/payload/start", vec![]));
    cases.push(("happy path, start and stop", happy));
    // Small chunks, each sent twice (the response was lost).
    let mut resend = Vec::new();
    for r in upload_reqs(&good, 100) {
        if r.0 == "sys/loader/chunk" {
            resend.push(r.clone());
        }
        resend.push(r);
    }
    cases.push(("every chunk resent", resend));
    let mut ooo = upload_reqs(&good, 128);
    ooo.swap(1, 2);
    cases.push(("out of order abandons the transfer", ooo));
    let mut bad_crc = upload_reqs(&good, 512);
    bad_crc[0].1 = begin_body(n, crc ^ 1);
    cases.push(("crc mismatch at commit", bad_crc));
    let mut short = upload_reqs(&good, 512);
    short.remove(short.len() - 2);
    cases.push(("commit before the last chunk", short));
    cases.push((
        "oversize begin",
        s("sys/loader/begin", begin_body(16385, 0)),
    ));
    cases.push(("zero-length begin", s("sys/loader/begin", begin_body(0, 0))));
    let mut over = s("sys/loader/begin", begin_body(4, Crc32::checksum(b"abcd")));
    over.extend(s("sys/loader/chunk", chunk_body(0, b"abcde")));
    cases.push(("chunk past the declared length", over));
    let mut empty = s("sys/loader/begin", begin_body(4, Crc32::checksum(b"abcd")));
    empty.extend(s("sys/loader/chunk", chunk_body(0, b"")));
    empty.extend(s("sys/loader/chunk", vec![0, 0]));
    empty.extend(s("sys/loader/chunk", chunk_body(0, &[0x55; 513])));
    empty.extend(s("sys/loader/begin", vec![1, 2, 3]));
    empty.extend(s("sys/loader/chunk", chunk_body(0, b"abcd")));
    empty.extend(s("sys/loader/commit", vec![0; 5]));
    empty.extend(s("sys/loader/commit", vec![0; 68]));
    cases.push(("malformed bodies leave the transfer alone", empty));
    cases.push(("chunk and commit while idle", {
        let mut v = s("sys/loader/chunk", chunk_body(0, b"x"));
        v.extend(s("sys/loader/commit", vec![]));
        v.extend(s("sys/payload/start", vec![]));
        v.extend(s("sys/payload/stop", vec![]));
        v
    }));
    cases.push(("refused image keeps the installed one", {
        let mut v = upload_reqs(&good, 512);
        v.extend(s("sys/payload/start", vec![]));
        v.extend(upload_reqs(&refused, 512));
        v.extend(upload_reqs(&cut, 512));
        v.extend(s("sys/payload/start", vec![]));
        v
    }));
    cases.push(("init trap", {
        let mut v = upload_reqs(&traps, 512);
        v.extend(s("sys/payload/start", vec![]));
        v.extend(s("sys/payload/stop", vec![]));
        v.extend(s("sys/payload/start", vec![]));
        v
    }));
    cases.push(("replace a running payload", {
        let mut v = upload_reqs(&good, 512);
        v.extend(s("sys/payload/start", vec![]));
        v.extend(upload_reqs(&blink, 300));
        v.extend(s("sys/payload/start", vec![]));
        v
    }));
    cases.push(("other sys keys", {
        let mut v = s("sys/nope", vec![]);
        v.extend(s("sys/board", vec![]));
        v.extend(s("sys/trust/claim", vec![0; 32]));
        v.extend(s("sys/trust/info", vec![]));
        v.extend(s("sys/info", vec![1]));
        v.extend(s("sys/payload/start", vec![1]));
        v
    }));

    let mut out = Vec::new();
    for (name, reqs) in &cases {
        let mut sh = Shell::new();
        let steps = list(reqs, |(k, b)| {
            let code = sh.handle(k, b);
            format!(
                r#"{{"key": {}, "body": "{}", "code": {code}, "status": {}}}"#,
                q(k),
                hex(b),
                sh.status as u8
            )
        });
        let inst = sh.installed.as_ref().map_or("null".into(), |i| {
            format!(
                r#"{{"len": {}, "crc32": {}, "sha256": "{}"}}"#,
                i.len(),
                Crc32::checksum(i),
                hex(&sha256::ImageHash::digest(i))
            )
        });
        out.push(format!(
            "    {{\"name\": {}, \"steps\": {steps},\n     \"installed\": {inst}}}",
            q(name)
        ));
    }
    // Check values for the streaming CRC-32 and SHA-256 cores.
    let lens = [0usize, 1, 3, 55, 56, 63, 64, 65, 119, 128, 1000];
    let checks = list(lens, |l| {
        let d: Vec<u8> = (0..l).map(|i| (i * 37 + 11) as u8).collect();
        format!(
            r#"{{"data": "{}", "crc32": {}, "sha256": "{}"}}"#,
            hex(&d),
            Crc32::checksum(&d),
            hex(&sha256::ImageHash::digest(&d))
        )
    });
    golden(
        "upload.json",
        &format!(
            "{{\"capacity\": {}, \"checks\": {checks},\n \"cases\": [\n{}\n]}}\n",
            wasm_hdl_ref::COLORLIGHT_MEMORY,
            out.join(",\n")
        ),
    );
}
