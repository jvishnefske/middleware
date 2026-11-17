//! The recording board and event scripts shared by the differential and
//! golden-vector tests.
#![allow(dead_code)]

use std::cell::Cell;
use std::fmt::Write as _;
use std::rc::Rc;

use middleware::wasm_abi::bridge::{Bridge, BridgeTable};
use middleware::wasm_abi::route::RouteStat;
use middleware::{Key, SenderId};
use wasm_hdl_ref::{Core, Host};
use wasm_host::{AbiError, Device, Pull};

/// 128 KiB: the templates and SDK builds declare one 64 KiB page, and
/// `memory.grow` may add a second as wasm-host's default limits allow.
pub type Model = Core<{ 128 * 1024 }>;

pub const NODE: SenderId = SenderId([0x02, 0x5A, 0x75, 0xB0, 0, 1, 0, 0]);

/// A board that logs every call as a line.
pub struct Rec {
    pub trace: Vec<String>,
    pub clock: Rc<Cell<u32>>,
    pub bridges: BridgeTable,
    pub led: bool,
}

impl Rec {
    pub fn new(clock: Rc<Cell<u32>>) -> Self {
        Rec {
            trace: Vec::new(),
            clock,
            bridges: BridgeTable::new(),
            led: false,
        }
    }
    fn line(&mut self, s: String) {
        self.trace.push(s);
    }
    fn do_publish(&mut self, key: Key<'_>, data: &[u8]) -> Result<(), AbiError> {
        let mut s = format!("publish {key} ");
        for b in data {
            let _ = write!(s, "{b:02x}");
        }
        self.line(s);
        Ok(())
    }
    fn do_bridge(&mut self, b: &Bridge<'_>) -> Result<u8, AbiError> {
        let key = match b.filter {
            middleware::Filter::Exact(k) => k.as_str().to_string(),
            middleware::Filter::Prefix(k) => format!("{k}/**"),
        };
        self.line(format!("bridge {} {key} {}", b.src, b.dst));
        if b.src > 1 || b.dst > 1 {
            return Err(AbiError::Unsupported);
        }
        self.bridges.add(b)
    }
    fn do_stat(&mut self, h: u8, s: RouteStat) -> Result<u32, AbiError> {
        self.line(format!("route_stat {h} {s:?}"));
        let r = self.bridges.get(h).ok_or(AbiError::Busy)?;
        Ok(match s {
            RouteStat::Frames | RouteStat::Chunks => r.stats.forwarded,
            _ => r.stats.dropped,
        })
    }
}

macro_rules! board {
    ($t:ty) => {
        impl $t for Rec {
            fn log(&mut self, line: &str) {
                self.line(format!("log {line}"));
            }
            fn publish(&mut self, key: Key<'_>, data: &[u8]) -> Result<(), AbiError> {
                self.do_publish(key, data)
            }
            fn now_ms(&mut self) -> u32 {
                self.clock.get()
            }
            fn gpio_output(&mut self, pin: u8) -> Result<(), AbiError> {
                self.line(format!("gpio_output {pin}"));
                if pin < 2 {
                    Ok(())
                } else {
                    Err(AbiError::Unsupported)
                }
            }
            fn gpio_input(&mut self, pin: u8, pull: Pull) -> Result<(), AbiError> {
                self.line(format!("gpio_input {pin} {pull:?}"));
                if pin < 2 {
                    Ok(())
                } else {
                    Err(AbiError::Unsupported)
                }
            }
            fn gpio_write(&mut self, pin: u8, high: bool) -> Result<(), AbiError> {
                self.line(format!("gpio_write {pin} {high}"));
                self.led = high;
                if pin < 2 {
                    Ok(())
                } else {
                    Err(AbiError::Unsupported)
                }
            }
            fn gpio_read(&mut self, pin: u8) -> Result<bool, AbiError> {
                self.line(format!("gpio_read {pin}"));
                Ok(self.led)
            }
            fn serve(&mut self, key: Key<'_>) -> Result<(), AbiError> {
                self.line(format!("serve {key}"));
                Ok(())
            }
            fn node_id(&mut self) -> Result<SenderId, AbiError> {
                self.line("node_id".into());
                Ok(NODE)
            }
            fn route_bridge(&mut self, b: &Bridge<'_>) -> Result<u8, AbiError> {
                self.do_bridge(b)
            }
            fn route_stop(&mut self, h: u8) -> Result<(), AbiError> {
                self.line(format!("route_stop {h}"));
                self.bridges.remove(h)
            }
            fn route_stat(&mut self, h: u8, s: RouteStat) -> Result<u32, AbiError> {
                self.do_stat(h, s)
            }
        }
    };
}

board!(Device);
board!(Host);

/// One step of a script.
#[derive(Clone)]
pub enum Step {
    /// Advance the clock and tick.
    Tick(u32),
    /// Deliver a message.
    Msg(String, Vec<u8>),
    /// A request to a served key.
    Req(String, Vec<u8>),
}

/// The default script: ticks, a message on each subscription, a request
/// on each served key, more ticks.
pub fn script(subs: &[String], serves: &[String]) -> Vec<Step> {
    let mut s = Vec::new();
    for t in [5, 10, 100, 500, 1000] {
        s.push(Step::Tick(t));
    }
    for k in subs {
        for body in [&b""[..], b"0", b"1", b"hello", &[0xFF, 0, 7][..]] {
            s.push(Step::Msg(k.clone(), body.to_vec()));
        }
    }
    for k in serves {
        for body in [&b""[..], b"\x01", b"start", b"\x00\x01\x02\x03"] {
            s.push(Step::Req(k.clone(), body.to_vec()));
        }
    }
    for t in [1500, 2000, 2600, 3000, 5000, 10_000] {
        s.push(Step::Tick(t));
    }
    for k in subs {
        s.push(Step::Msg(k.clone(), b"late".to_vec()));
    }
    s
}
