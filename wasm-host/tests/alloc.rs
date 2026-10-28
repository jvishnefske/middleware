//! After a successful load, calls into a payload allocate only what
//! [`Limits::load_heap_needed`] already counts (requirement W10):
//! translation is eager, the value stack is allocated in full on the first
//! call (`init`, inside the load) and the host buffers are reserved at
//! load, so the one thing that still grows is wasmi's call frame list, when
//! a call nests deeper than any before it, within
//! [`heap::PER_CALL_DEPTH`] per level of [`Limits::max_call_depth`].
//! Once grown it keeps its capacity: further calls allocate nothing.
//!
//! Then repeated refused and failed starts must return every byte they
//! allocated (see [`refused_and_failed_starts_return_every_byte`]).
//!
//! Its own test binary without the libtest harness (`harness = false` in
//! `Cargo.toml`): the counting allocator ([`peak_alloc`]) is process wide.

use middleware::Key;
use peak_alloc::PeakAlloc;
use wasm_host::{AbiError, Compiled, Device, Limits, LoadError, Payload, Pull, Ticked, heap};

#[global_allocator]
static PEAK: PeakAlloc = PeakAlloc;

/// A board that records nothing on the heap.
#[derive(Default)]
struct Quiet {
    logs: u32,
    published: u32,
}

impl Device for Quiet {
    fn log(&mut self, _line: &str) {
        self.logs += 1;
    }
    fn publish(&mut self, _key: Key<'_>, _data: &[u8]) -> Result<(), AbiError> {
        self.published += 1;
        Ok(())
    }
    fn now_ms(&mut self) -> u32 {
        0
    }
    fn gpio_output(&mut self, _pin: u8) -> Result<(), AbiError> {
        Ok(())
    }
    fn gpio_input(&mut self, _pin: u8, _pull: Pull) -> Result<(), AbiError> {
        Ok(())
    }
    fn gpio_write(&mut self, _pin: u8, _high: bool) -> Result<(), AbiError> {
        Ok(())
    }
    fn gpio_read(&mut self, _pin: u8) -> Result<bool, AbiError> {
        Ok(false)
    }
}

/// `init` subscribes and recurses 8 deep; `tick` recurses 20 deep (more
/// than `init`, so the frame list would have to grow) and logs and
/// publishes; `on_message` logs.
const PAYLOAD: &str = r#"
(module
  (import "mw" "log" (func $log (param i32 i32)))
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "subscribe" (func $subscribe (param i32 i32) (result i32)))
  (import "mw" "set_tick" (func $set_tick (param i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "in/x")
  (data (i32.const 16) "out/y")
  (func $f (param $n i32)
    (if (local.get $n)
      (then (call $f (i32.sub (local.get $n) (i32.const 1))))))
  (func (export "init")
    (drop (call $subscribe (i32.const 0) (i32.const 4)))
    (drop (call $set_tick (i32.const 10)))
    (call $f (i32.const 8)))
  (func (export "tick") (param i32)
    (call $f (i32.const 20))
    (call $log (i32.const 16) (i32.const 5))
    (drop (call $publish (i32.const 16) (i32.const 5) (i32.const 0) (i32.const 4))))
  (func (export "mw_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "on_message") (param i32 i32 i32)
    (call $log (local.get 1) (local.get 2))))
"#;

fn main() {
    let wasm = wat::parse_str(PAYLOAD).expect("valid WAT");
    let limits = Limits::default();
    let mut p = Payload::load(&wasm, Quiet::default(), limits).expect("loads");
    let key = Key::new("in/x").expect("key");

    // The first tick nests deeper than `init` did: the frame list grows,
    // within its share of the bound.
    let resident = PEAK.current_usage();
    PEAK.reset_peak_usage();
    assert_eq!(p.tick(10), Ok(Ticked::Ran));
    let grown = PEAK.peak_usage() - resident;
    let allowed = (limits.max_call_depth * heap::PER_CALL_DEPTH) as usize;
    assert!(
        grown <= allowed,
        "frame list grew {grown} > {allowed} bytes"
    );

    // Nothing allocates any more.
    let resident = PEAK.current_usage();
    PEAK.reset_peak_usage();
    for i in 1..200u32 {
        assert_eq!(p.tick(10 * (i + 1)), Ok(Ticked::Ran));
        assert_eq!(p.deliver(key, b"hello"), Ok(true));
    }
    assert_eq!(p.device().published, 200);
    assert_eq!(p.device().logs, 399);
    let peak = PEAK.peak_usage();
    assert_eq!(
        peak,
        resident,
        "calls after the deepest one allocated {} bytes",
        peak - resident
    );
    assert_eq!(PEAK.current_usage(), resident);
    println!("frame list grew {grown} of {allowed} bytes once; 398 later calls allocated nothing");
    drop(p);

    refused_and_failed_starts_return_every_byte(&wasm, limits);
}

/// `init` traps (unreachable) after subscribing: the load fails.
const TRAPS_IN_INIT: &str = r#"
(module
  (import "mw" "subscribe" (func $subscribe (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "in/x")
  (func (export "init")
    (drop (call $subscribe (i32.const 0) (i32.const 4)))
    unreachable))
"#;

/// Repeated refused or failed starts lose no heap (requirement W10): each
/// path a firmware takes when a start does not go ahead, run 20 times,
/// leaves exactly the heap in use before it.
///
/// * refused: the image is read into a buffer, [`Limits::heap_needed`]
///   says it does not fit, the buffer is dropped (esp-base's controller);
/// * compile error: [`Compiled::new`] rejects the image;
/// * compiled, then dropped without instantiating;
/// * `init` traps: [`Payload::instantiate`] fails after allocating linear
///   memory, the value stack and the host buffers;
/// * loaded, then dropped (a stop).
fn refused_and_failed_starts_return_every_byte(wasm: &[u8], limits: Limits) {
    let trap = wat::parse_str(TRAPS_IN_INIT).expect("valid WAT");
    let garbage = [0u8, b'a', b's', b'm', 1, 0, 0, 0, 0xff];
    let before = PEAK.current_usage();
    for round in 0..20 {
        // Refused: the firmware reads the image, sizes it, gives it back.
        let mut image = Vec::new();
        image.try_reserve_exact(wasm.len()).expect("room");
        image.extend_from_slice(wasm);
        assert!(limits.heap_needed(&image) > 0);
        drop(image);
        assert!(Compiled::new(&garbage, limits).is_err());
        drop(Compiled::new(wasm, limits).expect("compiles"));
        let compiled = Compiled::new(&trap, limits).expect("compiles");
        assert!(matches!(
            Payload::instantiate(compiled, Quiet::default()),
            Err(LoadError::Init { .. })
        ));
        drop(Payload::load(wasm, Quiet::default(), limits).expect("loads"));
        let now = PEAK.current_usage();
        assert_eq!(
            now,
            before,
            "round {round}: {} bytes still held",
            now.abs_diff(before)
        );
    }
    println!("20 rounds of refused, failed and stopped starts returned every byte");
}
