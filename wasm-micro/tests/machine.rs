//! `Machine`: a module run in buffers the caller owns, with no allocator.
//! (The interpreter underneath is the one `Instance` uses, which the
//! specification suite covers; these tests are about the buffers.)

use wasm_micro::{Buffers, Error, Host, Limits, Machine, footprint};

const LIMITS: Limits = Limits {
    stack: 64,
    frames: 8,
    labels: 24,
    memory_bytes: usize::MAX,
    table: 16,
    fuel: 1_000_000,
};

const PROGRAM: &str = r#"(module
    (import "mw" "add" (func $add (param i32 i32) (result i32)))
    (type $t (func (param i32) (result i32)))
    (memory (export "memory") 1 2)
    (global $calls (mut i32) (i32.const 0))
    (table 2 funcref)
    (elem (i32.const 0) $fib $peek)
    (data (i32.const 8) "wasm")
    (func $fib (param i32) (result i32)
        (global.set $calls (i32.add (global.get $calls) (i32.const 1)))
        (if (result i32) (i32.lt_u (local.get 0) (i32.const 2))
            (then (local.get 0))
            (else (call $add (call $fib (i32.sub (local.get 0) (i32.const 1)))
                             (call $fib (i32.sub (local.get 0) (i32.const 2)))))))
    (func $peek (param i32) (result i32) (i32.load8_u (local.get 0)))
    (func (export "via") (param i32 i32) (result i32)
        (call_indirect (type $t) (local.get 0) (local.get 1)))
    (func (export "calls") (result i32) (global.get $calls))
    (func (export "grow") (param i32) (result i32) (memory.grow (local.get 0)))
    (func (export "local") (result i32) (local i32) (local.get 0)))"#;

struct Adder(u32);
impl Host for Adder {
    fn call(&mut self, _: u32, mem: &mut [u8], vals: &mut [u64]) -> Result<(), Error> {
        self.0 += 1;
        mem[0] = 0xAB;
        vals[0] = (vals[0] as u32).wrapping_add(vals[1] as u32) as u64;
        Ok(())
    }
}

fn call<B: AsRef<[u8]>>(
    m: &mut Machine<B>,
    bufs: &mut Buffers,
    name: &str,
    args: &[u64],
) -> Result<u64, Error> {
    let f = m.export_func(bufs, name).unwrap();
    let mut out = [0];
    m.call(bufs, &mut Adder(0), f, args, &mut out)?;
    Ok(out[0])
}

#[test]
fn runs_in_buffers_of_exactly_the_footprint() {
    let wasm = wat::parse_str(PROGRAM).unwrap();
    let need = footprint(&wasm).unwrap();
    let (nw, ns) = (need.words(&LIMITS), need.slots(&LIMITS));
    // Stale contents everywhere: nothing may depend on zeroed buffers.
    let (mut words, mut slots, mut memory) = (
        vec![0xDEAD_BEEF; nw],
        vec![u64::MAX; ns],
        vec![0x55u8; 2 * 65536],
    );
    let mut bufs = Buffers {
        words: &mut words,
        slots: &mut slots,
        memory: &mut memory,
    };
    let mut host = Adder(0);
    let mut m = Machine::new(&wasm[..], &LIMITS, &mut bufs, &mut host).unwrap();

    assert_eq!(m.page_size(), Some(65536));
    assert_eq!(
        m.imports(&bufs)
            .map(|i| i.name.to_vec())
            .collect::<Vec<_>>(),
        [b"add"]
    );
    assert_eq!(m.exports(&bufs).count(), 5);
    assert_eq!(m.memory(&bufs).len(), 65536);
    assert_eq!(&m.memory(&bufs)[8..12], b"wasm");
    assert!(
        m.memory(&bufs)[12..].iter().all(|b| *b == 0),
        "memory starts zeroed"
    );

    let via = m.export_func(&bufs, "via").unwrap();
    let mut out = [0];
    m.call(&mut bufs, &mut host, via, &[6, 0], &mut out)
        .unwrap();
    assert_eq!(
        (out[0], host.0),
        (8, 12),
        "fib 6 through the table, 12 additions by the host"
    );
    assert_eq!(m.memory(&bufs)[0], 0xAB, "the host wrote to memory");
    assert_eq!(call(&mut m, &mut bufs, "via", &[9, 1]), Ok(b'a' as u64));
    assert_eq!(
        call(&mut m, &mut bufs, "calls", &[]),
        Ok(25),
        "globals persist between calls"
    );
    assert_eq!(
        call(&mut m, &mut bufs, "local", &[]),
        Ok(0),
        "locals are zeroed"
    );

    // Growth within the buffer: the new page reads as zero, not as 0x55.
    assert_eq!(call(&mut m, &mut bufs, "grow", &[1]), Ok(1));
    assert_eq!(m.memory(&bufs).len(), 2 * 65536);
    assert!(m.memory(&bufs)[65536..].iter().all(|b| *b == 0));
    assert_eq!(call(&mut m, &mut bufs, "grow", &[1]), Ok(u32::MAX as u64));

    // Fuel and depth limits end a call; the machine stays usable.
    m.set_fuel(50);
    assert_eq!(
        call(&mut m, &mut bufs, "via", &[20, 0]),
        Err(Error::OutOfFuel)
    );
    m.set_fuel(1_000_000);
    assert_eq!(
        call(&mut m, &mut bufs, "via", &[9, 0]),
        Err(Error::StackOverflow)
    );
    assert_eq!(call(&mut m, &mut bufs, "via", &[5, 0]), Ok(5));
    assert!(m.fuel() < 1_000_000);
}

#[test]
fn buffers_one_short_are_refused() {
    let wasm = wat::parse_str(PROGRAM).unwrap();
    let need = footprint(&wasm).unwrap();
    let (nw, ns) = (need.words(&LIMITS), need.slots(&LIMITS));
    let run = |nw: usize, ns: usize, nm: usize| {
        let (mut words, mut slots, mut memory) = (vec![0; nw], vec![0; ns], vec![0u8; nm]);
        let mut bufs = Buffers {
            words: &mut words,
            slots: &mut slots,
            memory: &mut memory,
        };
        let mut m = Machine::new(&wasm[..], &LIMITS, &mut bufs, &mut Adder(0))?;
        call(&mut m, &mut bufs, "via", &[9, 1])
    };
    assert_eq!(run(nw, ns, 65536), Ok(b'a' as u64));
    assert_eq!(run(nw - 1, ns, 65536), Err(Error::OutOfMemory));
    assert_eq!(run(nw, ns - 1, 65536), Err(Error::OutOfMemory));
    assert_eq!(
        run(need.index / 4 - 1, ns, 65536),
        Err(Error::OutOfMemory),
        "not even the index"
    );
    // A memory buffer shorter than a page backs the start of it; the data
    // segment at 8 still fits, an access past the buffer traps.
    assert_eq!(run(nw, ns, 4096), Ok(b'a' as u64));
    assert_eq!(run(nw, ns, 8), Err(Error::SegmentOutOfBounds));
    let (mut words, mut slots, mut memory) = (vec![0; nw], vec![0; ns], vec![0u8; 4096]);
    let mut bufs = Buffers {
        words: &mut words,
        slots: &mut slots,
        memory: &mut memory,
    };
    let mut m = Machine::new(&wasm[..], &LIMITS, &mut bufs, &mut Adder(0)).unwrap();
    assert_eq!(call(&mut m, &mut bufs, "via", &[4095, 1]), Ok(0));
    assert_eq!(
        call(&mut m, &mut bufs, "via", &[4096, 1]),
        Err(Error::MemoryOutOfBounds)
    );
    assert_eq!(call(&mut m, &mut bufs, "grow", &[1]), Ok(u32::MAX as u64));
}

#[test]
fn a_second_module_in_the_same_buffers_sees_nothing_of_the_first() {
    let (mut words, mut slots, mut memory) = (vec![0; 512], vec![0; 128], vec![0u8; 65536]);
    let mut bufs = Buffers {
        words: &mut words,
        slots: &mut slots,
        memory: &mut memory,
    };
    let first = wat::parse_str(PROGRAM).unwrap();
    let mut m = Machine::new(&first[..], &LIMITS, &mut bufs, &mut Adder(0)).unwrap();
    assert_eq!(call(&mut m, &mut bufs, "via", &[7, 0]), Ok(13));
    // Reads a local far above its own frame, an empty table slot and memory.
    let second = wat::parse_str(
        r#"(module (type $t (func (result i32))) (memory 1) (table 2 funcref)
            (func (export "peek") (param i32) (result i32) (i32.load (local.get 0)))
            (func (export "slot") (result i32) (call_indirect (type $t) (i32.const 0))))"#,
    )
    .unwrap();
    let mut m = Machine::new(&second[..], &LIMITS, &mut bufs, &mut ()).unwrap();
    assert!(m.memory(&bufs).iter().all(|b| *b == 0));
    assert!(
        bufs.slots.iter().all(|s| *s == 0),
        "globals and stack cleared"
    );
    let f = m.export_func(&bufs, "slot").unwrap();
    let mut out = [0];
    assert_eq!(
        m.call(&mut bufs, &mut (), f, &[], &mut out),
        Err(Error::UninitializedElement)
    );
}

#[test]
fn the_wrong_buffers_fail_without_panicking() {
    let wasm = wat::parse_str(PROGRAM).unwrap();
    let (mut words, mut slots, mut memory) = (vec![0; 512], vec![0; 128], vec![0u8; 65536]);
    let mut bufs = Buffers {
        words: &mut words,
        slots: &mut slots,
        memory: &mut memory,
    };
    let mut m = Machine::new(&wasm[..], &LIMITS, &mut bufs, &mut Adder(0)).unwrap();
    let via = m.export_func(&bufs, "via").unwrap();
    // Buffers that never held this module: lookups fail, nothing panics.
    for fill in [0u32, 7, u32::MAX] {
        let (mut words, mut slots, mut memory) = (vec![fill; 512], vec![0; 128], vec![0u8; 65536]);
        let mut other = Buffers {
            words: &mut words,
            slots: &mut slots,
            memory: &mut memory,
        };
        let mut out = [0];
        assert!(
            m.call(&mut other, &mut Adder(0), via, &[6, 0], &mut out)
                .is_err()
        );
        assert_eq!(
            m.export_func(&other, "via"),
            Some(via),
            "exports come from the image"
        );
    }
    let mut out = [0];
    let mut none = Buffers {
        words: &mut [],
        slots: &mut [],
        memory: &mut [],
    };
    assert_eq!(
        m.call(&mut none, &mut Adder(0), via, &[6, 0], &mut out),
        Err(Error::OutOfMemory)
    );
    assert_eq!(
        m.call(&mut bufs, &mut Adder(0), via, &[6, 0], &mut out),
        Ok(())
    );
    assert_eq!(out[0], 8);
}
