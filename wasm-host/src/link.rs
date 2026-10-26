//! Host imports: the per-payload [`State`] kept in the wasmi store, the
//! import implementations, and the thin glue that registers them with a
//! wasmi [`Linker`].
//!
//! Each import is split in two. The `*_call` functions take a view of linear
//! memory and the state and return `Result<_, AbiError>`; they hold the
//! logic, delegate argument checks to [`crate::abi`], and are tested with a
//! fake device. The closures in [`linker`] only fetch memory from the
//! [`Caller`] and fold the result into an `i32` code, so a misbehaving
//! payload gets an error code and never traps the host.
//!
//! The ABI 2 imports that move bytes (`spi_transfer`, `uart_write`,
//! `dma_read`, `uart_read`) never hand guest memory to the device: they copy
//! through [`State::scratch`], a host buffer of at most
//! [`periph::MAX_DMA`] bytes, so the device only ever sees host-owned data.
//!
//! The ABI 3 imports (PWM, ADC, I2C) are registered by `link_abi3`.
//! `i2c_transfer` is synchronous and copies through two stack buffers of
//! [`periph::MAX_I2C`] bytes: transmit bytes leave guest memory before the
//! device runs, received bytes enter it only after the device succeeds.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use middleware::wasm_abi::periph::{
    self, I2cConfig, MAX_I2C, PwmDuty, PwmSpec, SpiConfig, TimerSpec, UartConfig,
};
use middleware::wasm_abi::{self, AbiError, IMPORT_MODULE, MAX_MESSAGE, guest_range};
use wasmi::{Caller, Engine, Extern, Linker, StoreLimits};

use crate::abi::{self, Schedule, Slot};
use crate::{Device, InitLog};

/// Which optional exports the module provides.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Exports {
    /// `init` is exported.
    pub init: bool,
    /// `tick` is exported.
    pub tick: bool,
    /// Both `mw_alloc` and `on_message` are exported.
    pub messages: bool,
    /// `on_irq` is exported (ABI 2).
    pub irq: bool,
}

/// Everything the host keeps per payload, stored as the wasmi store data.
pub struct State<D> {
    /// The board the payload drives.
    pub device: D,
    /// Subscribed keys; the index is the slot passed to `on_message`.
    pub subscriptions: Vec<String>,
    /// Active `tick` schedule, if any.
    pub schedule: Option<Schedule>,
    /// Optional exports present in the module.
    pub exports: Exports,
    /// Memory limiter consulted by wasmi on growth.
    pub limits: StoreLimits,
    /// Host copy buffer for DMA and UART imports; grown on demand, never
    /// beyond [`periph::MAX_DMA`] bytes.
    pub scratch: Vec<u8>,
    /// While `init` runs, a bounded copy of its log lines (see [`InitLog`]);
    /// `None` at all other times.
    pub init_log: Option<InitLog>,
}

/// The first `n` bytes of the scratch buffer (`n <= MAX_DMA`), zeroed.
fn scratch(buf: &mut Vec<u8>, n: usize) -> &mut [u8] {
    let n = n.min(periph::MAX_DMA);
    buf.clear();
    buf.resize(n, 0);
    buf.as_mut_slice()
}

/// `gpio_irq(pin, edge)`.
pub fn gpio_irq_call<D: Device>(st: &mut State<D>, pin: i32, edge: i32) -> Result<(), AbiError> {
    let (pin, edge) = abi::gpio_irq(pin, edge)?;
    st.device.gpio_irq(pin, edge)
}

/// `timer_start(id, period_us, periodic)`.
pub fn timer_start_call<D: Device>(
    st: &mut State<D>,
    (id, period_us, periodic): (i32, i32, i32),
) -> Result<(), AbiError> {
    st.device
        .timer_start(TimerSpec::parse(id, period_us, periodic)?)
}

/// `timer_stop(id)`.
pub fn timer_stop_call<D: Device>(st: &mut State<D>, id: i32) -> Result<(), AbiError> {
    st.device.timer_stop(periph::timer_id(id)?)
}

/// `spi_open(bus, sck, mosi, miso, cs, freq_khz, mode)`.
pub fn spi_open_call<D: Device>(
    st: &mut State<D>,
    (bus, sck, mosi, miso, cs, freq, mode): (i32, i32, i32, i32, i32, i32, i32),
) -> Result<(), AbiError> {
    st.device
        .spi_open(SpiConfig::parse(bus, sck, mosi, miso, cs, freq, mode)?)
}

/// `spi_transfer(bus, tx_ptr, tx_len, rx_len)`: copies the transmit bytes
/// to the host buffer and starts the transfer.
pub fn spi_transfer_call<D: Device>(
    mem: &[u8],
    st: &mut State<D>,
    (bus, tx_ptr, tx_len, rx_len): (i32, i32, i32, i32),
) -> Result<(), AbiError> {
    let plan = abi::spi_transfer(bus, tx_ptr, tx_len, rx_len, mem.len())?;
    let src = mem.get(plan.tx.clone()).ok_or(AbiError::OutOfBounds)?;
    let tx = scratch(&mut st.scratch, src.len());
    tx.copy_from_slice(src.get(..tx.len()).ok_or(AbiError::OutOfBounds)?);
    st.device.spi_transfer(plan.bus, tx, plan.len)
}

/// `dma_read(channel, ptr, len)`: reads at most [`periph::MAX_DMA`] bytes
/// from the device and copies them into guest memory; returns the count.
pub fn dma_read_call<D: Device>(
    mem: &mut [u8],
    st: &mut State<D>,
    (channel, ptr, len): (i32, i32, i32),
) -> Result<i32, AbiError> {
    let channel = abi::dma_channel(channel)?;
    let target = abi::read_target(ptr, len, mem.len(), periph::MAX_DMA)?;
    let buf = scratch(&mut st.scratch, target.len());
    let n = st.device.dma_read(channel, buf)?;
    copy_out(mem, target.start, buf, n)
}

/// `uart_open(port, tx, rx, baud)`.
pub fn uart_open_call<D: Device>(
    st: &mut State<D>,
    (port, tx, rx, baud): (i32, i32, i32, i32),
) -> Result<(), AbiError> {
    st.device.uart_open(UartConfig::parse(port, tx, rx, baud)?)
}

/// `uart_write(port, ptr, len)`: copies the bytes to the host buffer and
/// starts the transmission.
pub fn uart_write_call<D: Device>(
    mem: &[u8],
    st: &mut State<D>,
    (port, ptr, len): (i32, i32, i32),
) -> Result<(), AbiError> {
    let (port, range) = abi::uart_write(port, ptr, len, mem.len())?;
    let src = mem.get(range).ok_or(AbiError::OutOfBounds)?;
    let data = scratch(&mut st.scratch, src.len());
    data.copy_from_slice(src.get(..data.len()).ok_or(AbiError::OutOfBounds)?);
    st.device.uart_write(port, data)
}

/// `uart_read(port, ptr, len)`: reads at most
/// [`periph::UART_RX_BUFFER`] buffered bytes into guest memory; returns the
/// count (may be 0).
pub fn uart_read_call<D: Device>(
    mem: &mut [u8],
    st: &mut State<D>,
    (port, ptr, len): (i32, i32, i32),
) -> Result<i32, AbiError> {
    let port = periph::uart_port(port)?;
    let target = abi::read_target(ptr, len, mem.len(), periph::UART_RX_BUFFER)?;
    let buf = scratch(&mut st.scratch, target.len());
    let n = st.device.uart_read(port, buf)?;
    copy_out(mem, target.start, buf, n)
}

/// `pwm_open(timer, freq_hz)`.
pub fn pwm_open_call<D: Device>(
    st: &mut State<D>,
    (timer, freq_hz): (i32, i32),
) -> Result<(), AbiError> {
    st.device.pwm_open(PwmSpec::parse(timer, freq_hz)?)
}

/// `pwm_set(timer, channel, duty)`.
pub fn pwm_set_call<D: Device>(
    st: &mut State<D>,
    (timer, channel, duty): (i32, i32, i32),
) -> Result<(), AbiError> {
    let d = PwmDuty::parse(timer, channel, duty)?;
    st.device.pwm_set(d.timer, d.channel, d.duty)
}

/// `pwm_stop(timer)`.
pub fn pwm_stop_call<D: Device>(st: &mut State<D>, timer: i32) -> Result<(), AbiError> {
    st.device.pwm_stop(periph::pwm_timer(timer)?)
}

/// `adc_read(pin)`: the raw reading, clamped to [`periph::ADC_MAX`].
pub fn adc_read_call<D: Device>(st: &mut State<D>, pin: i32) -> Result<i32, AbiError> {
    let raw = st.device.adc_read(periph::adc_pin(pin)?)?;
    Ok(periph::adc_result(raw))
}

/// `i2c_open(bus, freq_hz)`.
pub fn i2c_open_call<D: Device>(
    st: &mut State<D>,
    (bus, freq_hz): (i32, i32),
) -> Result<(), AbiError> {
    st.device.i2c_open(I2cConfig::parse(bus, freq_hz)?)
}

/// `i2c_transfer(bus, addr7, tx_ptr, tx_len, rx_ptr, rx_len)`: copies the
/// transmit bytes out of guest memory, runs the blocking transfer, and on
/// success copies the received bytes into guest memory. On error guest
/// memory is left untouched.
pub fn i2c_transfer_call<D: Device>(
    mem: &mut [u8],
    st: &mut State<D>,
    args: (i32, i32, i32, i32, i32, i32),
) -> Result<(), AbiError> {
    let plan = abi::i2c_transfer(args, mem.len())?;
    let (mut tx_buf, mut rx_buf) = ([0u8; MAX_I2C], [0u8; MAX_I2C]);
    let src = mem.get(plan.tx.clone()).ok_or(AbiError::OutOfBounds)?;
    let tx = tx_buf
        .get_mut(..src.len())
        .ok_or(AbiError::InvalidArgument)?;
    tx.copy_from_slice(src);
    let rx = rx_buf
        .get_mut(..plan.rx.len())
        .ok_or(AbiError::InvalidArgument)?;
    let t = plan.transfer;
    st.device.i2c_transfer(t.bus, t.addr, tx, rx)?;
    mem.get_mut(plan.rx)
        .ok_or(AbiError::OutOfBounds)?
        .copy_from_slice(rx);
    Ok(())
}

/// Copies the first `reported` bytes of `buf` (clamped to its length) into
/// guest memory at `start`; returns the count copied.
fn copy_out(mem: &mut [u8], start: usize, buf: &[u8], reported: usize) -> Result<i32, AbiError> {
    let n = abi::copied(reported, buf.len());
    let dst = mem.get_mut(start..start + n).ok_or(AbiError::OutOfBounds)?;
    dst.copy_from_slice(buf.get(..n).ok_or(AbiError::OutOfBounds)?);
    i32::try_from(n).map_err(|_| AbiError::InvalidArgument)
}

/// `log(ptr, len)`: forwards the (truncated) line to the device and, while
/// `init` runs, records it in [`State::init_log`].
pub fn log_call<D: Device>(
    mem: &[u8],
    st: &mut State<D>,
    ptr: i32,
    len: i32,
) -> Result<(), AbiError> {
    let range = guest_range(ptr, len, mem.len())?;
    let bytes = mem.get(range).ok_or(AbiError::OutOfBounds)?;
    let line = abi::log_line(bytes);
    st.device.log(line);
    if let Some(log) = st.init_log.as_mut() {
        log.push(line);
    }
    Ok(())
}

/// `publish(key_ptr, key_len, data_ptr, data_len)`.
pub fn publish_call<D: Device>(
    mem: &[u8],
    st: &mut State<D>,
    (kp, kl, dp, dl): (i32, i32, i32, i32),
) -> Result<(), AbiError> {
    let key = mem
        .get(guest_range(kp, kl, mem.len())?)
        .ok_or(AbiError::OutOfBounds)?;
    let data = mem
        .get(guest_range(dp, dl, mem.len())?)
        .ok_or(AbiError::OutOfBounds)?;
    if data.len() > MAX_MESSAGE {
        return Err(AbiError::InvalidArgument);
    }
    let key = abi::payload_key(key)?;
    st.device.publish(key, data)
}

/// `subscribe(key_ptr, key_len)`: returns the slot.
pub fn subscribe_call<D>(mem: &[u8], st: &mut State<D>, kp: i32, kl: i32) -> Result<i32, AbiError> {
    let key = mem
        .get(guest_range(kp, kl, mem.len())?)
        .ok_or(AbiError::OutOfBounds)?;
    let key = abi::payload_key(key)?;
    if !st.exports.messages {
        return Err(AbiError::Unsupported);
    }
    let slot = abi::subscribe_slot(&st.subscriptions, key.as_str())?;
    if let Slot::New(_) = slot {
        st.subscriptions.push(key.as_str().to_string());
    }
    i32::try_from(slot.index()).map_err(|_| AbiError::NoResources)
}

/// `set_tick(period_ms)`.
pub fn set_tick_call<D: Device>(st: &mut State<D>, period: i32) -> Result<(), AbiError> {
    let now = st.device.now_ms();
    st.schedule = abi::set_tick(period, now, st.exports.tick)?;
    Ok(())
}

/// Fetches the payload's exported memory, or `None` if it has none.
fn memory<D>(caller: &Caller<'_, State<D>>) -> Option<wasmi::Memory> {
    caller
        .get_export(wasm_abi::exports::MEMORY)
        .and_then(Extern::into_memory)
}

/// Runs `f` with linear memory and the state, folding errors into codes.
fn with_memory<D, F>(mut caller: Caller<'_, State<D>>, f: F) -> i32
where
    F: FnOnce(&mut [u8], &mut State<D>) -> Result<i32, AbiError>,
{
    match memory(&caller) {
        Some(mem) => {
            let (bytes, st) = mem.data_and_store_mut(&mut caller);
            abi::result_code(f(bytes, st))
        }
        None => AbiError::OutOfBounds.code(),
    }
}

/// Folds a unit result into `0` or an error code.
fn ok0(r: Result<(), AbiError>) -> Result<i32, AbiError> {
    r.map(|()| 0)
}

/// Builds a linker providing every import in [`wasm_abi::IMPORTS`].
pub fn linker<D: Device + 'static>(engine: &Engine) -> Result<Linker<State<D>>, wasmi::Error> {
    let mut l = Linker::<State<D>>::new(engine);
    let m = IMPORT_MODULE;
    l.func_wrap(m, "log", |c: Caller<'_, State<D>>, p: i32, n: i32| {
        // `log` has no result; a bad pointer is silently dropped.
        let _ = with_memory(c, |mem, st| ok0(log_call(mem, st, p, n)));
    })?;
    l.func_wrap(
        m,
        "publish",
        |c: Caller<'_, State<D>>, kp: i32, kl: i32, dp: i32, dl: i32| {
            with_memory(c, |mem, st| ok0(publish_call(mem, st, (kp, kl, dp, dl))))
        },
    )?;
    l.func_wrap(
        m,
        "subscribe",
        |c: Caller<'_, State<D>>, kp: i32, kl: i32| {
            with_memory(c, |mem, st| subscribe_call(mem, st, kp, kl))
        },
    )?;
    l.func_wrap(m, "now_ms", |mut c: Caller<'_, State<D>>| {
        c.data_mut().device.now_ms() as i32
    })?;
    l.func_wrap(m, "set_tick", |mut c: Caller<'_, State<D>>, period: i32| {
        abi::result_code(ok0(set_tick_call(c.data_mut(), period)))
    })?;
    l.func_wrap(m, "gpio_output", |mut c: Caller<'_, State<D>>, pin: i32| {
        let r = abi::pin(pin).and_then(|p| c.data_mut().device.gpio_output(p));
        abi::result_code(ok0(r))
    })?;
    l.func_wrap(
        m,
        "gpio_input",
        |mut c: Caller<'_, State<D>>, pin: i32, pull: i32| {
            let r = abi::pin(pin)
                .and_then(|p| Ok((p, abi::pull(pull)?)))
                .and_then(|(p, pl)| c.data_mut().device.gpio_input(p, pl));
            abi::result_code(ok0(r))
        },
    )?;
    l.func_wrap(
        m,
        "gpio_write",
        |mut c: Caller<'_, State<D>>, pin: i32, level: i32| {
            let r = abi::pin(pin)
                .and_then(|p| Ok((p, abi::level(level)?)))
                .and_then(|(p, high)| c.data_mut().device.gpio_write(p, high));
            abi::result_code(ok0(r))
        },
    )?;
    l.func_wrap(m, "gpio_read", |mut c: Caller<'_, State<D>>, pin: i32| {
        let r = abi::pin(pin)
            .and_then(|p| c.data_mut().device.gpio_read(p))
            .map(i32::from);
        abi::result_code(r)
    })?;
    link_abi2(&mut l)?;
    link_abi3(&mut l)?;
    Ok(l)
}

/// Registers the ABI 2 imports (interrupts, timers, SPI and UART DMA).
fn link_abi2<D: Device + 'static>(l: &mut Linker<State<D>>) -> Result<(), wasmi::Error> {
    let m = IMPORT_MODULE;
    l.func_wrap(m, "now_us", |mut c: Caller<'_, State<D>>| {
        c.data_mut().device.now_us() as i32
    })?;
    l.func_wrap(m, "irq_overflow", |mut c: Caller<'_, State<D>>| {
        abi::count_code(c.data_mut().device.irq_overflow())
    })?;
    l.func_wrap(
        m,
        "gpio_irq",
        |mut c: Caller<'_, State<D>>, pin: i32, edge: i32| {
            abi::result_code(ok0(gpio_irq_call(c.data_mut(), pin, edge)))
        },
    )?;
    l.func_wrap(
        m,
        "timer_start",
        |mut c: Caller<'_, State<D>>, id: i32, period: i32, periodic: i32| {
            abi::result_code(ok0(timer_start_call(c.data_mut(), (id, period, periodic))))
        },
    )?;
    l.func_wrap(m, "timer_stop", |mut c: Caller<'_, State<D>>, id: i32| {
        abi::result_code(ok0(timer_stop_call(c.data_mut(), id)))
    })?;
    l.func_wrap(
        m,
        "spi_open",
        |mut c: Caller<'_, State<D>>,
         bus: i32,
         sck: i32,
         mosi: i32,
         miso: i32,
         cs: i32,
         freq: i32,
         mode: i32| {
            let args = (bus, sck, mosi, miso, cs, freq, mode);
            abi::result_code(ok0(spi_open_call(c.data_mut(), args)))
        },
    )?;
    l.func_wrap(
        m,
        "spi_transfer",
        |c: Caller<'_, State<D>>, bus: i32, ptr: i32, tx_len: i32, rx_len: i32| {
            with_memory(c, |mem, st| {
                ok0(spi_transfer_call(mem, st, (bus, ptr, tx_len, rx_len)))
            })
        },
    )?;
    l.func_wrap(
        m,
        "dma_read",
        |c: Caller<'_, State<D>>, channel: i32, ptr: i32, len: i32| {
            with_memory(c, |mem, st| dma_read_call(mem, st, (channel, ptr, len)))
        },
    )?;
    l.func_wrap(
        m,
        "uart_open",
        |mut c: Caller<'_, State<D>>, port: i32, tx: i32, rx: i32, baud: i32| {
            abi::result_code(ok0(uart_open_call(c.data_mut(), (port, tx, rx, baud))))
        },
    )?;
    l.func_wrap(
        m,
        "uart_write",
        |c: Caller<'_, State<D>>, port: i32, ptr: i32, len: i32| {
            with_memory(c, |mem, st| ok0(uart_write_call(mem, st, (port, ptr, len))))
        },
    )?;
    l.func_wrap(
        m,
        "uart_read",
        |c: Caller<'_, State<D>>, port: i32, ptr: i32, len: i32| {
            with_memory(c, |mem, st| uart_read_call(mem, st, (port, ptr, len)))
        },
    )?;
    Ok(())
}

/// Registers the ABI 3 imports (PWM, ADC, I2C).
fn link_abi3<D: Device + 'static>(l: &mut Linker<State<D>>) -> Result<(), wasmi::Error> {
    let m = IMPORT_MODULE;
    l.func_wrap(
        m,
        "pwm_open",
        |mut c: Caller<'_, State<D>>, timer: i32, freq: i32| {
            abi::result_code(ok0(pwm_open_call(c.data_mut(), (timer, freq))))
        },
    )?;
    l.func_wrap(
        m,
        "pwm_set",
        |mut c: Caller<'_, State<D>>, timer: i32, channel: i32, duty: i32| {
            abi::result_code(ok0(pwm_set_call(c.data_mut(), (timer, channel, duty))))
        },
    )?;
    l.func_wrap(m, "pwm_stop", |mut c: Caller<'_, State<D>>, timer: i32| {
        abi::result_code(ok0(pwm_stop_call(c.data_mut(), timer)))
    })?;
    l.func_wrap(m, "adc_read", |mut c: Caller<'_, State<D>>, pin: i32| {
        abi::result_code(adc_read_call(c.data_mut(), pin))
    })?;
    l.func_wrap(
        m,
        "i2c_open",
        |mut c: Caller<'_, State<D>>, bus: i32, freq: i32| {
            abi::result_code(ok0(i2c_open_call(c.data_mut(), (bus, freq))))
        },
    )?;
    l.func_wrap(
        m,
        "i2c_transfer",
        |c: Caller<'_, State<D>>,
         bus: i32,
         addr: i32,
         tx_ptr: i32,
         tx_len: i32,
         rx_ptr: i32,
         rx_len: i32| {
            let args = (bus, addr, tx_ptr, tx_len, rx_ptr, rx_len);
            with_memory(c, |mem, st| ok0(i2c_transfer_call(mem, st, args)))
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Pull;
    use alloc::vec;
    use middleware::Key;

    #[derive(Default)]
    struct Rec {
        logs: Vec<String>,
        published: Vec<(String, Vec<u8>)>,
    }

    impl Device for Rec {
        fn log(&mut self, line: &str) {
            self.logs.push(line.to_string());
        }
        fn publish(&mut self, key: Key<'_>, data: &[u8]) -> Result<(), AbiError> {
            self.published
                .push((key.as_str().to_string(), data.to_vec()));
            Ok(())
        }
        fn now_ms(&mut self) -> u32 {
            1000
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
            Ok(true)
        }
    }

    fn state(exports: Exports) -> State<Rec> {
        State {
            device: Rec::default(),
            subscriptions: Vec::new(),
            schedule: None,
            exports,
            limits: wasmi::StoreLimitsBuilder::new().build(),
            scratch: Vec::new(),
            init_log: None,
        }
    }

    #[test]
    fn publish_checks_bounds_size_and_key() {
        let mut mem = vec![0u8; 1024];
        mem[..5].copy_from_slice(b"a/key");
        mem[8..10].copy_from_slice(b"hi");
        let mut st = state(Exports::default());
        assert_eq!(publish_call(&mem, &mut st, (0, 5, 8, 2)), Ok(()));
        assert_eq!(
            st.device.published,
            vec![("a/key".to_string(), b"hi".to_vec())]
        );
        assert_eq!(
            publish_call(&mem, &mut st, (0, 5, 1020, 8)),
            Err(AbiError::OutOfBounds)
        );
        assert_eq!(
            publish_call(&mem, &mut st, (0, 5, 0, MAX_MESSAGE as i32 + 1)),
            Err(AbiError::InvalidArgument)
        );
        mem[..5].copy_from_slice(b"sys/x");
        assert_eq!(
            publish_call(&mem, &mut st, (0, 5, 8, 2)),
            Err(AbiError::InvalidKey)
        );
    }

    #[test]
    fn subscribe_needs_message_exports() {
        let mem = b"a/key".to_vec();
        let mut st = state(Exports::default());
        assert_eq!(
            subscribe_call(&mem, &mut st, 0, 5),
            Err(AbiError::Unsupported)
        );
        let mut st = state(Exports {
            messages: true,
            ..Exports::default()
        });
        assert_eq!(subscribe_call(&mem, &mut st, 0, 5), Ok(0));
        assert_eq!(subscribe_call(&mem, &mut st, 0, 5), Ok(0));
        assert_eq!(st.subscriptions, vec!["a/key".to_string()]);
    }

    #[test]
    fn set_tick_uses_device_clock() {
        let mut st = state(Exports {
            tick: true,
            ..Exports::default()
        });
        assert_eq!(set_tick_call(&mut st, 100), Ok(()));
        assert_eq!(
            st.schedule,
            Some(Schedule {
                period: 100,
                due: 1100
            })
        );
    }

    #[test]
    fn log_rejects_bad_pointer() {
        let mut st = state(Exports::default());
        assert_eq!(log_call(b"hey", &mut st, 0, 3), Ok(()));
        assert_eq!(log_call(b"hey", &mut st, 2, 3), Err(AbiError::OutOfBounds));
        assert_eq!(st.device.logs, vec!["hey".to_string()]);
        assert_eq!(st.init_log, None, "not recorded outside init");
    }

    #[test]
    fn log_is_recorded_while_init_runs() {
        let mut st = state(Exports::default());
        st.init_log = Some(InitLog::new());
        assert_eq!(log_call(b"hey", &mut st, 0, 3), Ok(()));
        let kept: Vec<&str> = st
            .init_log
            .as_ref()
            .map(|l| l.lines().collect())
            .unwrap_or_default();
        assert_eq!(kept, vec!["hey"]);
        assert_eq!(st.device.logs, vec!["hey".to_string()], "still forwarded");
    }

    #[test]
    fn i2c_transfer_leaves_memory_alone_on_error() {
        let mut mem = vec![0xAAu8; 64];
        let mut st = state(Exports::default());
        assert_eq!(
            i2c_transfer_call(&mut mem, &mut st, (0, 0x50, 0, 2, 8, 4)),
            Err(AbiError::Unsupported)
        );
        assert!(mem.iter().all(|&b| b == 0xAA));
        assert_eq!(adc_read_call(&mut st, 3), Err(AbiError::Unsupported));
        assert_eq!(
            pwm_set_call(&mut st, (0, 4, 0)),
            Err(AbiError::InvalidArgument)
        );
    }
}
