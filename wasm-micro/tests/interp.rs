//! Behaviour beyond the specification suite: the host interface, limits,
//! and safety on modules a validator would have refused.

use wasm_micro::{Error, Host, Instance, Limits, Module};

fn wasm(text: &str) -> &'static [u8] {
    wat::parse_str(text).unwrap().leak()
}

fn instance(text: &str, limits: &Limits) -> Result<Instance<&'static [u8]>, Error> {
    Instance::new(Module::new(wasm(text))?, limits, &mut ())
}

fn call(inst: &mut Instance<&'static [u8]>, name: &str, args: &[u64]) -> Result<u64, Error> {
    let f = inst.module().export_func(name).unwrap();
    let mut out = [0];
    inst.call(&mut (), f, args, &mut out)?;
    Ok(out[0])
}

const FIB: &str = r#"(module
    (func $fib (export "fib") (param i32) (result i32)
        (if (result i32) (i32.lt_u (local.get 0) (i32.const 2))
            (then (local.get 0))
            (else (i32.add (call $fib (i32.sub (local.get 0) (i32.const 1)))
                           (call $fib (i32.sub (local.get 0) (i32.const 2)))))))
    (func (export "spin") (result i32) (loop (br 0)) (i32.const 0))
    (func $down (export "down") (param i32) (result i32)
        (call $down (i32.add (local.get 0) (i32.const 1)))))"#;

#[test]
fn recursion_and_control_flow() {
    let mut i = instance(FIB, &Limits::default()).unwrap();
    assert_eq!(call(&mut i, "fib", &[20]), Ok(6765));
}

#[test]
fn fuel_stops_a_loop_and_the_instance_survives() {
    let mut i = instance(FIB, &Limits::default()).unwrap();
    i.set_fuel(10_000);
    assert_eq!(call(&mut i, "spin", &[]), Err(Error::OutOfFuel));
    assert_eq!(i.fuel(), 0);
    i.set_fuel(1_000_000);
    assert_eq!(call(&mut i, "fib", &[10]), Ok(55));
    assert!(i.fuel() < 1_000_000);
}

#[test]
fn runaway_recursion_hits_the_frame_limit() {
    let mut i = instance(
        FIB,
        &Limits {
            frames: 8,
            ..Limits::default()
        },
    )
    .unwrap();
    assert_eq!(call(&mut i, "down", &[0]), Err(Error::StackOverflow));
    assert_eq!(call(&mut i, "fib", &[5]), Ok(5));
    // fib n nests n calls, down to fib 1.
    assert_eq!(call(&mut i, "fib", &[8]), Ok(21));
    assert_eq!(call(&mut i, "fib", &[9]), Err(Error::StackOverflow));
}

struct Log(Vec<String>);
impl Host for Log {
    fn call(&mut self, import: u32, mem: &mut [u8], vals: &mut [u64]) -> Result<(), Error> {
        match import {
            // log(ptr, len)
            0 => {
                let (p, n) = (vals[0] as usize, vals[1] as usize);
                let s = mem.get(p..p + n).ok_or(Error::Host(1))?;
                self.0.push(String::from_utf8_lossy(s).into_owned());
            }
            // divmod(a, b) -> (a / b, a % b), and it writes to memory
            1 => {
                let (a, b) = (vals[0] as u32, vals[1] as u32);
                if b == 0 {
                    return Err(Error::Host(2));
                }
                mem[0] = b'H';
                vals[0] = (a / b) as u64;
                vals[1] = (a % b) as u64;
            }
            // wide() -> i64 : more results than parameters
            _ => vals[0] = 0x1_0000_0002,
        }
        Ok(())
    }
}

#[test]
fn host_imports_see_arguments_memory_and_return_results() {
    let bytes = wasm(
        r#"(module
        (import "mw" "log" (func $log (param i32 i32)))
        (import "mw" "divmod" (func $divmod (param i32 i32) (result i32 i32)))
        (import "mw" "wide" (func $wide (result i64)))
        (memory 1)
        (data (i32.const 0) "hello")
        (func (export "run") (param i32 i32) (result i32)
            (call $divmod (local.get 0) (local.get 1))
            (i32.add (i32.mul (i32.const 100)))
            (call $log (i32.const 0) (i32.const 5)))
        (func (export "wide") (result i64) (call $wide)))"#,
    );
    let module = Module::new(bytes).unwrap();
    let names: Vec<_> = module
        .imports()
        .map(|i| (i.module, i.name, i.params.len(), i.results.len()))
        .collect();
    assert_eq!(
        names,
        [
            (&b"mw"[..], &b"log"[..], 2, 0),
            (b"mw", b"divmod", 2, 2),
            (b"mw", b"wide", 0, 1)
        ]
    );
    assert_eq!(module.import_count(), 3);
    let (run, wide) = (
        module.export_func("run").unwrap(),
        module.export_func("wide").unwrap(),
    );
    let mut host = Log(Vec::new());
    let mut i = Instance::new(module, &Limits::default(), &mut host).unwrap();
    let mut out = [0];
    i.call(&mut host, run, &[47, 5], &mut out).unwrap();
    assert_eq!(out[0], 9 + 100 * 2);
    assert_eq!(host.0, ["Hello"]);
    assert_eq!(
        i.call(&mut host, run, &[1, 0], &mut out),
        Err(Error::Host(2))
    );
    i.call(&mut host, wide, &[], &mut out).unwrap();
    assert_eq!(out[0], 0x1_0000_0002);
    // Arity is checked at the boundary.
    assert_eq!(i.call(&mut host, run, &[1], &mut out), Err(Error::Invalid));
}

const MEM: &str = r#"(module
    (memory 1 4)
    (data (i32.const 16) "\2a")
    (func (export "peek") (param i32) (result i32) (i32.load8_u (local.get 0)))
    (func (export "size") (result i32) (memory.size))
    (func (export "grow") (param i32) (result i32) (memory.grow (local.get 0))))"#;

#[test]
fn memory_backing_can_be_smaller_than_a_page() {
    let mut i = instance(
        MEM,
        &Limits {
            memory_bytes: 4096,
            ..Limits::default()
        },
    )
    .unwrap();
    assert_eq!(i.memory().len(), 4096);
    assert_eq!(call(&mut i, "size", &[]), Ok(1));
    assert_eq!(call(&mut i, "peek", &[16]), Ok(42));
    assert_eq!(call(&mut i, "peek", &[4095]), Ok(0));
    assert_eq!(call(&mut i, "peek", &[4096]), Err(Error::MemoryOutOfBounds));
    // One page is all 4096 bytes round up to.
    assert_eq!(call(&mut i, "grow", &[1]), Ok(u32::MAX as u64));
    // A segment above the cap does not fit.
    let high = r#"(module (memory 1) (data (i32.const 5000) "x"))"#;
    let l = Limits {
        memory_bytes: 4096,
        ..Limits::default()
    };
    assert_eq!(instance(high, &l).err(), Some(Error::SegmentOutOfBounds));
}

#[test]
fn one_byte_pages_and_the_footprint_is_what_loading_allocates() {
    let text = r#"(module
        (memory 100 300 (pagesize 1))
        (data (i32.const 96) "abcd")
        (func (export "peek") (param i32) (result i32) (i32.load8_u (local.get 0)))
        (func (export "size") (result i32) (memory.size))
        (func (export "grow") (param i32) (result i32) (memory.grow (local.get 0))))"#;
    let mut i = instance(
        text,
        &Limits {
            memory_bytes: 250,
            ..Limits::default()
        },
    )
    .unwrap();
    assert_eq!(i.module().page_size(), Some(1));
    assert_eq!(i.memory().len(), 100);
    assert_eq!(call(&mut i, "peek", &[99]), Ok(b'd' as u64));
    assert_eq!(call(&mut i, "peek", &[100]), Err(Error::MemoryOutOfBounds));
    assert_eq!(call(&mut i, "grow", &[150]), Ok(100));
    assert_eq!(call(&mut i, "size", &[]), Ok(250));
    assert_eq!(call(&mut i, "grow", &[1]), Ok(u32::MAX as u64));

    // FIB: 1 type... counted by hand: 3 functions, one `if` with `else`
    // (3 entries), one `loop` (none).
    let f = wasm_micro::footprint(wasm(FIB)).unwrap();
    assert_eq!(f.index, 4 * 2 + 8 * 3 + 8 * 3);
    assert_eq!((f.scratch, f.globals, f.table), (8, 0, 0));
    assert!(f.index <= wasm_micro::index_bound(wasm(FIB).len()));
}

#[test]
fn memory_grows_to_the_smaller_of_both_maxima() {
    let mut i = instance(MEM, &Limits::default()).unwrap();
    assert_eq!(call(&mut i, "grow", &[3]), Ok(1));
    assert_eq!(call(&mut i, "grow", &[1]), Ok(u32::MAX as u64));
    assert_eq!(i.memory().len(), 4 * 65536);
    let mut i = instance(
        MEM,
        &Limits {
            memory_bytes: 2 * 65536,
            ..Limits::default()
        },
    )
    .unwrap();
    assert_eq!(call(&mut i, "grow", &[1]), Ok(1));
    assert_eq!(call(&mut i, "grow", &[1]), Ok(u32::MAX as u64));
    let l = Limits {
        memory_bytes: 0,
        ..Limits::default()
    };
    assert_eq!(instance(MEM, &l).err(), Some(Error::Limit));
}

#[test]
fn globals_tables_start_and_custom_sections() {
    let bytes = wasm(
        r#"(module
        (type $t (func (result i32)))
        (global $g (export "g") (mut i32) (i32.const 1))
        (table 3 funcref)
        (elem (i32.const 1) $seven $long)
        (func $seven (result i32) (i32.const 7))
        (func $long (result i64) (i64.const 7))
        (func $init (global.set $g (i32.const 5)))
        (start $init)
        (func (export "via") (param i32) (result i32) (call_indirect (type $t) (local.get 0)))
        (@custom "mw.channels" "abc"))"#,
    );
    let module = Module::new(bytes).unwrap();
    assert_eq!(module.custom_section("mw.channels"), Some(&b"abc"[..]));
    assert_eq!(module.custom_section("other"), None);
    assert_eq!(module.export_func("g"), None);
    let g = module.export_global("g").unwrap();
    let mut i = Instance::new(module, &Limits::default(), &mut ()).unwrap();
    assert_eq!(i.global(g), Some(5));
    assert_eq!(call(&mut i, "via", &[1]), Ok(7));
    assert_eq!(call(&mut i, "via", &[0]), Err(Error::UninitializedElement));
    assert_eq!(call(&mut i, "via", &[2]), Err(Error::SignatureMismatch));
    assert_eq!(call(&mut i, "via", &[3]), Err(Error::UndefinedElement));
}

#[test]
fn features_outside_the_scope_are_refused_at_load() {
    for text in [
        r#"(module (import "a" "m" (memory 1)))"#,
        r#"(module (import "a" "g" (global i32)))"#,
        r#"(module (memory 1) (memory 1))"#,
        r#"(module (memory i64 1))"#,
        r#"(module (memory 1) (data "x") (func (memory.init 0 (i32.const 0) (i32.const 0) (i32.const 0))))"#,
        r#"(module (table 1 funcref) (func (drop (table.size 0))))"#,
        r#"(module (func (param v128)))"#,
        r#"(module (func $f (return_call $f)))"#,
    ] {
        assert_eq!(
            Module::new(wasm(text)).err(),
            Some(Error::Unsupported),
            "{text}"
        );
    }
    assert_eq!(
        Module::new(b"\0asm\x02\0\0\0").err(),
        Some(Error::Malformed)
    );
    assert_eq!(Module::new(b"").err(), Some(Error::Malformed));
}

/// Hand-assembled: one function `() -> i32` with the given body.
fn raw(body: &[u8]) -> Vec<u8> {
    let mut m =
        b"\0asm\x01\0\0\0\x01\x05\x01\x60\0\x01\x7f\x03\x02\x01\0\x07\x05\x01\x01f\0\0".to_vec();
    m.extend([0x0A, body.len() as u8 + 4, 1, body.len() as u8 + 2, 0]);
    m.extend(body);
    m.push(0x0B);
    m
}

#[test]
fn ill_typed_modules_trap_instead_of_misbehaving() {
    let run = |body: &[u8]| {
        let bytes: &'static [u8] = raw(body).leak();
        let mut i = Instance::new(Module::new(bytes)?, &Limits::default(), &mut ())?;
        call(&mut i, "f", &[])
    };
    assert_eq!(run(&[0x41, 0x2A]), Ok(42));
    // i32.add on an empty stack.
    assert_eq!(run(&[0x6A]), Err(Error::Invalid));
    // Nothing to return.
    assert_eq!(run(&[]), Err(Error::Invalid));
    // local.get of a local that is far outside the stack.
    assert_eq!(run(&[0x20, 0xFF, 0xFF, 0xFF, 0x0F]), Err(Error::Invalid));
    // Branch out of the function's own labels, a missing global, no memory.
    assert_eq!(run(&[0x0C, 0x05]), Err(Error::Invalid));
    assert_eq!(run(&[0x23, 0x00]), Err(Error::Invalid));
    assert_eq!(
        run(&[0x41, 0x00, 0x28, 0x02, 0x00]),
        Err(Error::MemoryOutOfBounds)
    );
    // Call of a function that does not exist; unbalanced `end`.
    assert_eq!(run(&[0x10, 0x09]), Err(Error::Invalid));
    assert_eq!(
        Module::new(&raw(&[0x0B, 0x41, 0x00])[..]).err(),
        Some(Error::Malformed)
    );
}

#[cfg(not(feature = "float"))]
#[test]
fn without_the_float_feature_arithmetic_traps_and_bits_still_move() {
    let text = r#"(module
        (func (export "bits") (result i32) (i32.reinterpret_f32 (f32.const 1.5)))
        (func (export "add") (result i32) (i32.trunc_f32_s (f32.add (f32.const 1) (f32.const 2)))))"#;
    let mut i = instance(text, &Limits::default()).unwrap();
    assert_eq!(call(&mut i, "bits", &[]), Ok(0x3FC0_0000));
    assert_eq!(call(&mut i, "add", &[]), Err(Error::Unsupported));
}

/// Corrupts a module many ways and runs whatever still loads. Nothing may
/// panic (this build has overflow checks on) or run away.
#[test]
fn corrupted_modules_never_panic() {
    let good = wat::parse_str(
        r#"(module
        (import "mw" "log" (func $log (param i32 i32)))
        (type $t (func (param i32) (result i32)))
        (memory 1 2)
        (global $g (mut i64) (i64.const 3))
        (table 2 funcref)
        (elem (i32.const 0) $a $f)
        (data (i32.const 8) "abcdefgh")
        (func $a (param i32) (result i32) (local i64)
            (local.set 1 (i64.load (i32.const 8)))
            (global.set $g (i64.add (global.get $g) (local.get 1)))
            (i32.wrap_i64 (i64.rotl (local.get 1) (i64.extend_i32_u (local.get 0)))))
        (func $f (export "f") (param i32) (result i32) (local i32)
            (block $out (result i32)
                (loop $l
                    (local.set 1 (i32.add (local.get 1) (call_indirect (type $t) (local.get 0) (i32.const 0))))
                    (call $log (i32.const 8) (local.get 1))
                    (memory.copy (i32.const 100) (i32.const 8) (i32.const 8))
                    (memory.fill (i32.const 200) (local.get 1) (i32.const 9))
                    (local.set 0 (i32.sub (local.get 0) (i32.const 1)))
                    (br_if $l (local.get 0))
                    (br_table $out $out (local.get 1) (i32.div_s (local.get 1) (i32.const 3))))
                (if (result i32) (memory.grow (i32.const 1)) (then (i32.const 1)) (else (select (i32.const 2) (i32.const 3) (local.get 1)))))))"#,
    )
    .unwrap();
    struct Ok0;
    impl Host for Ok0 {
        fn call(&mut self, _: u32, _: &mut [u8], _: &mut [u64]) -> Result<(), Error> {
            Ok(())
        }
    }
    let limits = Limits {
        memory_bytes: 1 << 20,
        fuel: 3_000,
        stack: 256,
        frames: 16,
        labels: 64,
        table: 64,
    };
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let (mut loaded, mut returned) = (0, 0);
    for round in 0..30_000 {
        let mut bytes = good.clone();
        // Round 0 runs the module as written.
        let edits = if round == 0 { 0 } else { 1 + round % 3 };
        for _ in 0..edits {
            let at = 8 + next() as usize % (bytes.len() - 8);
            match next() % 3 {
                0 => bytes[at] = next() as u8,
                1 => bytes[at] ^= 1 << (next() % 8),
                _ => bytes[at] = bytes[at].wrapping_add(1),
            }
        }
        if round % 50 == 49 {
            bytes.truncate(8 + next() as usize % (bytes.len() - 8));
        }
        let Ok(module) = Module::new(&bytes[..]) else {
            continue;
        };
        let f = module.export_func("f");
        let Ok(mut inst) = Instance::new(module, &limits, &mut Ok0) else {
            continue;
        };
        loaded += 1;
        let Some((p, q)) = f.and_then(|f| inst.module().func_type(f)) else {
            continue;
        };
        let (f, args, mut out) = (f.unwrap(), vec![3; p.len()], vec![0; q.len()]);
        inst.set_fuel(3_000);
        let r = inst.call(&mut Ok0, f, &args, &mut out);
        assert!(round != 0 || r.is_ok(), "{r:?}");
        returned += r.is_ok() as u32;
    }
    // The corruption is not so heavy that nothing runs.
    assert!(
        loaded > 3_000 && returned > 1_000,
        "{loaded} loaded, {returned} returned"
    );
}
