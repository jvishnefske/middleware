//! End-to-end tests of the ABI 5 request imports and export (`serve`,
//! `node_id`, `on_request`) with WAT payloads: a request reaches the slot
//! with its body and meta block and the reply comes back; every way a
//! payload can get it wrong (no buffer, a reply longer than offered, an
//! unknown code, running out of fuel) is a `HandlerError` or a trap, never
//! a host fault; ABI 1-4 payloads load unchanged; and a payload that
//! imports the ABI 5 functions is refused by an ABI 4 host.

use middleware::wasm_abi::serve::{MAX_REPLY, MAX_REQUEST, MAX_SERVES};
use middleware::wasm_abi::{ABI4_IMPORTS, IMPORTS, import_in};
use middleware::{HandlerError, Key, RequestMeta, SenderId};
use wasm_host::{AbiError, Device, Limits, LoadError, Payload, Pull, Trap};

const NODE: SenderId = SenderId([0x24, 0x6f, 0x28, 0xaa, 0x01, 0xfe, 0, 0]);

#[derive(Default)]
struct Board {
    served: Vec<String>,
    published: Vec<(String, Vec<u8>)>,
    /// `node_id` answers this; `None`: Unsupported.
    node: Option<SenderId>,
    /// `serve` answers Unsupported (a board that cannot route requests).
    no_routing: bool,
}

impl Board {
    fn new() -> Self {
        Board {
            node: Some(NODE),
            ..Board::default()
        }
    }
    fn results(&self) -> Vec<i32> {
        self.published
            .iter()
            .filter(|(k, _)| k == "r")
            .map(|(_, d)| i32::from_le_bytes(d.as_slice().try_into().expect("4 bytes")))
            .collect()
    }
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
    fn serve(&mut self, key: Key<'_>) -> Result<(), AbiError> {
        if self.no_routing {
            return Err(AbiError::Unsupported);
        }
        self.served.push(key.as_str().to_string());
        Ok(())
    }
    fn node_id(&mut self) -> Result<SenderId, AbiError> {
        self.node.ok_or(AbiError::Unsupported)
    }
}

/// A board implementing only the required methods: every ABI 5 call is
/// `Unsupported` by the trait's defaults.
struct OldBoard;

impl Device for OldBoard {
    fn log(&mut self, _: &str) {}
    fn publish(&mut self, _: Key<'_>, _: &[u8]) -> Result<(), AbiError> {
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

/// The module around an `init` body and an `on_request` body. Data: the
/// key `app/echo` at 32, the key `sys/x` at 48, `r` (results) at 64. The
/// request buffer lives at 1024 (`$alloc`, a global so a test can make it
/// decline or point outside memory). `$report` publishes an i32 on `r`.
fn module(init: &str, on_request: &str, alloc: i32) -> Vec<u8> {
    let src = format!(
        r#"(module
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "serve" (func $serve (param i32 i32) (result i32)))
  (import "mw" "node_id" (func $node_id (param i32) (result i32)))
  (memory (export "memory") 1)
  (global $alloc (mut i32) (i32.const {alloc}))
  (data (i32.const 32) "app/echo")
  (data (i32.const 48) "sys/x")
  (data (i32.const 64) "r")
  (func $report (param $v i32)
    (i32.store (i32.const 80) (local.get $v))
    (drop (call $publish (i32.const 64) (i32.const 1) (i32.const 80) (i32.const 4))))
  (func (export "init") {init})
  (func (export "mw_alloc") (param i32) (result i32) (global.get $alloc))
  (func (export "on_request") (param $slot i32) (param $req i32) (param $len i32)
        (param $meta i32) (param $reply i32) (param $cap i32) (result i32)
    {on_request}))"#
    );
    wat::parse_str(src).expect("WAT")
}

const SERVE_ECHO: &str = r#"(call $report (call $serve (i32.const 32) (i32.const 8)))"#;

/// Replies: slot, meta flags, deadline, the sender's first byte, then the
/// request body; fails BufferTooSmall when that does not fit.
const ECHO: &str = r#"
    (if (i32.lt_s (local.get $cap) (i32.add (local.get $len) (i32.const 4)))
      (then (return (i32.const -3))))
    (i32.store8 (local.get $reply) (local.get $slot))
    (i32.store8 (i32.add (local.get $reply) (i32.const 1)) (i32.load8_u (local.get $meta)))
    (i32.store8 (i32.add (local.get $reply) (i32.const 2))
      (i32.load8_u (i32.add (local.get $meta) (i32.const 4))))
    (i32.store8 (i32.add (local.get $reply) (i32.const 3))
      (i32.load8_u (i32.add (local.get $meta) (i32.const 8))))
    (memory.copy (i32.add (local.get $reply) (i32.const 4)) (local.get $req) (local.get $len))
    (i32.add (local.get $len) (i32.const 4))"#;

fn key(s: &str) -> Key<'_> {
    Key::new(s).expect("key")
}

fn load(wasm: &[u8], board: Board) -> Payload<Board> {
    Payload::load(wasm, board, Limits::default()).expect("loads")
}

#[test]
fn s1_a_request_reaches_its_slot_with_meta_and_the_reply_comes_back() {
    let mut p = load(&module(SERVE_ECHO, ECHO, 1024), Board::new());
    assert_eq!(p.device().results(), vec![0]);
    assert_eq!(p.serves().collect::<Vec<_>>(), vec!["app/echo"]);
    assert_eq!(p.device().served, vec!["app/echo"]);
    assert!(p.handles_requests());

    let meta = RequestMeta::NONE
        .with_sender(SenderId([9, 0, 0, 0, 0, 0, 0, 0]))
        .with_deadline_ms(200);
    let mut reply = [0u8; 16];
    let r = p.request(key("app/echo"), b"ping", &meta, &mut reply);
    assert_eq!(r, Ok(Ok(8)));
    assert_eq!(&reply[..8], &[0, 3, 200, 9, b'p', b'i', b'n', b'g']);

    // No meta: flags 0. A key nothing serves: NotFound, payload not called.
    let r = p.request(key("app/echo"), b"", &RequestMeta::NONE, &mut reply);
    assert_eq!((r, &reply[..4]), (Ok(Ok(4)), &[0u8, 0, 0, 0][..]));
    let calls = p.fuel_stats().calls;
    assert_eq!(
        p.request(key("app/other"), b"x", &RequestMeta::NONE, &mut reply),
        Ok(Err(HandlerError::NotFound))
    );
    assert_eq!(p.fuel_stats().calls, calls, "not called");
}

#[test]
fn s2_the_reply_capacity_is_the_callers_buffer_up_to_max_reply() {
    let mut p = load(&module(SERVE_ECHO, ECHO, 1024), Board::new());
    let mut small = [0u8; 6];
    assert_eq!(
        p.request(key("app/echo"), b"ping", &RequestMeta::NONE, &mut small),
        Ok(Err(HandlerError::BufferTooSmall))
    );
    // A body over MAX_REQUEST is refused before the payload runs.
    let big = vec![0u8; MAX_REQUEST + 1];
    let mut reply = vec![0u8; MAX_REPLY * 2];
    let calls = p.fuel_stats().calls;
    assert_eq!(
        p.request(key("app/echo"), &big, &RequestMeta::NONE, &mut reply),
        Ok(Err(HandlerError::InvalidRequest))
    );
    assert_eq!(p.fuel_stats().calls, calls);
    // The largest request echoes into a MAX_REPLY buffer only if it fits.
    let body = vec![7u8; MAX_REPLY - 4];
    assert_eq!(
        p.request(key("app/echo"), &body, &RequestMeta::NONE, &mut reply),
        Ok(Ok(MAX_REPLY))
    );
    assert!(!p.is_poisoned());
}

#[test]
fn s3_out_of_range_replies_and_unknown_codes_are_internal() {
    for (ret, want) in [
        (
            "(i32.add (local.get $cap) (i32.const 1))",
            HandlerError::Internal,
        ),
        ("(i32.const -8)", HandlerError::Internal),
        ("(i32.const 0x7fffffff)", HandlerError::Internal),
        ("(i32.const -2)", HandlerError::InvalidRequest),
        ("(i32.const -7)", HandlerError::DeadlineExceeded),
    ] {
        let mut p = load(&module(SERVE_ECHO, ret, 1024), Board::new());
        let mut reply = [0xEEu8; 8];
        let r = p.request(key("app/echo"), b"x", &RequestMeta::NONE, &mut reply);
        assert_eq!(r, Ok(Err(want)), "{ret}");
        assert_eq!(reply, [0xEE; 8], "{ret}: nothing copied");
        assert!(!p.is_poisoned());
    }
}

#[test]
fn s4_a_declined_or_out_of_memory_buffer_is_unavailable() {
    for alloc in [0, 65_536 - 8, -4] {
        let mut p = load(&module(SERVE_ECHO, ECHO, alloc), Board::new());
        let mut reply = [0u8; 8];
        let r = p.request(key("app/echo"), b"x", &RequestMeta::NONE, &mut reply);
        assert_eq!(r, Ok(Err(HandlerError::Unavailable)), "alloc {alloc}");
        assert!(!p.is_poisoned());
    }
}

#[test]
fn s5_running_out_of_fuel_in_on_request_traps_and_poisons() {
    let wasm = module(SERVE_ECHO, "(loop $l (br $l)) (i32.const 0)", 1024);
    let mut p =
        Payload::load(&wasm, Board::new(), Limits::default().with_fuel(10_000)).expect("loads");
    let mut reply = [0u8; 8];
    assert_eq!(
        p.request(key("app/echo"), b"x", &RequestMeta::NONE, &mut reply),
        Err(Trap::OutOfFuel)
    );
    assert!(p.is_poisoned());
    assert_eq!(
        p.request(key("app/echo"), b"x", &RequestMeta::NONE, &mut reply),
        Err(Trap::Poisoned)
    );
}

#[test]
fn s6_serve_checks_keys_bounds_slots_and_the_board() {
    let mut init = String::new();
    // sys/ key, out of bounds, then MAX_SERVES + 1 distinct keys built at
    // 200 as "k/<digit>".
    init.push_str("(call $report (call $serve (i32.const 48) (i32.const 5)))");
    init.push_str("(call $report (call $serve (i32.const 65530) (i32.const 8)))");
    init.push_str(
        "(i32.store8 (i32.const 200) (i32.const 107)) (i32.store8 (i32.const 201) (i32.const 47))",
    );
    for d in 0..=MAX_SERVES {
        init.push_str(&format!(
            "(i32.store8 (i32.const 202) (i32.const {})) (call $report (call $serve (i32.const 200) (i32.const 3)))",
            48 + d
        ));
    }
    let mut p = load(&module(&init, ECHO, 1024), Board::new());
    let mut want = vec![AbiError::InvalidKey.code(), AbiError::OutOfBounds.code()];
    want.extend((0..MAX_SERVES as i32).collect::<Vec<_>>());
    want.push(AbiError::NoResources.code());
    assert_eq!(p.device().results(), want);

    // A board that cannot route requests refuses, and so does the trait's
    // default.
    let board = Board {
        no_routing: true,
        ..Board::new()
    };
    let mut p = load(&module(SERVE_ECHO, ECHO, 1024), board);
    assert_eq!(p.device().results(), vec![AbiError::Unsupported.code()]);
    assert_eq!(p.serves().count(), 0);
}

#[test]
fn s7_serve_without_on_request_is_unsupported() {
    let wasm = wat::parse_str(
        r#"(module
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "serve" (func $serve (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 32) "app/echo")
  (data (i32.const 64) "r")
  (func (export "init")
    (i32.store (i32.const 80) (call $serve (i32.const 32) (i32.const 8)))
    (drop (call $publish (i32.const 64) (i32.const 1) (i32.const 80) (i32.const 4))))
  (func (export "mw_alloc") (param i32) (result i32) (i32.const 1024)))"#,
    )
    .expect("WAT");
    let mut p = load(&wasm, Board::new());
    assert_eq!(p.device().results(), vec![AbiError::Unsupported.code()]);
    assert!(!p.handles_requests());
}

#[test]
fn s8_node_id_writes_eight_bytes_or_fails_cleanly() {
    let init = r#"
      (call $report (call $node_id (i32.const 300)))
      (call $report (i32.load (i32.const 300)))
      (call $report (call $node_id (i32.const 65530)))"#;
    let mut p = load(&module(init, ECHO, 1024), Board::new());
    let first = i32::from_le_bytes([NODE.0[0], NODE.0[1], NODE.0[2], NODE.0[3]]);
    assert_eq!(
        p.device().results(),
        vec![0, first, AbiError::OutOfBounds.code()]
    );
    let board = Board {
        node: None,
        ..Board::new()
    };
    let mut p = load(&module(init, ECHO, 1024), board);
    assert_eq!(
        p.device().results(),
        vec![
            AbiError::Unsupported.code(),
            0,
            AbiError::OutOfBounds.code()
        ]
    );
}

#[test]
fn s9_a_board_without_abi5_methods_answers_unsupported() {
    let wasm = module(
        "(drop (call $serve (i32.const 32) (i32.const 8))) (drop (call $node_id (i32.const 300)))",
        ECHO,
        1024,
    );
    let mut p = Payload::load(&wasm, OldBoard, Limits::default()).expect("loads");
    let mut reply = [0u8; 8];
    assert_eq!(
        p.request(key("app/echo"), b"x", &RequestMeta::NONE, &mut reply),
        Ok(Err(HandlerError::NotFound))
    );
}

#[test]
fn s10_abi5_imports_are_unknown_to_an_abi4_host() {
    // What an ABI 4 host checks each import against: the ABI 4 prefix.
    let wasm = module(SERVE_ECHO, ECHO, 1024);
    let names = ["publish", "serve", "node_id"];
    let unknown: Vec<_> = names.iter().filter(|n| import_in(4, n).is_none()).collect();
    assert_eq!(unknown, [&"serve", &"node_id"]);
    assert_eq!(IMPORTS.len(), ABI4_IMPORTS + 2);
    // This host knows them; a module importing an import it does not know
    // fails to load with UnknownImport, exactly what an ABI 4 host reports
    // for `serve`.
    assert!(Payload::load(&wasm, Board::new(), Limits::default()).is_ok());
    let newer = wat::parse_str(
        r#"(module (import "mw" "serve_v6" (func (param i32 i32) (result i32))) (memory (export "memory") 1))"#,
    )
    .expect("WAT");
    assert!(matches!(
        Payload::load(&newer, Board::new(), Limits::default()),
        Err(LoadError::UnknownImport { .. })
    ));
    // on_request with the wrong shape is a bad export.
    let bad = wat::parse_str(
        r#"(module (memory (export "memory") 1) (func (export "on_request") (param i32) (result i32) (i32.const 0)))"#,
    )
    .expect("WAT");
    assert!(matches!(
        Payload::load(&bad, Board::new(), Limits::default()),
        Err(LoadError::BadExport { .. })
    ));
}
