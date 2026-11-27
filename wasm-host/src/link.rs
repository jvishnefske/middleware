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
//! The ABI 3 imports (PWM, ADC, I2C) are registered by `link_abi3`, the
//! ABI 4 route imports by `link_abi4`. Route imports pass the device a key
//! borrowed from guest memory and a parsed config; no frame data crosses
//! this layer.
//!
//! The ABI 5 imports are registered by `link_abi5`: `serve` keeps the key
//! in [`State::serves`] (reserved at load) after the device accepted it,
//! and `node_id` writes the device's id into guest memory after checking
//! the range.
//!
//! The ABI 10 `manifest` import is registered by `link_abi10`: it writes the
//! device's encoded manifest, and only once the manifest fits the payload's
//! `cap`.
//!
//! The ABI 7 LED-strip imports are registered by `link_abi7`;
//! `ledstrip_write` copies the frame through a stack buffer of
//! [`ledstrip::MAX_FRAME`] bytes before the device runs.
//!
//! The ABI 3 imports
//! `i2c_transfer` is synchronous and copies through two stack buffers of
//! [`periph::MAX_I2C`] bytes: transmit bytes leave guest memory before the
//! device runs, received bytes enter it only after the device succeeds.

#[cfg(test)]
use alloc::string::{String, ToString};
#[cfg(test)]
use alloc::vec::Vec;

use middleware::loader::manifest::MANIFEST_MAX;
#[cfg(any(feature = "wasmi", test))]
use middleware::wasm_abi;
#[cfg(feature = "wasmi")]
use middleware::wasm_abi::IMPORT_MODULE;
use middleware::wasm_abi::bridge;
use middleware::wasm_abi::ledstrip::{self, LedStripSpec};
use middleware::wasm_abi::periph::{
    self, I2cConfig, MAX_I2C, PwmDuty, PwmSpec, SpiConfig, TimerSpec, UartConfig,
};
use middleware::wasm_abi::route::{self, RouteStat};
use middleware::wasm_abi::{AbiError, MAX_MESSAGE, guest_range};
#[cfg(feature = "wasmi")]
use wasmi::{Caller, Engine, Extern, Linker, StoreLimits};

use crate::abi::{self, Schedule, Slot};
use crate::fixed::{Scratch, Serves, Subscriptions, text};
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
    /// Both `mw_alloc` and `on_request` are exported (ABI 5).
    pub requests: bool,
}

/// Everything the host keeps per payload (on the `wasmi` backend, the
/// store data). Its keys, copy buffer, log and channel table are fixed-size
/// and inline ([`crate::fixed`]): the state allocates nothing.
pub struct State<D> {
    /// The board the payload drives.
    pub device: D,
    /// Subscribed keys; the index is the slot passed to `on_message`.
    pub subscriptions: Subscriptions,
    /// The most subscriptions ([`crate::Limits::max_subscriptions`]).
    pub max_subscriptions: usize,
    /// Served keys (ABI 5); the index is the slot passed to `on_request`.
    /// At most [`wasm_abi::serve::MAX_SERVES`].
    pub serves: Serves,
    /// Active `tick` schedule, if any.
    pub schedule: Option<Schedule>,
    /// Optional exports present in the module.
    pub exports: Exports,
    /// Memory limiter consulted by wasmi on growth (the `wasmi` backend
    /// only; `micro` takes its cap at instantiation).
    #[cfg(feature = "wasmi")]
    pub limits: StoreLimits,
    /// Host copy buffer for DMA and UART imports, [`periph::MAX_DMA`]
    /// bytes.
    pub scratch: Scratch,
    /// While `init` runs, a bounded copy of its log lines (see [`InitLog`]);
    /// `None` at all other times.
    pub init_log: Option<InitLog>,
    /// The I2C devices sampled routes own (SC3): the payload's
    /// `i2c_transfer` to one of them is `Busy` until its route stops.
    pub i2c_owners: route::I2cOwners,
    /// The declared channels and what they resolved to (ABI 8).
    pub channels: crate::channels::ChannelTable,
}

impl<D> State<D> {
    /// The state of a fresh payload: no keys, schedule, routes or channels
    /// yet. It may hold `max_subscriptions` subscriptions and linear memory
    /// may grow to `max_memory_bytes`.
    pub(crate) fn new(
        device: D,
        exports: Exports,
        max_subscriptions: usize,
        max_memory_bytes: u32,
    ) -> Self {
        #[cfg(not(feature = "wasmi"))]
        let _ = max_memory_bytes;
        State {
            device,
            subscriptions: Subscriptions::new(),
            max_subscriptions,
            serves: Serves::new(),
            schedule: None,
            exports,
            #[cfg(feature = "wasmi")]
            limits: wasmi::StoreLimitsBuilder::new()
                .memory_size(abi::memory_bytes(max_memory_bytes))
                .memories(1)
                .instances(1)
                .build(),
            scratch: Scratch::new(),
            init_log: None,
            i2c_owners: route::I2cOwners::new(),
            channels: crate::channels::ChannelTable::default(),
        }
    }
}

/// The first `n` bytes of the scratch buffer (`n <= MAX_DMA`), zeroed.
fn scratch(buf: &mut Scratch, n: usize) -> &mut [u8] {
    buf.clear();
    // Cannot fail: `n` is within the capacity.
    let _ = buf.resize(n.min(periph::MAX_DMA), 0);
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
    st.i2c_owners
        .check_transfer(plan.transfer.bus, plan.transfer.addr)?;
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

/// `route_source(source, key_ptr, key_len, cfg_ptr, cfg_len)` (ABI 4):
/// returns the route handle.
pub fn route_source_call<D: Device>(
    mem: &[u8],
    st: &mut State<D>,
    args: (i32, i32, i32, i32, i32),
) -> Result<i32, AbiError> {
    let (source, key) = match abi::route_source_any(args, mem)? {
        abi::RoutePlan::Frames(plan) => {
            (route::RouteSource::Frames(plan.source, plan.cfg), plan.key)
        }
        abi::RoutePlan::Samples(plan) => (route::RouteSource::Samples(plan.source), plan.key),
    };
    start_route(st, source, key)
}

/// Starts a source route on `key` through the device, with the I2C
/// ownership rules of SC3 for sampled sources: `route_source`, and the
/// runtime itself for a channel bound to a local source (BD4). Returns
/// the handle code.
pub fn start_route<D: Device>(
    st: &mut State<D>,
    source: route::RouteSource,
    key: middleware::Key<'_>,
) -> Result<i32, AbiError> {
    match source {
        route::RouteSource::Frames(source, cfg) => {
            let handle = st.device.route_source(source, key, cfg)?;
            let code = abi::route_handle_code(handle)?;
            st.i2c_owners = st.i2c_owners.release(handle);
            Ok(code)
        }
        route::RouteSource::Samples(source) => {
            let dev = st.device.sample_device(&source);
            if let Some(dev) = dev {
                st.i2c_owners.check_free(dev)?;
            }
            let handle = st.device.route_samples(source, key)?;
            let code = abi::route_handle_code(handle)?;
            st.i2c_owners = match dev {
                Some(dev) => st.i2c_owners.claim(handle, dev)?,
                None => st.i2c_owners.release(handle),
            };
            Ok(code)
        }
    }
}

/// `channel_open(name_ptr, name_len)` (ABI 8): the input's subscription
/// slot or the output's handle ([`crate::channels::ChannelTable::open`]).
pub fn channel_open_call<D>(
    mem: &[u8],
    st: &mut State<D>,
    (np, nl): (i32, i32),
) -> Result<i32, AbiError> {
    let name = mem
        .get(guest_range(np, nl, mem.len())?)
        .ok_or(AbiError::OutOfBounds)?;
    st.channels.open(name)
}

/// `channel_publish(handle, data_ptr, data_len)` (ABI 8): publishes on the
/// output channel's bound key; `InvalidArgument` for any other handle.
pub fn channel_publish_call<D: Device>(
    mem: &[u8],
    st: &mut State<D>,
    (h, dp, dl): (i32, i32, i32),
) -> Result<(), AbiError> {
    let data = mem
        .get(guest_range(dp, dl, mem.len())?)
        .ok_or(AbiError::OutOfBounds)?;
    if data.len() > MAX_MESSAGE {
        return Err(AbiError::InvalidArgument);
    }
    let key = st.channels.output_key(h)?;
    st.device.publish(key, data)
}

/// `route_sink(key_ptr, key_len, bus, cfg_ptr, cfg_len)` (ABI 4): returns
/// the route handle.
pub fn route_sink_call<D: Device>(
    mem: &[u8],
    st: &mut State<D>,
    args: (i32, i32, i32, i32, i32),
) -> Result<i32, AbiError> {
    let plan = abi::route_sink(args, mem)?;
    let handle = st.device.route_sink(plan.key, plan.bus, &plan.cfg)?;
    abi::route_handle_code(handle)
}

/// `route_bridge(src_link, key_ptr, key_len, dst_link)` (ABI 6): links
/// first, then the pointer pair, then the key; returns the bridge handle.
pub fn route_bridge_call<D: Device>(
    mem: &[u8],
    st: &mut State<D>,
    (src, kp, kl, dst): (i32, i32, i32, i32),
) -> Result<i32, AbiError> {
    let _ = bridge::links(src, dst)?;
    let key = mem
        .get(guest_range(kp, kl, mem.len())?)
        .ok_or(AbiError::OutOfBounds)?;
    let b = bridge::parse(src, key, dst)?;
    let handle = st.device.route_bridge(&b)?;
    if bridge::handle_index(handle).is_none() {
        return Err(AbiError::NoResources);
    }
    Ok(i32::from(handle))
}

/// `route_stop(handle)` (ABI 4).
pub fn route_stop_call<D: Device>(st: &mut State<D>, handle: i32) -> Result<(), AbiError> {
    let handle = route::route_handle(handle)?;
    st.device.route_stop(handle)?;
    st.i2c_owners = st.i2c_owners.release(handle);
    Ok(())
}

/// `route_stat(handle, stat)` (ABI 4): the counter, saturated to
/// `i32::MAX`.
pub fn route_stat_call<D: Device>(
    st: &mut State<D>,
    (handle, stat): (i32, i32),
) -> Result<i32, AbiError> {
    let handle = route::route_handle(handle)?;
    let stat = RouteStat::from_i32(stat)?;
    Ok(abi::count_code(st.device.route_stat(handle, stat)?))
}

/// `ledstrip_open(pin, count)` (ABI 7): returns the strip handle.
pub fn ledstrip_open_call<D: Device>(
    st: &mut State<D>,
    (pin, count): (i32, i32),
) -> Result<i32, AbiError> {
    let handle = st.device.ledstrip_open(LedStripSpec::parse(pin, count)?)?;
    abi::ledstrip_handle_code(handle)
}

/// `ledstrip_write(handle, ptr, len)` (ABI 7): copies the frame out of
/// guest memory into a stack buffer, then sends it.
pub fn ledstrip_write_call<D: Device>(
    mem: &[u8],
    st: &mut State<D>,
    (handle, ptr, len): (i32, i32, i32),
) -> Result<(), AbiError> {
    let (handle, range) = abi::ledstrip_write(handle, ptr, len, mem.len())?;
    let src = mem.get(range).ok_or(AbiError::OutOfBounds)?;
    let mut buf = [0u8; ledstrip::MAX_FRAME];
    let frame = buf.get_mut(..src.len()).ok_or(AbiError::InvalidArgument)?;
    frame.copy_from_slice(src);
    st.device.ledstrip_write(handle, frame)
}

/// `ledstrip_close(handle)` (ABI 7).
pub fn ledstrip_close_call<D: Device>(st: &mut State<D>, handle: i32) -> Result<(), AbiError> {
    st.device.ledstrip_close(ledstrip::ledstrip_handle(handle)?)
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
    let slot = abi::subscribe_slot(&st.subscriptions, key.as_str(), st.max_subscriptions)?;
    if let Slot::New(_) = slot {
        st.subscriptions
            .push(text(key.as_str()))
            .map_err(|_| AbiError::NoResources)?;
    }
    i32::try_from(slot.index()).map_err(|_| AbiError::NoResources)
}

/// `serve(key_ptr, key_len)` (ABI 5): returns the slot. The key follows
/// the publish rules (no `sys/`); the module must export `mw_alloc` and
/// `on_request`, and the device must accept the key ([`Device::serve`]),
/// before it takes a slot.
pub fn serve_call<D: Device>(
    mem: &[u8],
    st: &mut State<D>,
    kp: i32,
    kl: i32,
) -> Result<i32, AbiError> {
    let key = mem
        .get(guest_range(kp, kl, mem.len())?)
        .ok_or(AbiError::OutOfBounds)?;
    let key = abi::payload_key(key)?;
    if !st.exports.requests {
        return Err(AbiError::Unsupported);
    }
    let slot = abi::serve_slot(&st.serves, key.as_str())?;
    if let Slot::New(_) = slot {
        if st.serves.is_full() {
            return Err(AbiError::NoResources);
        }
        st.device.serve(key)?;
        st.serves
            .push(text(key.as_str()))
            .map_err(|_| AbiError::NoResources)?;
    }
    i32::try_from(slot.index()).map_err(|_| AbiError::NoResources)
}

/// `node_id(ptr)` (ABI 5): writes the device's 8-byte id at `ptr`. The
/// range is checked before the device is asked, and memory is left alone
/// on any error.
pub fn node_id_call<D: Device>(
    mem: &mut [u8],
    st: &mut State<D>,
    ptr: i32,
) -> Result<(), AbiError> {
    let target = abi::node_id_target(ptr, mem.len())?;
    let id = st.device.node_id()?;
    mem.get_mut(target)
        .ok_or(AbiError::OutOfBounds)?
        .copy_from_slice(&id.0);
    Ok(())
}

/// `manifest(ptr, cap)` (ABI 10): writes the device's encoded manifest at
/// `ptr` and returns its length. The window is checked first; the device
/// encodes into a host buffer, and guest memory is written only once the
/// manifest is known to fit `cap`, so a short `cap` never leaves a partial
/// manifest behind.
pub fn manifest_call<D: Device>(
    mem: &mut [u8],
    st: &mut State<D>,
    ptr: i32,
    cap: i32,
) -> Result<i32, AbiError> {
    let target = abi::manifest_target(ptr, cap, mem.len())?;
    let mut buf = [0u8; MANIFEST_MAX];
    let len = st.device.manifest(&mut buf)?;
    let n = abi::manifest_len(len, target.len())?;
    let bytes = buf.get(..n).ok_or(AbiError::NoResources)?;
    mem.get_mut(target.start..target.start + n)
        .ok_or(AbiError::OutOfBounds)?
        .copy_from_slice(bytes);
    i32::try_from(n).map_err(|_| AbiError::NoResources)
}

/// `set_tick(period_ms)`.
pub fn set_tick_call<D: Device>(st: &mut State<D>, period: i32) -> Result<(), AbiError> {
    let now = st.device.now_ms();
    st.schedule = abi::set_tick(period, now, st.exports.tick)?;
    Ok(())
}

#[cfg(feature = "wasmi")]
/// Fetches the payload's exported memory, or `None` if it has none.
fn memory<D>(caller: &Caller<'_, State<D>>) -> Option<wasmi::Memory> {
    caller
        .get_export(wasm_abi::exports::MEMORY)
        .and_then(Extern::into_memory)
}

#[cfg(feature = "wasmi")]
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
pub(crate) fn ok0(r: Result<(), AbiError>) -> Result<i32, AbiError> {
    r.map(|()| 0)
}

#[cfg(feature = "wasmi")]
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
    link_abi4(&mut l)?;
    link_abi5(&mut l)?;
    link_abi6(&mut l)?;
    link_abi7(&mut l)?;
    link_abi8(&mut l)?;
    link_abi10(&mut l)?;
    Ok(l)
}

#[cfg(feature = "wasmi")]
/// Registers the ABI 10 import (the node's manifest).
fn link_abi10<D: Device + 'static>(l: &mut Linker<State<D>>) -> Result<(), wasmi::Error> {
    l.func_wrap(
        IMPORT_MODULE,
        "manifest",
        |c: Caller<'_, State<D>>, ptr: i32, cap: i32| {
            with_memory(c, |mem, st| manifest_call(mem, st, ptr, cap))
        },
    )?;
    Ok(())
}

#[cfg(feature = "wasmi")]
/// Registers the ABI 8 imports (channels).
fn link_abi8<D: Device + 'static>(l: &mut Linker<State<D>>) -> Result<(), wasmi::Error> {
    let m = IMPORT_MODULE;
    l.func_wrap(
        m,
        "channel_open",
        |c: Caller<'_, State<D>>, np: i32, nl: i32| {
            with_memory(c, |mem, st| channel_open_call(mem, st, (np, nl)))
        },
    )?;
    l.func_wrap(
        m,
        "channel_publish",
        |c: Caller<'_, State<D>>, h: i32, dp: i32, dl: i32| {
            with_memory(c, |mem, st| ok0(channel_publish_call(mem, st, (h, dp, dl))))
        },
    )?;
    Ok(())
}

#[cfg(feature = "wasmi")]
/// Registers the ABI 6 import (bridging between links).
fn link_abi6<D: Device + 'static>(l: &mut Linker<State<D>>) -> Result<(), wasmi::Error> {
    l.func_wrap(
        IMPORT_MODULE,
        "route_bridge",
        |c: Caller<'_, State<D>>, src: i32, kp: i32, kl: i32, dst: i32| {
            with_memory(c, |mem, st| route_bridge_call(mem, st, (src, kp, kl, dst)))
        },
    )?;
    Ok(())
}

#[cfg(feature = "wasmi")]
/// Registers the ABI 5 imports (serving requests).
fn link_abi5<D: Device + 'static>(l: &mut Linker<State<D>>) -> Result<(), wasmi::Error> {
    let m = IMPORT_MODULE;
    l.func_wrap(m, "serve", |c: Caller<'_, State<D>>, kp: i32, kl: i32| {
        with_memory(c, |mem, st| serve_call(mem, st, kp, kl))
    })?;
    l.func_wrap(m, "node_id", |c: Caller<'_, State<D>>, ptr: i32| {
        with_memory(c, |mem, st| ok0(node_id_call(mem, st, ptr)))
    })?;
    Ok(())
}

#[cfg(feature = "wasmi")]
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

#[cfg(feature = "wasmi")]
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

#[cfg(feature = "wasmi")]
/// Registers the ABI 4 imports (DMA routes).
fn link_abi4<D: Device + 'static>(l: &mut Linker<State<D>>) -> Result<(), wasmi::Error> {
    let m = IMPORT_MODULE;
    l.func_wrap(
        m,
        "route_source",
        |c: Caller<'_, State<D>>, source: i32, kp: i32, kl: i32, cp: i32, cl: i32| {
            with_memory(c, |mem, st| {
                route_source_call(mem, st, (source, kp, kl, cp, cl))
            })
        },
    )?;
    l.func_wrap(
        m,
        "route_sink",
        |c: Caller<'_, State<D>>, kp: i32, kl: i32, bus: i32, cp: i32, cl: i32| {
            with_memory(c, |mem, st| route_sink_call(mem, st, (kp, kl, bus, cp, cl)))
        },
    )?;
    l.func_wrap(
        m,
        "route_stop",
        |mut c: Caller<'_, State<D>>, handle: i32| {
            abi::result_code(ok0(route_stop_call(c.data_mut(), handle)))
        },
    )?;
    l.func_wrap(
        m,
        "route_stat",
        |mut c: Caller<'_, State<D>>, handle: i32, stat: i32| {
            abi::result_code(route_stat_call(c.data_mut(), (handle, stat)))
        },
    )?;
    Ok(())
}

#[cfg(feature = "wasmi")]
/// Registers the ABI 7 imports (LED strips).
fn link_abi7<D: Device + 'static>(l: &mut Linker<State<D>>) -> Result<(), wasmi::Error> {
    let m = IMPORT_MODULE;
    l.func_wrap(
        m,
        "ledstrip_open",
        |mut c: Caller<'_, State<D>>, pin: i32, count: i32| {
            abi::result_code(ledstrip_open_call(c.data_mut(), (pin, count)))
        },
    )?;
    l.func_wrap(
        m,
        "ledstrip_write",
        |c: Caller<'_, State<D>>, handle: i32, ptr: i32, len: i32| {
            with_memory(c, |mem, st| {
                ok0(ledstrip_write_call(mem, st, (handle, ptr, len)))
            })
        },
    )?;
    l.func_wrap(
        m,
        "ledstrip_close",
        |mut c: Caller<'_, State<D>>, handle: i32| {
            abi::result_code(ok0(ledstrip_close_call(c.data_mut(), handle)))
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
        served: Vec<String>,
        node: Option<middleware::SenderId>,
        manifest: Option<Vec<u8>>,
    }

    /// A board with an IMU at 0x1f on I2C bus 0 and no microphone.
    #[derive(Default)]
    struct Sensors {
        i2c: Vec<u8>,
        routes: [bool; route::MAX_ROUTES],
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
        fn serve(&mut self, key: Key<'_>) -> Result<(), AbiError> {
            if key.as_str() == "refused/key" {
                return Err(AbiError::Unsupported);
            }
            self.served.push(key.as_str().to_string());
            Ok(())
        }
        fn node_id(&mut self) -> Result<middleware::SenderId, AbiError> {
            self.node.ok_or(AbiError::Unsupported)
        }
        fn manifest(&mut self, out: &mut [u8]) -> Result<usize, AbiError> {
            let m = self.manifest.as_ref().ok_or(AbiError::Unsupported)?;
            out.get_mut(..m.len())
                .ok_or(AbiError::NoResources)?
                .copy_from_slice(m);
            Ok(m.len())
        }
    }

    impl Device for Sensors {
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
        fn i2c_transfer(
            &mut self,
            _: u8,
            addr: u8,
            _: &[u8],
            _: &mut [u8],
        ) -> Result<(), AbiError> {
            self.i2c.push(addr);
            Ok(())
        }
        fn route_samples(
            &mut self,
            source: route::SampleSource,
            _: Key<'_>,
        ) -> Result<u8, AbiError> {
            if source.kind() == route::SourceKind::Microphone {
                return Err(AbiError::InvalidArgument);
            }
            let free = self
                .routes
                .iter()
                .position(|r| !r)
                .ok_or(AbiError::NoResources)?;
            self.routes[free] = true;
            u8::try_from(free).map_err(|_| AbiError::NoResources)
        }
        fn sample_device(&self, source: &route::SampleSource) -> Option<route::I2cDevice> {
            matches!(source, route::SampleSource::Imu(_))
                .then_some(route::I2cDevice { bus: 0, addr: 0x1f })
        }
        fn route_stop(&mut self, handle: u8) -> Result<(), AbiError> {
            let slot = self
                .routes
                .get_mut(usize::from(handle))
                .ok_or(AbiError::Busy)?;
            if !*slot {
                return Err(AbiError::Busy);
            }
            *slot = false;
            Ok(())
        }
    }

    #[test]
    fn sc3_a_sampled_route_makes_i2c_to_its_device_busy_until_it_stops() {
        let mut mem = vec![0u8; 64];
        mem[..7].copy_from_slice(b"app/imu");
        let imu = route::ImuConfig::new(100, 2, false, 10).expect("valid");
        mem[16..24].copy_from_slice(&imu.encode());
        mem[32] = 0x0d; // WHO_AM_I register
        let mut st = state_on(Sensors::default(), Exports::default());
        // Bus 0, 0x1f, write one byte at 32, read one byte at 40.
        let to = |addr: i32| (0, addr, 32, 1, 40, 1);
        assert_eq!(i2c_transfer_call(&mut mem, &mut st, to(0x1f)), Ok(()));
        assert_eq!(route_source_call(&mem, &mut st, (2, 0, 7, 16, 8)), Ok(0));
        assert_eq!(
            i2c_transfer_call(&mut mem, &mut st, to(0x1f)),
            Err(AbiError::Busy),
            "the route owns the IMU"
        );
        assert_eq!(
            i2c_transfer_call(&mut mem, &mut st, to(0x1a)),
            Ok(()),
            "other devices stay free"
        );
        assert_eq!(
            st.device.i2c,
            vec![0x1f, 0x1a],
            "a Busy transfer never reaches the bus"
        );
        assert_eq!(
            route_source_call(&mem, &mut st, (2, 0, 7, 16, 8)),
            Err(AbiError::Busy),
            "one route per device"
        );
        assert_eq!(st.device.routes, [true, false, false, false]);
        // A board without the source answers InvalidArgument.
        let mic = route::MicConfig::new(16_000, 0, route::AudioMode::Pcm, 160, 0).expect("valid");
        mem[16..24].copy_from_slice(&mic.encode());
        assert_eq!(
            route_source_call(&mem, &mut st, (3, 0, 7, 16, 8)),
            Err(AbiError::InvalidArgument)
        );
        assert_eq!(route_stop_call(&mut st, 0), Ok(()));
        assert_eq!(
            i2c_transfer_call(&mut mem, &mut st, to(0x1f)),
            Ok(()),
            "stopped: free again"
        );
        assert_eq!(route_stop_call(&mut st, 0), Err(AbiError::Busy));
    }

    fn state(exports: Exports) -> State<Rec> {
        state_on(Rec::default(), exports)
    }

    fn state_on<D>(device: D, exports: Exports) -> State<D> {
        State::new(device, exports, wasm_abi::MAX_SUBSCRIPTIONS, u32::MAX)
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
        assert_eq!(st.subscriptions, ["a/key"]);
    }

    #[test]
    fn serve_needs_request_exports_and_the_device() {
        let mut mem = b"a/key".to_vec();
        let mut st = state(Exports {
            messages: true,
            ..Exports::default()
        });
        assert_eq!(serve_call(&mem, &mut st, 0, 5), Err(AbiError::Unsupported));
        let mut st = state(Exports {
            requests: true,
            ..Exports::default()
        });
        assert_eq!(serve_call(&mem, &mut st, 0, 5), Ok(0));
        assert_eq!(serve_call(&mem, &mut st, 0, 5), Ok(0), "again: same slot");
        assert_eq!(st.serves, ["a/key"]);
        assert_eq!(st.device.served, vec!["a/key".to_string()], "asked once");
        assert_eq!(serve_call(&mem, &mut st, 2, 9), Err(AbiError::OutOfBounds));
        mem[..4].copy_from_slice(b"sys/");
        assert_eq!(serve_call(&mem, &mut st, 0, 5), Err(AbiError::InvalidKey));
        let refused = b"refused/key".to_vec();
        assert_eq!(
            serve_call(&refused, &mut st, 0, 11),
            Err(AbiError::Unsupported)
        );
        assert_eq!(st.serves.len(), 1, "a refused key takes no slot");
    }

    #[test]
    fn node_id_checks_the_range_and_the_device() {
        let mut mem = vec![0xAAu8; 16];
        let mut st = state(Exports::default());
        assert_eq!(
            node_id_call(&mut mem, &mut st, 0),
            Err(AbiError::Unsupported)
        );
        assert!(mem.iter().all(|&b| b == 0xAA));
        st.device.node = Some(middleware::SenderId([1, 2, 3, 4, 5, 6, 7, 8]));
        assert_eq!(
            node_id_call(&mut mem, &mut st, 9),
            Err(AbiError::OutOfBounds)
        );
        assert!(mem.iter().all(|&b| b == 0xAA));
        assert_eq!(node_id_call(&mut mem, &mut st, 8), Ok(()));
        assert_eq!(&mem[8..], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn manifest_checks_the_cap_and_the_device() {
        let mut mem = vec![0xAAu8; 64];
        let mut st = state(Exports::default());
        // No manifest on the device: nothing is written.
        assert_eq!(
            manifest_call(&mut mem, &mut st, 0, 64),
            Err(AbiError::Unsupported)
        );
        assert!(mem.iter().all(|&b| b == 0xAA));
        st.device.manifest = Some(vec![7u8; 40]);
        // A cap below the manifest's length is refused, with no partial write.
        assert_eq!(
            manifest_call(&mut mem, &mut st, 0, 39),
            Err(AbiError::InvalidArgument)
        );
        assert!(mem.iter().all(|&b| b == 0xAA));
        // Negative cap and a window outside memory are refused before the device.
        assert_eq!(
            manifest_call(&mut mem, &mut st, 0, -1),
            Err(AbiError::InvalidArgument)
        );
        assert_eq!(
            manifest_call(&mut mem, &mut st, 40, 40),
            Err(AbiError::OutOfBounds)
        );
        assert!(mem.iter().all(|&b| b == 0xAA));
        // A cap that fits writes exactly the manifest and returns its length.
        assert_eq!(manifest_call(&mut mem, &mut st, 8, 48), Ok(40));
        assert!(mem[..8].iter().all(|&b| b == 0xAA));
        assert!(mem[8..48].iter().all(|&b| b == 7));
        assert!(mem[48..].iter().all(|&b| b == 0xAA));
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
