//! The `wasm-micro` backend (feature `micro` without `wasmi`): the image
//! is copied once and interpreted in place, with no validation pass.
//!
//! It runs where wasmi does not fit or does not build (no CAS atomics).
//! The differences a host sees:
//!
//! * a module wasmi's validator would refuse is not refused here: it
//!   traps, or computes garbage in its own memory, when the offending
//!   code runs (see the `wasm-micro` crate docs);
//! * fuel is one unit per instruction executed, where wasmi charges by
//!   its own translated instructions, so the same payload uses a different
//!   amount of [`crate::Limits::fuel_per_call`];
//! * floating-point arithmetic traps unless feature `micro-float` is on;
//! * trap and load messages differ in wording.

use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::vec::Vec;

use middleware::wasm_abi::AbiError;
use wasm_micro::{Error, ExportKind};

use crate::abi::{self, ImportKind, Shape};
use crate::link::{self, State};
use crate::{Device, Limits, LoadError, Trap, heap};

pub(crate) const NAME: &str = "micro";

/// What an export is, as [`crate::validate`] needs it.
pub(crate) enum Export {
    /// A function; `None` when it uses a non-`i32` type.
    Func(Option<Shape>),
    Memory,
    Other,
}

const I32: u8 = 0x7F;

fn shape((params, results): (&[u8], &[u8])) -> Option<Shape> {
    let all_i32 = |v: &[u8]| v.iter().all(|t| *t == I32);
    (all_i32(params) && all_i32(results)).then_some(Shape {
        params: params.len(),
        results: results.len(),
    })
}

fn invalid(e: Error) -> LoadError {
    match e {
        Error::OutOfMemory => LoadError::OutOfMemory,
        e => LoadError::Invalid(e.as_str().to_string()),
    }
}

fn name(bytes: &[u8]) -> Result<&str, LoadError> {
    core::str::from_utf8(bytes).map_err(|_| invalid(Error::Malformed))
}

/// A parsed module with its own copy of the image.
pub(crate) struct Module(wasm_micro::Module<Box<[u8]>>);

impl Module {
    pub fn new(wasm: &[u8], _limits: &Limits) -> Result<Self, LoadError> {
        let mut image = Vec::new();
        image
            .try_reserve_exact(wasm.len())
            .map_err(|_| LoadError::OutOfMemory)?;
        image.extend_from_slice(wasm);
        wasm_micro::Module::new(image.into_boxed_slice())
            .map(Module)
            .map_err(invalid)
    }

    /// Calls `f` with every import's module, name and kind. Imports other
    /// than functions never get this far: the parser refuses them.
    pub fn imports(
        &self,
        f: &mut dyn FnMut(&str, &str, ImportKind) -> Result<(), LoadError>,
    ) -> Result<(), LoadError> {
        let mut seen = 0;
        for i in self.0.imports() {
            let kind = ImportKind::Func(shape((i.params, i.results)));
            f(name(i.module)?, name(i.name)?, kind)?;
            seen += 1;
        }
        if seen == self.0.import_count() {
            Ok(())
        } else {
            Err(invalid(Error::Malformed))
        }
    }

    /// Calls `f` with every export's name and kind.
    pub fn exports(
        &self,
        f: &mut dyn FnMut(&str, Export) -> Result<(), LoadError>,
    ) -> Result<(), LoadError> {
        for e in self.0.exports() {
            let kind = match e.kind {
                ExportKind::Func => Export::Func(self.0.func_type(e.index).and_then(shape)),
                ExportKind::Memory => Export::Memory,
                _ => Export::Other,
            };
            f(name(e.name)?, kind)?;
        }
        Ok(())
    }
}

/// One import: linear memory, the state and the raw argument slots in;
/// the `i32` result (ignored for `log`) out.
type Handler<D> = fn(&mut [u8], &mut State<D>, &[u64]) -> i32;

/// Argument `i` as the `i32` it is.
fn a(vals: &[u64], i: usize) -> i32 {
    vals.get(i).copied().unwrap_or(0) as i32
}

fn code(r: Result<i32, AbiError>) -> i32 {
    abi::result_code(r)
}

/// The handler of import `name`: the same `*_call` functions the wasmi
/// linker registers, behind one signature.
fn handler<D: Device>(name: &str) -> Option<Handler<D>> {
    use link::*;
    Some(match name {
        "log" => |m, st, v| {
            // `log` has no result; a bad pointer is silently dropped.
            let _ = log_call(m, st, a(v, 0), a(v, 1));
            0
        },
        "publish" => |m, st, v| {
            code(ok0(publish_call(
                m,
                st,
                (a(v, 0), a(v, 1), a(v, 2), a(v, 3)),
            )))
        },
        "subscribe" => |m, st, v| code(subscribe_call(m, st, a(v, 0), a(v, 1))),
        "now_ms" => |_, st, _| st.device.now_ms() as i32,
        "set_tick" => |_, st, v| code(ok0(set_tick_call(st, a(v, 0)))),
        "gpio_output" => {
            |_, st, v| code(ok0(abi::pin(a(v, 0)).and_then(|p| st.device.gpio_output(p))))
        }
        "gpio_input" => |_, st, v| {
            let r = abi::pin(a(v, 0))
                .and_then(|p| Ok((p, abi::pull(a(v, 1))?)))
                .and_then(|(p, pull)| st.device.gpio_input(p, pull));
            code(ok0(r))
        },
        "gpio_write" => |_, st, v| {
            let r = abi::pin(a(v, 0))
                .and_then(|p| Ok((p, abi::level(a(v, 1))?)))
                .and_then(|(p, high)| st.device.gpio_write(p, high));
            code(ok0(r))
        },
        "gpio_read" => |_, st, v| {
            let r = abi::pin(a(v, 0)).and_then(|p| st.device.gpio_read(p));
            code(r.map(i32::from))
        },
        // ABI 2: interrupts, timers, SPI and UART DMA.
        "now_us" => |_, st, _| st.device.now_us() as i32,
        "irq_overflow" => |_, st, _| abi::count_code(st.device.irq_overflow()),
        "gpio_irq" => |_, st, v| code(ok0(gpio_irq_call(st, a(v, 0), a(v, 1)))),
        "timer_start" => |_, st, v| code(ok0(timer_start_call(st, (a(v, 0), a(v, 1), a(v, 2))))),
        "timer_stop" => |_, st, v| code(ok0(timer_stop_call(st, a(v, 0)))),
        "spi_open" => |_, st, v| {
            let args = (
                a(v, 0),
                a(v, 1),
                a(v, 2),
                a(v, 3),
                a(v, 4),
                a(v, 5),
                a(v, 6),
            );
            code(ok0(spi_open_call(st, args)))
        },
        "spi_transfer" => |m, st, v| {
            code(ok0(spi_transfer_call(
                m,
                st,
                (a(v, 0), a(v, 1), a(v, 2), a(v, 3)),
            )))
        },
        "dma_read" => |m, st, v| code(dma_read_call(m, st, (a(v, 0), a(v, 1), a(v, 2)))),
        "uart_open" => |_, st, v| {
            code(ok0(uart_open_call(
                st,
                (a(v, 0), a(v, 1), a(v, 2), a(v, 3)),
            )))
        },
        "uart_write" => |m, st, v| code(ok0(uart_write_call(m, st, (a(v, 0), a(v, 1), a(v, 2))))),
        "uart_read" => |m, st, v| code(uart_read_call(m, st, (a(v, 0), a(v, 1), a(v, 2)))),
        // ABI 3: PWM, ADC, I2C.
        "pwm_open" => |_, st, v| code(ok0(pwm_open_call(st, (a(v, 0), a(v, 1))))),
        "pwm_set" => |_, st, v| code(ok0(pwm_set_call(st, (a(v, 0), a(v, 1), a(v, 2))))),
        "pwm_stop" => |_, st, v| code(ok0(pwm_stop_call(st, a(v, 0)))),
        "adc_read" => |_, st, v| code(adc_read_call(st, a(v, 0))),
        "i2c_open" => |_, st, v| code(ok0(i2c_open_call(st, (a(v, 0), a(v, 1))))),
        "i2c_transfer" => |m, st, v| {
            let args = (a(v, 0), a(v, 1), a(v, 2), a(v, 3), a(v, 4), a(v, 5));
            code(ok0(i2c_transfer_call(m, st, args)))
        },
        // ABI 4: DMA routes.
        "route_source" => |m, st, v| {
            code(route_source_call(
                m,
                st,
                (a(v, 0), a(v, 1), a(v, 2), a(v, 3), a(v, 4)),
            ))
        },
        "route_sink" => |m, st, v| {
            code(route_sink_call(
                m,
                st,
                (a(v, 0), a(v, 1), a(v, 2), a(v, 3), a(v, 4)),
            ))
        },
        "route_stop" => |_, st, v| code(ok0(route_stop_call(st, a(v, 0)))),
        "route_stat" => |_, st, v| code(route_stat_call(st, (a(v, 0), a(v, 1)))),
        // ABI 5: serving requests.
        "serve" => |m, st, v| code(serve_call(m, st, a(v, 0), a(v, 1))),
        "node_id" => |m, st, v| code(ok0(node_id_call(m, st, a(v, 0)))),
        // ABI 6: bridging between links.
        "route_bridge" => |m, st, v| {
            code(route_bridge_call(
                m,
                st,
                (a(v, 0), a(v, 1), a(v, 2), a(v, 3)),
            ))
        },
        // ABI 7: LED strips.
        "ledstrip_open" => |_, st, v| code(ledstrip_open_call(st, (a(v, 0), a(v, 1)))),
        "ledstrip_write" => {
            |m, st, v| code(ok0(ledstrip_write_call(m, st, (a(v, 0), a(v, 1), a(v, 2)))))
        }
        "ledstrip_close" => |_, st, v| code(ok0(ledstrip_close_call(st, a(v, 0)))),
        // ABI 8: channels.
        "channel_open" => |m, st, v| code(channel_open_call(m, st, (a(v, 0), a(v, 1)))),
        "channel_publish" => |m, st, v| {
            code(ok0(channel_publish_call(
                m,
                st,
                (a(v, 0), a(v, 1), a(v, 2)),
            )))
        },
        // ABI 10: the node's manifest.
        "manifest" => |m, st, v| code(manifest_call(m, st, a(v, 0), a(v, 1))),
        _ => return None,
    })
}

/// The payload's state and its imports, by import index.
struct Host<D> {
    state: State<D>,
    handlers: Vec<Handler<D>>,
}

impl<D> wasm_micro::Host for Host<D> {
    fn call(&mut self, import: u32, mem: &mut [u8], vals: &mut [u64]) -> Result<(), Error> {
        let h = self.handlers.get(import as usize).ok_or(Error::Invalid)?;
        let r = h(mem, &mut self.state, vals);
        if let Some(v) = vals.first_mut() {
            *v = u64::from(r as u32);
        }
        Ok(())
    }
}

fn trap(e: Error) -> Trap {
    match e {
        Error::OutOfFuel => Trap::OutOfFuel,
        e => Trap::Wasm(e.as_str().to_string()),
    }
}

/// An instantiated module and the payload's [`State`].
pub(crate) struct Instance<D: Device + 'static> {
    inst: wasm_micro::Instance<Box<[u8]>>,
    host: Host<D>,
}

impl<D: Device + 'static> Instance<D> {
    /// Resolves the imports, instantiates and runs the start function.
    pub fn new(module: Module, state: State<D>, limits: &Limits) -> Result<Self, LoadError> {
        let module = module.0;
        let mut handlers = Vec::new();
        handlers
            .try_reserve_exact(module.import_count() as usize)
            .map_err(|_| LoadError::OutOfMemory)?;
        for i in module.imports() {
            // `validate` already matched every import against the ABI.
            let h = name(i.name).ok().and_then(handler::<D>);
            handlers.push(h.ok_or_else(|| invalid(Error::Unsupported))?);
        }
        let mut host = Host { state, handlers };
        // Whole pages only, so growth stops at the cap as it does on wasmi
        // rather than leaving a page partly backed.
        let page = module.page_size().unwrap_or(1).max(1) as usize;
        let cap = abi::memory_bytes(limits.max_memory_bytes);
        let micro = wasm_micro::Limits {
            stack: heap::stack_slots(limits.max_stack_bytes),
            frames: limits.max_call_depth as usize,
            labels: heap::labels(limits.max_stack_bytes),
            memory_bytes: cap / page * page,
            table: heap::MAX_TABLE,
            fuel: limits.fuel_per_call,
        };
        let inst = wasm_micro::Instance::new(module, &micro, &mut host).map_err(|e| match e {
            Error::OutOfMemory => LoadError::OutOfMemory,
            e => LoadError::Instantiate(e.as_str().to_string()),
        })?;
        Ok(Instance { inst, host })
    }

    /// Calls the export `name` on `fuel`.
    pub fn call(
        &mut self,
        name: &str,
        args: &[i32],
        returns: bool,
        fuel: u64,
    ) -> Result<i32, Trap> {
        let f = self.inst.module().export_func(name);
        let f = f.ok_or_else(|| trap(Error::Invalid))?;
        let mut raw = [0u64; 6];
        let n = args.len().min(raw.len());
        for (d, s) in raw.iter_mut().zip(args) {
            *d = u64::from(*s as u32);
        }
        let mut out = [0u64; 1];
        self.inst.set_fuel(fuel);
        self.inst
            .call(
                &mut self.host,
                f,
                &raw[..n],
                &mut out[..usize::from(returns)],
            )
            .map(|()| out[0] as i32)
            .map_err(trap)
    }

    /// Fuel left after the last call.
    pub fn fuel_left(&self) -> Option<u64> {
        Some(self.inst.fuel())
    }

    pub fn state(&self) -> &State<D> {
        &self.host.state
    }

    pub fn state_mut(&mut self) -> &mut State<D> {
        &mut self.host.state
    }

    pub fn into_state(self) -> State<D> {
        self.host.state
    }

    /// Linear memory (a module has at most one, and `validate` required
    /// it to be exported).
    pub fn memory(&self) -> &[u8] {
        self.inst.memory()
    }

    pub fn memory_mut(&mut self) -> &mut [u8] {
        self.inst.memory_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Pull;
    use middleware::wasm_abi::{self, IMPORTS};

    struct Board;
    impl Device for Board {
        fn log(&mut self, _: &str) {}
        fn publish(&mut self, _: middleware::Key<'_>, _: &[u8]) -> Result<(), AbiError> {
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

    #[test]
    fn every_abi_import_has_a_handler_and_nothing_else_does() {
        assert_eq!(IMPORTS.len(), wasm_abi::ABI10_IMPORTS);
        for i in IMPORTS {
            assert!(handler::<Board>(i.name).is_some(), "{}", i.name);
        }
        assert!(handler::<Board>("fopen").is_none());
    }
}
