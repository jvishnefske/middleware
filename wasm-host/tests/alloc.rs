//! After a successful load, calls into a payload allocate only what
//! [`Limits::load_heap_needed`] already counts (requirement W10):
//! translation is eager, the value stack is allocated in full on the first
//! call (`init`, inside the load) and the host buffers are reserved at
//! load, so the one thing that still grows is wasmi's call frame list, when
//! a call nests deeper than any before it, within
//! [`heap::PER_CALL_DEPTH`] per level of [`Limits::max_call_depth`].
//! Once grown it keeps its capacity: further calls allocate nothing.
//!
//! Its own test binary without the libtest harness (`harness = false` in
//! `Cargo.toml`): the counting allocator ([`peak_alloc`]) is process wide.

use middleware::Key;
use peak_alloc::PeakAlloc;
use wasm_host::{AbiError, Device, Limits, Payload, Pull, Ticked, heap};

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
}
