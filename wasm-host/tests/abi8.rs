//! End-to-end tests of ABI 8 channels with a WAT payload: its
//! `mw.channels` section declares `audio` (an input of microphone level
//! batches, required) and `level` (an optional output). `init` opens both
//! and keeps the handles (at 100 and 104); `on_message` republishes on
//! `level`. The binding decides what `audio` is: a local source the
//! runtime starts through the device, or a key. Unbound, mismatched and
//! refused bindings fail the load naming the channel; an ABI 7 host does
//! not know the imports.

use middleware::Key;
use middleware::loader::binding::{Binding, Target};
use middleware::loader::trust::Version;
use middleware::stream::samples::{SampleFormat, SensorKind};
use middleware::wasm_abi::channels::{
    Channel, Direction, OUTPUT_BASE, SECTION, encode, encoded_len,
};
use middleware::wasm_abi::import_in;
use middleware::wasm_abi::route::{AudioMode, ImuConfig, MicConfig, RouteSource, SampleSource};
use wasm_host::channels::BindError;
use wasm_host::{AbiError, Device, Limits, LoadError, Payload, Pull};

#[derive(Default)]
struct Board {
    routes: Vec<(SampleSource, String)>,
    published: Vec<(String, Vec<u8>)>,
    refuse: bool,
}

impl Device for Board {
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
    fn route_samples(&mut self, source: SampleSource, key: Key<'_>) -> Result<u8, AbiError> {
        if self.refuse {
            return Err(AbiError::InvalidArgument);
        }
        self.routes.push((source, key.as_str().to_string()));
        Ok(self.routes.len() as u8 - 1)
    }
}

const DECLS: &[Channel<'static>] = &[
    Channel::samples(
        "audio",
        Direction::In,
        Some(SensorKind::Microphone),
        8_000,
        48_000,
        true,
    )
    .with_formats(1 << SampleFormat::Levels as u8),
    Channel::bytes("level", Direction::Out, false),
];
const LEN: usize = encoded_len(DECLS);
static SECTION_BYTES: [u8; LEN] = encode::<LEN>(DECLS);

/// The payload, with `section` as its `mw.channels` (none: no section).
fn module(section: Option<&[u8]>) -> Vec<u8> {
    let mut wasm = wat::parse_str(
        r#"(module
  (import "mw" "channel_open" (func $open (param i32 i32) (result i32)))
  (import "mw" "channel_publish" (func $pub (param i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 32) "audio")
  (data (i32.const 48) "level")
  (func (export "init")
    (i32.store (i32.const 100) (call $open (i32.const 32) (i32.const 5)))
    (i32.store (i32.const 104) (call $open (i32.const 48) (i32.const 5))))
  (func (export "mw_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "on_message") (param $slot i32) (param $p i32) (param $n i32)
    (i32.store (i32.const 108) (call $pub (i32.load (i32.const 104)) (local.get $p) (local.get $n)))))"#,
    )
    .expect("WAT");
    if let Some(body) = section {
        let mut s = vec![SECTION.len() as u8];
        s.extend_from_slice(SECTION.as_bytes());
        s.extend_from_slice(body);
        wasm.push(0);
        // LEB128 of the section length (fits two bytes).
        let n = s.len();
        if n < 128 {
            wasm.push(n as u8);
        } else {
            wasm.extend_from_slice(&[(n as u8 & 0x7f) | 0x80, (n >> 7) as u8]);
        }
        wasm.extend_from_slice(&s);
    }
    wasm
}

fn levels(rate: u16) -> RouteSource {
    RouteSource::Samples(SampleSource::Microphone(
        MicConfig::new(rate, 10, AudioMode::Levels, 10, 160).unwrap(),
    ))
}

fn load(board: Board, binding: Option<&Binding<'_>>) -> Result<Payload<Board>, LoadError> {
    let c = wasm_host::Compiled::new(&module(Some(&SECTION_BYTES)), Limits::default())?;
    assert_eq!(c.channels().len(), 2);
    Payload::instantiate_bound(c, board, binding)
}

fn i32_at(p: &Payload<Board>, at: usize) -> i32 {
    i32::from_le_bytes(p.memory()[at..at + 4].try_into().unwrap())
}

fn bound<'a>(entries: &[(&'a str, Target<'a>)]) -> Binding<'a> {
    let mut b = Binding::new([0; 32], Version(1));
    for (n, t) in entries {
        b.bind(n, *t).unwrap();
    }
    b
}

#[test]
fn bd4_a_local_source_starts_on_the_internal_key_before_init() {
    let b = bound(&[
        ("audio", Target::Local(levels(16_000))),
        ("level", Target::Key(Key::from_static("lab/level"))),
    ]);
    let mut p = load(Board::default(), Some(&b)).unwrap();
    let SampleSource::Microphone(cfg) = p.device().routes[0].0 else {
        panic!()
    };
    assert_eq!(
        (cfg.rate_hz, p.device().routes[0].1.as_str()),
        (16_000, "ch/audio")
    );
    assert_eq!(p.subscriptions().collect::<Vec<_>>(), ["ch/audio"]);
    // `channel_open` gave the slot and the output handle.
    assert_eq!((i32_at(&p, 100), i32_at(&p, 104)), (0, OUTPUT_BASE + 1));
    assert!(p.deliver(Key::from_static("ch/audio"), b"batch").unwrap());
    assert_eq!(i32_at(&p, 108), 0);
    assert_eq!(
        p.device().published,
        [("lab/level".to_string(), b"batch".to_vec())]
    );
}

#[test]
fn bd4_a_key_target_subscribes_and_an_unbound_optional_output_is_unsupported() {
    let b = bound(&[("audio", Target::Key(Key::from_static("mic/level")))]);
    let mut p = load(Board::default(), Some(&b)).unwrap();
    assert!(p.device().routes.is_empty());
    assert_eq!(p.subscriptions().collect::<Vec<_>>(), ["mic/level"]);
    assert_eq!(i32_at(&p, 104), AbiError::Unsupported.code());
    assert!(p.deliver(Key::from_static("mic/level"), b"x").unwrap());
    // Publishing on the failed handle is refused, nothing published.
    assert_eq!(i32_at(&p, 108), AbiError::InvalidArgument.code());
    assert!(p.device().published.is_empty());
}

#[test]
fn bd4_bad_bindings_fail_the_load_naming_the_channel() {
    let bind_err = |b: Option<&Binding<'_>>, board| match load(board, b) {
        Err(LoadError::Bind(e)) => e,
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("loaded"),
    };
    assert_eq!(
        bind_err(None, Board::default()),
        BindError::Unbound("audio".into())
    );
    let imu = RouteSource::Samples(SampleSource::Imu(
        ImuConfig::new(100, 2, false, 10).unwrap(),
    ));
    assert_eq!(
        bind_err(
            Some(&bound(&[("audio", Target::Local(imu))])),
            Board::default()
        ),
        BindError::Mismatch("audio".into())
    );
    let refusing = Board {
        refuse: true,
        ..Board::default()
    };
    assert_eq!(
        bind_err(
            Some(&bound(&[("audio", Target::Local(levels(16_000)))])),
            refusing
        ),
        BindError::Route("audio".into(), AbiError::InvalidArgument)
    );
    let e = LoadError::Bind(BindError::Unbound("audio".into()));
    assert_eq!(
        e.to_string(),
        "binding: channel audio is required and not bound"
    );
}

#[test]
fn ch3_a_malformed_section_is_refused_and_modules_without_one_ignore_bindings() {
    let mut bad = SECTION_BYTES.to_vec();
    bad.push(0);
    let e = wasm_host::Compiled::new(&module(Some(&bad)), Limits::default()).err();
    assert!(matches!(e, Some(LoadError::Channels(_))));
    let c = wasm_host::Compiled::new(&module(None), Limits::default()).unwrap();
    assert!(c.channels().is_empty());
    let b = bound(&[("audio", Target::Local(levels(16_000)))]);
    let mut p = Payload::instantiate_bound(c, Board::default(), Some(&b)).unwrap();
    // No declaration: the names are not channels.
    assert!(p.device().routes.is_empty());
    assert_eq!(i32_at(&p, 100), AbiError::InvalidArgument.code());
}

#[test]
fn ch2_an_abi7_host_does_not_know_the_channel_imports() {
    for name in ["channel_open", "channel_publish"] {
        assert!(import_in(7, name).is_none());
        assert!(import_in(8, name).is_some());
    }
}

#[test]
fn pr3_a_remote_channel_takes_its_node_only() {
    use middleware::SenderId;
    let node = SenderId([7; 8]);
    let b = bound(&[
        (
            "audio",
            Target::Remote {
                node,
                key: Key::from_static("mic/level"),
            },
        ),
        (
            "level",
            Target::Remote {
                node,
                key: Key::from_static("lab/level"),
            },
        ),
    ]);
    let mut p = load(Board::default(), Some(&b)).unwrap();
    let k = Key::from_static("mic/level");
    assert!(!p.deliver(k, b"x").unwrap(), "an unknown sender");
    assert!(
        !p.deliver_from(k, b"x", Some(SenderId([8; 8]))).unwrap(),
        "another node"
    );
    assert!(p.device().published.is_empty());
    assert!(p.deliver_from(k, b"ok", Some(node)).unwrap());
    // The output publishes on its key, normally.
    assert_eq!(
        p.device().published,
        [("lab/level".to_string(), b"ok".to_vec())]
    );
}
