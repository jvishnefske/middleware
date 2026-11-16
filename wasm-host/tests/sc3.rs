//! SC3 end to end: a sampled route owns the I2C device it streams from.
//! The payload starts an IMU route; every message on `go` makes it read
//! the IMU's WHO_AM_I (bus 0, 0x1F) and publish the `i2c_transfer` result
//! on `r`. The transfer is `Busy` while the route runs and reaches the
//! device again once the route is stopped, by the payload (`stop`) or by
//! the device (a `RouteStopped` event, even without `on_irq`).

use middleware::Key;
use wasm_host::{AbiError, Device, Event, EventKind, I2cDevice, Limits, Payload, Pull, SampleSource};

const IMU: I2cDevice = I2cDevice { bus: 0, addr: 0x1f };

#[derive(Default)]
struct Board {
    routes: u8,
    i2c: usize,
    published: Vec<(String, Vec<u8>)>,
}

impl Board {
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
        self.published.push((key.as_str().to_string(), data.to_vec()));
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
    fn i2c_transfer(&mut self, _: u8, _: u8, _: &[u8], rx: &mut [u8]) -> Result<(), AbiError> {
        self.i2c += 1;
        rx.fill(0xc7);
        Ok(())
    }
    fn route_samples(&mut self, _: SampleSource, _: Key<'_>) -> Result<u8, AbiError> {
        let h = self.routes;
        self.routes += 1;
        Ok(h)
    }
    fn sample_device(&self, source: &SampleSource) -> Option<I2cDevice> {
        matches!(source, SampleSource::Imu(_)).then_some(IMU)
    }
    fn route_stop(&mut self, _: u8) -> Result<(), AbiError> {
        Ok(())
    }
}

/// `init` subscribes to `go` and `stop` and starts the IMU route (kind 2,
/// 100 Hz, ±2 g, 3 channels, 10 samples); `go` reads WHO_AM_I, `stop`
/// stops route 0 and starts it again on `again`.
const PAYLOAD: &str = r#"
(module
  (import "mw" "subscribe" (func $subscribe (param i32 i32) (result i32)))
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "i2c_transfer" (func $i2c (param i32 i32 i32 i32 i32 i32) (result i32)))
  (import "mw" "route_source" (func $route_source (param i32 i32 i32 i32 i32) (result i32)))
  (import "mw" "route_stop" (func $route_stop (param i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "go")
  (data (i32.const 8) "stop")
  (data (i32.const 16) "r")
  (data (i32.const 24) "app/imu")
  (data (i32.const 32) "\64\00\02\03\0a\00\00\00")
  (data (i32.const 40) "\0d")
  (func $start
    (i32.store (i32.const 64)
      (call $route_source (i32.const 2) (i32.const 24) (i32.const 7) (i32.const 32) (i32.const 8)))
    (drop (call $publish (i32.const 16) (i32.const 1) (i32.const 64) (i32.const 4))))
  (func (export "init")
    (drop (call $subscribe (i32.const 0) (i32.const 2)))
    (drop (call $subscribe (i32.const 8) (i32.const 4)))
    (call $start))
  (func (export "mw_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "on_message") (param $slot i32) (param $p i32) (param $n i32)
    (if (i32.eqz (local.get $slot))
      (then
        (i32.store (i32.const 64)
          (call $i2c (i32.const 0) (i32.const 0x1f) (i32.const 40) (i32.const 1) (i32.const 48) (i32.const 1)))
        (drop (call $publish (i32.const 16) (i32.const 1) (i32.const 64) (i32.const 4))))
      (else
        (i32.store (i32.const 64) (call $route_stop (i32.const 0)))
        (drop (call $publish (i32.const 16) (i32.const 1) (i32.const 64) (i32.const 4)))))))
"#;

fn key(s: &str) -> Key<'_> {
    Key::new(s).expect("key")
}

#[test]
fn sc3_i2c_to_a_routed_device_is_busy_until_the_route_stops() {
    let wasm = wat::parse_str(PAYLOAD).expect("WAT");
    let mut p = Payload::load(&wasm, Board::default(), Limits::default()).expect("loads");
    let busy = AbiError::Busy.code();
    assert!(p.deliver(key("go"), b"").expect("runs"));
    // The payload stops the route: WHO_AM_I reaches the IMU again.
    assert!(p.deliver(key("stop"), b"").expect("runs"));
    assert!(p.deliver(key("go"), b"").expect("runs"));
    assert_eq!(p.device().results(), [0, busy, 0, 0]);
    assert_eq!(p.device().i2c, 1, "the Busy transfer never reached the bus");
}

#[test]
fn sc3_a_route_the_device_stopped_gives_its_device_back() {
    let wasm = wat::parse_str(PAYLOAD).expect("WAT");
    let mut p = Payload::load(&wasm, Board::default(), Limits::default()).expect("loads");
    assert!(p.deliver(key("go"), b"").expect("runs"));
    // No on_irq export: the event is dropped, but the device is released.
    let stopped = Event {
        kind: EventKind::RouteStopped,
        source: 0,
        value: AbiError::Transport.code(),
        time_us: 0,
    };
    assert_eq!(p.dispatch(stopped), Ok(false));
    assert!(p.deliver(key("go"), b"").expect("runs"));
    assert_eq!(p.device().results(), [0, AbiError::Busy.code(), 0]);
}
