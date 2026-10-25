//! End-to-end tests of the ABI 2 imports and `on_irq` dispatch
//! (requirement D4), with WAT payloads and a fake device that records every
//! peripheral call and returns scripted results.

use middleware::Key;
use middleware::wasm_abi::periph::MAX_DMA;
use wasm_host::{
    AbiError, Device, DmaChannel, Edge, Event, EventKind, Limits, LoadError, Payload, Pull,
    SpiConfig, TimerSpec, Trap, UartConfig,
};

/// Records peripheral calls; `fail` makes every ABI 2 peripheral call return
/// that error instead (e.g. `Busy`).
#[derive(Default)]
struct Fake {
    now_us: u32,
    overflow: u32,
    fail: Option<AbiError>,
    /// Bytes `dma_read` / `uart_read` hand back.
    rx: Vec<u8>,
    calls: Vec<String>,
    published: Vec<(String, Vec<u8>)>,
}

impl Fake {
    /// Payload results reported as 4-byte LE values on key `r`.
    fn results(&self) -> Vec<i32> {
        self.data("r")
            .iter()
            .map(|d| i32::from_le_bytes(d.as_slice().try_into().expect("4 bytes")))
            .collect()
    }

    fn data(&self, key: &str) -> Vec<Vec<u8>> {
        self.published
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, d)| d.clone())
            .collect()
    }

    fn outcome(&mut self, call: String) -> Result<(), AbiError> {
        self.calls.push(call);
        self.fail.map_or(Ok(()), Err)
    }

    fn read(&mut self, call: String, out: &mut [u8]) -> Result<usize, AbiError> {
        self.outcome(format!("{call} cap={}", out.len()))?;
        let n = self.rx.len().min(out.len());
        out[..n].copy_from_slice(&self.rx[..n]);
        Ok(n)
    }
}

impl Device for Fake {
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
    fn now_us(&mut self) -> u32 {
        self.now_us
    }
    fn irq_overflow(&mut self) -> u32 {
        core::mem::take(&mut self.overflow)
    }
    fn gpio_irq(&mut self, pin: u8, edge: Edge) -> Result<(), AbiError> {
        self.outcome(format!("gpio_irq {pin} {edge:?}"))
    }
    fn timer_start(&mut self, s: TimerSpec) -> Result<(), AbiError> {
        self.outcome(format!(
            "timer_start {} {} {}",
            s.id, s.period_us, s.periodic
        ))
    }
    fn timer_stop(&mut self, id: u8) -> Result<(), AbiError> {
        self.outcome(format!("timer_stop {id}"))
    }
    fn spi_open(&mut self, c: SpiConfig) -> Result<(), AbiError> {
        self.outcome(format!(
            "spi_open {} {} {:?} {:?} {:?} {} {}",
            c.bus, c.sck, c.mosi, c.miso, c.cs, c.freq_khz, c.mode
        ))
    }
    fn spi_transfer(&mut self, bus: u8, tx: &[u8], len: usize) -> Result<(), AbiError> {
        self.outcome(format!("spi_transfer {bus} {tx:?} {len}"))
    }
    fn dma_read(&mut self, channel: DmaChannel, out: &mut [u8]) -> Result<usize, AbiError> {
        self.read(format!("dma_read {channel:?}"), out)
    }
    fn uart_open(&mut self, c: UartConfig) -> Result<(), AbiError> {
        self.outcome(format!(
            "uart_open {} {:?} {:?} {}",
            c.port, c.tx, c.rx, c.baud
        ))
    }
    fn uart_write(&mut self, port: u8, data: &[u8]) -> Result<(), AbiError> {
        self.outcome(format!("uart_write {port} {data:?}"))
    }
    fn uart_read(&mut self, port: u8, out: &mut [u8]) -> Result<usize, AbiError> {
        self.read(format!("uart_read {port}"), out)
    }
}

/// Implements only the ABI 1 methods, so every ABI 2 method is a default.
struct Abi1Board;

impl Device for Abi1Board {
    fn log(&mut self, _: &str) {}
    fn publish(&mut self, _: Key<'_>, _: &[u8]) -> Result<(), AbiError> {
        Ok(())
    }
    fn now_ms(&mut self) -> u32 {
        7
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

/// Every ABI 2 import plus `$report` (publishes an i32 on key `r`, stored at
/// offset 0; value scratch at 4). Data: "ping" at 16.
const PRELUDE: &str = r#"
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "now_us" (func $now_us (result i32)))
  (import "mw" "irq_overflow" (func $irq_overflow (result i32)))
  (import "mw" "gpio_irq" (func $gpio_irq (param i32 i32) (result i32)))
  (import "mw" "timer_start" (func $timer_start (param i32 i32 i32) (result i32)))
  (import "mw" "timer_stop" (func $timer_stop (param i32) (result i32)))
  (import "mw" "spi_open" (func $spi_open (param i32 i32 i32 i32 i32 i32 i32) (result i32)))
  (import "mw" "spi_transfer" (func $spi_transfer (param i32 i32 i32 i32) (result i32)))
  (import "mw" "dma_read" (func $dma_read (param i32 i32 i32) (result i32)))
  (import "mw" "uart_open" (func $uart_open (param i32 i32 i32 i32) (result i32)))
  (import "mw" "uart_write" (func $uart_write (param i32 i32 i32) (result i32)))
  (import "mw" "uart_read" (func $uart_read (param i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "r")
  (data (i32.const 16) "ping")
  (func $report (param $v i32)
    (i32.store (i32.const 4) (local.get $v))
    (drop (call $publish (i32.const 0) (i32.const 1) (i32.const 4) (i32.const 4))))
"#;

fn module(body: &str) -> Vec<u8> {
    wat::parse_str(format!("(module {PRELUDE} {body})")).expect("valid WAT")
}

fn load_with<D: Device + 'static>(body: &str, device: D) -> Payload<D> {
    Payload::load(&module(body), device, Limits::default()).expect("payload loads")
}

/// Runs `calls` (a sequence of `(call $report ...)` expressions) in `init`.
fn run_init(calls: &str, device: Fake) -> Payload<Fake> {
    load_with(&format!(r#"(func (export "init") {calls})"#), device)
}

const BUSY: i32 = AbiError::Busy.code();
const BAD: i32 = AbiError::InvalidArgument.code();
const OOB: i32 = AbiError::OutOfBounds.code();
const UNSUPPORTED: i32 = AbiError::Unsupported.code();

// ------------------------------------------------------------ clock, overflow

#[test]
fn d4_now_us_and_irq_overflow() {
    let fake = Fake {
        now_us: 123_456,
        overflow: 3,
        ..Fake::default()
    };
    let mut p = run_init(
        r#"(call $report (call $now_us))
           (call $report (call $irq_overflow))
           (call $report (call $irq_overflow))"#,
        fake,
    );
    assert_eq!(p.device().results(), vec![123_456, 3, 0]);
}

// ------------------------------------------------------------------ gpio_irq

#[test]
fn d4_gpio_irq_happy_path_and_bad_args() {
    let mut p = run_init(
        r#"(call $report (call $gpio_irq (i32.const 4) (i32.const 1)))
           (call $report (call $gpio_irq (i32.const 4) (i32.const 0)))
           (call $report (call $gpio_irq (i32.const 4) (i32.const 4)))
           (call $report (call $gpio_irq (i32.const 256) (i32.const 1)))
           (call $report (call $gpio_irq (i32.const -1) (i32.const 3)))"#,
        Fake::default(),
    );
    assert_eq!(p.device().results(), vec![0, 0, BAD, BAD, BAD]);
    assert_eq!(
        p.device().calls,
        vec!["gpio_irq 4 Rising", "gpio_irq 4 Off"]
    );
}

// -------------------------------------------------------------------- timers

#[test]
fn d4_timer_start_stop_and_bad_ids() {
    let mut p = run_init(
        r#"(call $report (call $timer_start (i32.const 3) (i32.const 1000) (i32.const 1)))
           (call $report (call $timer_start (i32.const 0) (i32.const 100) (i32.const 0)))
           (call $report (call $timer_start (i32.const 4) (i32.const 1000) (i32.const 1)))
           (call $report (call $timer_start (i32.const 0) (i32.const 99) (i32.const 1)))
           (call $report (call $timer_start (i32.const 0) (i32.const -1) (i32.const 1)))
           (call $report (call $timer_start (i32.const 0) (i32.const 1000) (i32.const 2)))
           (call $report (call $timer_stop (i32.const 3)))
           (call $report (call $timer_stop (i32.const 4)))
           (call $report (call $timer_stop (i32.const -1)))"#,
        Fake::default(),
    );
    assert_eq!(
        p.device().results(),
        vec![0, 0, BAD, BAD, BAD, BAD, 0, BAD, BAD]
    );
    assert_eq!(
        p.device().calls,
        vec![
            "timer_start 3 1000 true",
            "timer_start 0 100 false",
            "timer_stop 3"
        ]
    );
}

// ----------------------------------------------------------------------- SPI

#[test]
fn d4_spi_open_happy_path_and_bad_config() {
    let mut p = run_init(
        r#"(call $report (call $spi_open (i32.const 0) (i32.const 6) (i32.const 7) (i32.const 2) (i32.const -1) (i32.const 1000) (i32.const 3)))
           (call $report (call $spi_open (i32.const 1) (i32.const 6) (i32.const 7) (i32.const 2) (i32.const -1) (i32.const 1000) (i32.const 0)))
           (call $report (call $spi_open (i32.const 0) (i32.const 6) (i32.const 6) (i32.const 2) (i32.const -1) (i32.const 1000) (i32.const 0)))
           (call $report (call $spi_open (i32.const 0) (i32.const 6) (i32.const 7) (i32.const 2) (i32.const -1) (i32.const 0) (i32.const 0)))
           (call $report (call $spi_open (i32.const 0) (i32.const 6) (i32.const 7) (i32.const 2) (i32.const -1) (i32.const 1000) (i32.const 4)))"#,
        Fake::default(),
    );
    assert_eq!(p.device().results(), vec![0, BAD, BAD, BAD, BAD]);
    assert_eq!(
        p.device().calls,
        vec!["spi_open 0 6 Some(7) Some(2) None 1000 3"]
    );
}

#[test]
fn d4_spi_transfer_copies_tx_and_checks_lengths_and_bounds() {
    let mut p = run_init(
        &format!(
            r#"(call $report (call $spi_transfer (i32.const 0) (i32.const 16) (i32.const 4) (i32.const 4)))
               (call $report (call $spi_transfer (i32.const 0) (i32.const 16) (i32.const 2) (i32.const 8)))
               (call $report (call $spi_transfer (i32.const 0) (i32.const 16) (i32.const 0) (i32.const 3)))
               (call $report (call $spi_transfer (i32.const 0) (i32.const 65534) (i32.const 4) (i32.const 4)))
               (call $report (call $spi_transfer (i32.const 0) (i32.const -4) (i32.const 4) (i32.const 0)))
               (call $report (call $spi_transfer (i32.const 0) (i32.const 0) (i32.const {over}) (i32.const 0)))
               (call $report (call $spi_transfer (i32.const 0) (i32.const 0) (i32.const 0) (i32.const {over})))
               (call $report (call $spi_transfer (i32.const 0) (i32.const 0) (i32.const 0) (i32.const 0)))
               (call $report (call $spi_transfer (i32.const 0) (i32.const 0) (i32.const -1) (i32.const 4)))
               (call $report (call $spi_transfer (i32.const 1) (i32.const 16) (i32.const 4) (i32.const 4)))
               (call $report (call $spi_transfer (i32.const 0) (i32.const 0) (i32.const {max}) (i32.const 0)))"#,
            over = MAX_DMA + 1,
            max = MAX_DMA
        ),
        Fake::default(),
    );
    assert_eq!(
        p.device().results(),
        vec![0, 0, 0, OOB, OOB, BAD, BAD, BAD, BAD, BAD, 0]
    );
    let calls = &p.device().calls;
    assert_eq!(calls[0], "spi_transfer 0 [112, 105, 110, 103] 4");
    assert_eq!(calls[1], "spi_transfer 0 [112, 105] 8", "rx_len > tx_len");
    assert_eq!(calls[2], "spi_transfer 0 [] 3", "receive only");
    assert!(
        calls[3].ends_with(&format!("] {MAX_DMA}")),
        "full MAX_DMA tx"
    );
    assert_eq!(calls.len(), 4);
}

#[test]
fn d4_dma_read_copies_into_guest_memory() {
    // on_irq: on DmaDone, read into offset 256 and publish what arrived on
    // key "d"; report the count.
    let body = r#"
      (data (i32.const 8) "d")
      (func (export "on_irq") (param $kind i32) (param $src i32) (param $val i32) (param $t i32)
        (local $n i32)
        (local.set $n (call $dma_read (local.get $src) (i32.const 256) (i32.const 64)))
        (call $report (local.get $n))
        (if (i32.gt_s (local.get $n) (i32.const 0))
          (then (drop (call $publish (i32.const 8) (i32.const 1) (i32.const 256) (local.get $n))))))"#;
    let fake = Fake {
        rx: b"pong!".to_vec(),
        ..Fake::default()
    };
    let mut p = load_with(body, fake);
    assert!(p.handles_irq());
    let done = Event {
        kind: EventKind::DmaDone,
        source: DmaChannel::Spi0 as i32,
        value: 5,
        time_us: 10,
    };
    assert_eq!(p.dispatch(done), Ok(true));
    assert_eq!(p.device().results(), vec![5]);
    assert_eq!(p.device().data("d"), vec![b"pong!".to_vec()]);
    assert_eq!(p.device().calls, vec!["dma_read Spi0 cap=64"]);
}

#[test]
fn d4_dma_read_bounds_cap_and_bad_channel() {
    let fake = Fake {
        rx: vec![0xAB; 8000],
        ..Fake::default()
    };
    let mut p = run_init(
        r#"(call $report (call $dma_read (i32.const 0) (i32.const 1024) (i32.const 8192)))
           (call $report (call $dma_read (i32.const 1) (i32.const 1024) (i32.const 0)))
           (call $report (call $dma_read (i32.const 2) (i32.const 1024) (i32.const 4)))
           (call $report (call $dma_read (i32.const 0) (i32.const 65530) (i32.const 8)))
           (call $report (call $dma_read (i32.const 0) (i32.const 0) (i32.const -1)))
           (call $report (call $dma_read (i32.const 0) (i32.const 0) (i32.const 70000)))
           (call $report (i32.load8_u (i32.const 5115)))
           (call $report (i32.load8_u (i32.const 5116)))"#,
        fake,
    );
    // The guest buffer is 8192 bytes but the host copies at most MAX_DMA.
    assert_eq!(
        p.device().results(),
        vec![MAX_DMA as i32, 0, BAD, OOB, OOB, OOB, 0xAB, 0]
    );
    assert_eq!(
        p.device().calls,
        vec![
            format!("dma_read Spi0 cap={MAX_DMA}"),
            "dma_read Uart1Tx cap=0".to_string()
        ]
    );
}

// ---------------------------------------------------------------------- UART

#[test]
fn d4_uart_open_write_read() {
    let fake = Fake {
        rx: b"hey".to_vec(),
        ..Fake::default()
    };
    let mut p = run_init(
        &format!(
            r#"(call $report (call $uart_open (i32.const 1) (i32.const 4) (i32.const 5) (i32.const 115200)))
               (call $report (call $uart_open (i32.const 0) (i32.const 4) (i32.const 5) (i32.const 115200)))
               (call $report (call $uart_open (i32.const 1) (i32.const 4) (i32.const 4) (i32.const 115200)))
               (call $report (call $uart_open (i32.const 1) (i32.const 4) (i32.const 5) (i32.const 299)))
               (call $report (call $uart_write (i32.const 1) (i32.const 16) (i32.const 4)))
               (call $report (call $uart_write (i32.const 0) (i32.const 16) (i32.const 4)))
               (call $report (call $uart_write (i32.const 1) (i32.const 16) (i32.const 0)))
               (call $report (call $uart_write (i32.const 1) (i32.const 0) (i32.const {over})))
               (call $report (call $uart_write (i32.const 1) (i32.const 65534) (i32.const 4)))
               (call $report (call $uart_read (i32.const 1) (i32.const 512) (i32.const 16)))
               (call $report (i32.load8_u (i32.const 514)))
               (call $report (call $uart_read (i32.const 2) (i32.const 512) (i32.const 16)))
               (call $report (call $uart_read (i32.const 1) (i32.const 65530) (i32.const 16)))"#,
            over = MAX_DMA + 1
        ),
        fake,
    );
    assert_eq!(
        p.device().results(),
        vec![
            0,
            BAD,
            BAD,
            BAD,
            0,
            BAD,
            BAD,
            BAD,
            OOB,
            3,
            i32::from(b'y'),
            BAD,
            OOB
        ]
    );
    assert_eq!(
        p.device().calls,
        vec![
            "uart_open 1 Some(4) Some(5) 115200",
            "uart_write 1 [112, 105, 110, 103]",
            "uart_read 1 cap=16"
        ]
    );
}

// ------------------------------------------------------- device error codes

#[test]
fn d4_busy_from_device_is_passed_through() {
    let fake = Fake {
        fail: Some(AbiError::Busy),
        ..Fake::default()
    };
    let mut p = run_init(
        r#"(call $report (call $gpio_irq (i32.const 4) (i32.const 1)))
           (call $report (call $timer_start (i32.const 0) (i32.const 1000) (i32.const 1)))
           (call $report (call $timer_stop (i32.const 0)))
           (call $report (call $spi_open (i32.const 0) (i32.const 6) (i32.const 7) (i32.const -1) (i32.const -1) (i32.const 1000) (i32.const 0)))
           (call $report (call $spi_transfer (i32.const 0) (i32.const 16) (i32.const 4) (i32.const 4)))
           (call $report (call $dma_read (i32.const 0) (i32.const 64) (i32.const 4)))
           (call $report (call $uart_open (i32.const 1) (i32.const 4) (i32.const 5) (i32.const 9600)))
           (call $report (call $uart_write (i32.const 1) (i32.const 16) (i32.const 4)))
           (call $report (call $uart_read (i32.const 1) (i32.const 64) (i32.const 4)))"#,
        fake,
    );
    assert_eq!(p.device().results(), vec![BUSY; 9]);
    assert!(!p.is_poisoned());
}

#[test]
fn d4_default_device_reports_unsupported() {
    // Every ABI 2 import with valid arguments; results stored at 64.. as i32s.
    let body = r#"
      (func $at (param $i i32) (param $v i32)
        (i32.store (i32.add (i32.const 64) (i32.shl (local.get $i) (i32.const 2))) (local.get $v)))
      (func (export "init")
        (call $at (i32.const 0) (call $now_us))
        (call $at (i32.const 1) (call $irq_overflow))
        (call $at (i32.const 2) (call $gpio_irq (i32.const 4) (i32.const 1)))
        (call $at (i32.const 3) (call $timer_start (i32.const 0) (i32.const 1000) (i32.const 1)))
        (call $at (i32.const 4) (call $timer_stop (i32.const 0)))
        (call $at (i32.const 5) (call $spi_open (i32.const 0) (i32.const 6) (i32.const 7) (i32.const -1) (i32.const -1) (i32.const 1000) (i32.const 0)))
        (call $at (i32.const 6) (call $spi_transfer (i32.const 0) (i32.const 16) (i32.const 4) (i32.const 4)))
        (call $at (i32.const 7) (call $dma_read (i32.const 0) (i32.const 512) (i32.const 4)))
        (call $at (i32.const 8) (call $uart_open (i32.const 1) (i32.const 4) (i32.const 5) (i32.const 9600)))
        (call $at (i32.const 9) (call $uart_write (i32.const 1) (i32.const 16) (i32.const 4)))
        (call $at (i32.const 10) (call $uart_read (i32.const 1) (i32.const 512) (i32.const 4))))
      (func (export "on_irq") (param i32 i32 i32 i32)
        (local $i i32)
        (loop $l
          (call $report (i32.load (i32.add (i32.const 64) (i32.shl (local.get $i) (i32.const 2)))))
          (local.set $i (i32.add (local.get $i) (i32.const 1)))
          (br_if $l (i32.lt_u (local.get $i) (i32.const 11)))))"#;
    // A board that records publishes but, like Abi1Board, leaves every
    // ABI 2 method at its default. `init` stores the results; dispatching
    // any event makes `on_irq` publish them.
    struct Recorder(Abi1Board, Vec<i32>);
    impl Device for Recorder {
        fn log(&mut self, _: &str) {}
        fn publish(&mut self, _: Key<'_>, d: &[u8]) -> Result<(), AbiError> {
            self.1
                .push(i32::from_le_bytes(d.try_into().expect("4 bytes")));
            Ok(())
        }
        fn now_ms(&mut self) -> u32 {
            self.0.now_ms()
        }
        fn gpio_output(&mut self, p: u8) -> Result<(), AbiError> {
            self.0.gpio_output(p)
        }
        fn gpio_input(&mut self, p: u8, pull: Pull) -> Result<(), AbiError> {
            self.0.gpio_input(p, pull)
        }
        fn gpio_write(&mut self, p: u8, h: bool) -> Result<(), AbiError> {
            self.0.gpio_write(p, h)
        }
        fn gpio_read(&mut self, p: u8) -> Result<bool, AbiError> {
            self.0.gpio_read(p)
        }
    }
    let mut p = load_with(body, Recorder(Abi1Board, Vec::new()));
    let ev = Event {
        kind: EventKind::Gpio,
        source: 0,
        value: 0,
        time_us: 0,
    };
    assert_eq!(p.dispatch(ev), Ok(true));
    let mut expected = vec![7000, 0];
    expected.extend([UNSUPPORTED; 9]);
    assert_eq!(p.device().1, expected);

    // The plain ABI 1 board loads the same ABI 2 module too.
    assert!(Payload::load(&module(body), Abi1Board, Limits::default()).is_ok());
}

// ------------------------------------------------------------------ dispatch

const ON_IRQ_REPORTS_ARGS: &str = r#"
  (func (export "on_irq") (param $k i32) (param $s i32) (param $v i32) (param $t i32)
    (call $report (local.get $k))
    (call $report (local.get $s))
    (call $report (local.get $v))
    (call $report (local.get $t)))"#;

#[test]
fn d4_dispatch_calls_on_irq_with_event_args() {
    let mut p = load_with(ON_IRQ_REPORTS_ARGS, Fake::default());
    assert!(p.handles_irq());
    let timer = Event {
        kind: EventKind::Timer,
        source: 2,
        value: 3,
        time_us: 0xFFFF_FFF0,
    };
    let rx = Event {
        kind: EventKind::UartRx,
        source: 1,
        value: 17,
        time_us: 42,
    };
    assert_eq!(p.dispatch(timer), Ok(true));
    assert_eq!(p.dispatch(rx), Ok(true));
    assert_eq!(p.device().results(), vec![2, 2, 3, -16, 4, 1, 17, 42]);
}

#[test]
fn d4_dispatch_without_on_irq_returns_false() {
    let mut p = load_with("", Fake::default());
    assert!(!p.handles_irq());
    let ev = Event {
        kind: EventKind::Gpio,
        source: 4,
        value: 1,
        time_us: 0,
    };
    assert_eq!(p.dispatch(ev), Ok(false));
    assert!(!p.is_poisoned());
}

#[test]
fn d4_infinite_loop_in_on_irq_runs_out_of_fuel_and_poisons() {
    let body = r#"
      (func (export "on_irq") (param i32 i32 i32 i32) (loop $l (br $l)))"#;
    let limits = Limits {
        fuel_per_call: 10_000,
        ..Limits::default()
    };
    let mut p = Payload::load(&module(body), Fake::default(), limits).expect("loads");
    let ev = Event {
        kind: EventKind::Gpio,
        source: 4,
        value: 1,
        time_us: 0,
    };
    assert_eq!(p.dispatch(ev), Err(Trap::OutOfFuel));
    assert!(p.is_poisoned());
    assert_eq!(p.dispatch(ev), Err(Trap::Poisoned));
    assert_eq!(p.next_tick(), None);
}

#[test]
fn d4_dispatch_refuels_every_call() {
    // Each call burns most of a small fuel budget; repeated dispatches must
    // all succeed because fuel is refilled per call.
    let body = r#"
      (func (export "on_irq") (param i32 i32 i32 i32)
        (local $i i32)
        (loop $l
          (local.set $i (i32.add (local.get $i) (i32.const 1)))
          (br_if $l (i32.lt_u (local.get $i) (i32.const 500)))))"#;
    let limits = Limits {
        fuel_per_call: 5_000,
        ..Limits::default()
    };
    let mut p = Payload::load(&module(body), Fake::default(), limits).expect("loads");
    let ev = Event {
        kind: EventKind::Timer,
        source: 0,
        value: 1,
        time_us: 0,
    };
    for _ in 0..10 {
        assert_eq!(p.dispatch(ev), Ok(true));
    }
}

#[test]
fn d4_on_irq_with_wrong_signature_is_rejected() {
    let wasm = wat::parse_str(
        r#"(module (memory (export "memory") 1)
             (func (export "on_irq") (param i32 i32 i32)))"#,
    )
    .unwrap();
    let err = Payload::load(&wasm, Fake::default(), Limits::default()).err();
    assert_eq!(
        err,
        Some(LoadError::BadExport {
            name: "on_irq".into()
        })
    );
    let wasm = wat::parse_str(
        r#"(module (memory (export "memory") 1)
             (func (export "on_irq") (param i32 i32 i32 i32) (result i32) (i32.const 0)))"#,
    )
    .unwrap();
    assert!(matches!(
        Payload::load(&wasm, Fake::default(), Limits::default()).err(),
        Some(LoadError::BadExport { .. })
    ));
}

#[test]
fn d4_abi1_module_still_loads_and_runs() {
    let abi1 = wat::parse_str(
        r#"(module
             (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
             (import "mw" "set_tick" (func $set_tick (param i32) (result i32)))
             (import "mw" "now_ms" (func $now_ms (result i32)))
             (memory (export "memory") 1)
             (data (i32.const 0) "r")
             (func (export "init") (drop (call $set_tick (i32.const 10))))
             (func (export "tick") (param $t i32)
               (i32.store (i32.const 4) (local.get $t))
               (drop (call $publish (i32.const 0) (i32.const 1) (i32.const 4) (i32.const 4)))))"#,
    )
    .unwrap();
    let mut p = Payload::load(&abi1, Fake::default(), Limits::default()).expect("ABI 1 loads");
    assert!(!p.handles_irq());
    assert_eq!(p.next_tick(), Some(10));
    p.tick(10).expect("tick");
    assert_eq!(p.device().results(), vec![10]);
    // ABI 1 payloads also load on an ABI 1-only board.
    assert!(Payload::load(&abi1, Abi1Board, Limits::default()).is_ok());
}
