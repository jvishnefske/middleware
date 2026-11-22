//! End-to-end tests of ABI 9 machine channels with a WAT payload: its
//! `mw.channels` section declares `x_limit` (a required digital input) and
//! `motion` (a required segments output). `init` opens both (handles at 100
//! and 104); `on_message` publishes the 17-byte segment kept at 200 on
//! `motion` (result at 108). Bound to driver points, both are carried on
//! the node-internal `drv/<point>` keys: the node delivers what its driver
//! reads there and takes what the payload publishes there.

use middleware::Key;
use middleware::loader::binding::{Binding, Target, is_internal_key};
use middleware::loader::trust::Version;
use middleware::wasm_abi::channels::{
    Channel, DataKind, Direction, OUTPUT_BASE, SECTION, encode, encoded_len,
};
use middleware::wasm_abi::machine::{Digital, Segment};
use middleware::wasm_abi::{ABI_VERSION, imports_in};
use wasm_host::channels::BindError;
use wasm_host::{AbiError, Device, Limits, LoadError, Payload, Pull};

#[derive(Default)]
struct Node {
    published: Vec<(String, Vec<u8>)>,
}

impl Device for Node {
    fn log(&mut self, _: &str) {}
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
}

const DECLS: &[Channel<'static>] = &[
    Channel::machine("x_limit", Direction::In, DataKind::Digital, true),
    Channel::machine("motion", Direction::Out, DataKind::Segments, true),
];
const LEN: usize = encoded_len(DECLS);
static SECTION_BYTES: [u8; LEN] = encode::<LEN>(DECLS);

const SEGMENT: Segment = Segment {
    seq: 1,
    x_um: -2_000,
    z_um: -10_000,
    feed: 800,
    flags: 0,
};

fn wat_bytes(b: &[u8]) -> String {
    b.iter().map(|x| format!("\\{x:02x}")).collect()
}

fn module(section: &[u8]) -> Vec<u8> {
    let mut wasm = wat::parse_str(format!(
        r#"(module
  (import "mw" "channel_open" (func $open (param i32 i32) (result i32)))
  (import "mw" "channel_publish" (func $pub (param i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 32) "x_limit")
  (data (i32.const 48) "motion")
  (data (i32.const 200) "{seg}")
  (func (export "init")
    (i32.store (i32.const 100) (call $open (i32.const 32) (i32.const 7)))
    (i32.store (i32.const 104) (call $open (i32.const 48) (i32.const 6))))
  (func (export "mw_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "on_message") (param $slot i32) (param $p i32) (param $n i32)
    (i32.store (i32.const 108) (call $pub (i32.load (i32.const 104)) (i32.const 200) (i32.const 17)))))"#,
        seg = wat_bytes(&SEGMENT.encode())
    ))
    .expect("WAT");
    let mut s = vec![SECTION.len() as u8];
    s.extend_from_slice(SECTION.as_bytes());
    s.extend_from_slice(section);
    wasm.push(0);
    wasm.push(s.len() as u8);
    wasm.extend_from_slice(&s);
    wasm
}

fn load(binding: Option<&Binding<'_>>) -> Result<Payload<Node>, LoadError> {
    let c = wasm_host::Compiled::new(&module(&SECTION_BYTES), Limits::default())?;
    Payload::instantiate_bound(c, Node::default(), binding)
}

fn i32_at(p: &Payload<Node>, at: usize) -> i32 {
    i32::from_le_bytes(p.memory()[at..at + 4].try_into().unwrap())
}

fn drivers<'a>(limit: &'a str, motion: &'a str) -> Binding<'a> {
    let mut b = Binding::new([0; 32], Version(1));
    b.bind("x_limit", Target::Driver(limit)).unwrap();
    b.bind("motion", Target::Driver(motion)).unwrap();
    b
}

#[test]
fn mc4_driver_points_are_carried_on_their_internal_keys() {
    let b = drivers("x_limit_min", "motion");
    let mut p = load(Some(&b)).unwrap();
    assert_eq!(p.subscriptions().collect::<Vec<_>>(), ["drv/x_limit_min"]);
    assert_eq!((i32_at(&p, 100), i32_at(&p, 104)), (0, OUTPUT_BASE + 1));
    // The driver reads the switch; the payload answers with a segment on
    // the motion point's key.
    let made = Digital { on: true, at_us: 5 }.encode();
    assert!(
        p.deliver(Key::from_static("drv/x_limit_min"), &made)
            .unwrap()
    );
    assert_eq!(i32_at(&p, 108), 0);
    let (key, data) = &p.device().published[0];
    assert_eq!(key, "drv/motion");
    assert!(
        is_internal_key(Key::new(key).unwrap()),
        "never sent on a link"
    );
    assert_eq!(Segment::decode(data), Some(SEGMENT));
}

#[test]
fn mc4_a_key_still_binds_a_machine_channel() {
    // For a model run against a simulator elsewhere on the bus.
    let mut b = Binding::new([0; 32], Version(1));
    b.bind("x_limit", Target::Key(Key::from_static("sim/x_limit")))
        .unwrap();
    b.bind("motion", Target::Key(Key::from_static("sim/motion")))
        .unwrap();
    let mut p = load(Some(&b)).unwrap();
    assert!(p.deliver(Key::from_static("sim/x_limit"), &[0; 9]).unwrap());
    assert_eq!(p.device().published[0].0, "sim/motion");
}

#[test]
fn mc4_unbound_machine_channels_fail_the_load() {
    match load(None) {
        Err(LoadError::Bind(e)) => assert_eq!(e, BindError::Unbound("x_limit".into())),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("loaded"),
    }
}

#[test]
fn mc4_an_unknown_kind_is_refused_at_compile() {
    // Kind 7 is past ABI 9, as kind 3 was past ABI 8: the section is refused.
    let mut bad = SECTION_BYTES.to_vec();
    bad[2 + 1 + "x_limit".len() + 1] = 7;
    assert!(matches!(
        wasm_host::Compiled::new(&module(&bad), Limits::default()).err(),
        Some(LoadError::Channels(_))
    ));
}

#[test]
fn mc4_abi9_adds_no_imports() {
    assert!(ABI_VERSION >= 9);
    assert_eq!(imports_in(9), imports_in(8));
}
