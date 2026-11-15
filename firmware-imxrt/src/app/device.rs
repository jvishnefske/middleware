//! The board as a payload sees it: `wasm_host::Device` over the EVKB's
//! GPIO, ADC1 and LPI2C1, the clock and the message bus (`wasm` only),
//! ported from firmware-rp's device.
//!
//! | ABI call | RT base |
//! |----------|---------|
//! | `log`, `publish` | `sys/log` and publications on the bus (`sys/...` keys refused) |
//! | `now_ms`, `now_us` | embassy-time (the 1 MHz PIT) |
//! | `gpio_output`, `gpio_input`, `gpio_write`, `gpio_read` | embassy-nxp `Flex` pins on the board pads ([`imxrt_base::pins`] rules, `board_pins::imxrt::evkb`); output levels by a read-modify-write of the bank's `DR` ([`write_level`]); `Busy` on A4/A5 while I2C bus 0 is open |
//! | `timer_start`, `timer_stop` | four software timers (`esp_base::periph::timers`) fired by the controller loop as `Timer` events |
//! | `irq_overflow` | the event queue's drop count |
//! | `adc_read` | A0–A5 on ADC1, blocking 12-bit read ([`super::adc`]) |
//! | `i2c_open`, `i2c_transfer` | bus 0 on LPI2C1 (A4 SDA / A5 SCL), 10 kHz–1 MHz, blocking and time bounded ([`super::i2c`]) |
//! | `serve` (ABI 5) | any non-`sys` key; the controller routes its requests ([`imxrt_base::requests`]) |
//! | `node_id` (ABI 5) | the chip unique id, the node `sys/info` reports |
//! | `route_source` (Imu) | one FXOS8700CQ route ([`ImuRoute`], handle 0) on LPI2C1 at 0x1F (opened at 400 kHz if the payload has not), polled by [`RtDevice::poll`] on the route's clock, its batches published on the route key with a `RouteFrames` event each (`RouteDropped` for samples the sensor overwrote); `route_stop` puts the sensor in standby; `route_stat`: `Frames`/`Chunks` = batches published, `FramesDropped` = samples the sensor overwrote, `ChunksDropped` = failed reads and batches the bus refused |
//! | `route_source` (Microphone) | one microphone route ([`MicRoute`], handle 1): the WM8960 at 0x1A configured over LPI2C1 ([`wm8960::record_plan`] at the route's gain), SAI1 receiving its right word into a DMA ring ([`super::sai`]), drained every [`mic_route::POLL_US`] into batches of the route's mode; `route_stop` powers the codec down and stops SAI1, its DMA and the audio PLL; `route_stat` as for the IMU (`FramesDropped`: samples not published) |
//! | `spi_*`, `gpio_irq`, `uart_*`, `dma_read`, `pwm_*`, `route_sink`, frame routes | `Unsupported` (the trait's defaults); SPI (LPSPI1) is not wired to payloads |
//!
//! The hardware ([`Hw`]) rests in [`HW`] between runs. [`RtDevice::new`]
//! takes it and `Drop` puts it back with the I2C bus closed and every pin
//! the payload configured (or the bus held) returned to an input without
//! pull, so a payload that fails to instantiate, traps or is stopped
//! leaves the board as it found it.

use core::cell::RefCell;

use embassy_nxp::gpio::{Flex, Pull as HalPull};
use embassy_nxp::pac;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::Instant;
use heapless::String;
use esp_base::periph::timer_event;
use esp_base::periph::timers::Timers;
use imxrt_base::chip::Profile;
use imxrt_base::pins::{GPIOS, Pins, ReadFrom};
use imxrt_base::sensors::imu_route::ImuRoute;
use imxrt_base::sensors::mic_route::{self, MicRoute};
use imxrt_base::sensors::{FXOS8700_ADDR, WM8960_ADDR, fxos8700, wm8960};
use middleware::key::MAX_KEY_LEN;
use middleware::loader::is_system_key;
use middleware::stream::samples::MAX_BATCH;
use middleware::wasm_abi::route::{I2cDevice, RouteStat, SampleSource};
use middleware::wasm_abi::events::{Event, EventKind, EventQueue, MAX_EVENTS};
use middleware::wasm_abi::periph::{I2cConfig, TimerSpec};
use middleware::wasm_abi::{AbiError, Pull};
use middleware::{Key, SenderId};

use super::adc::Adc;
use super::bus::Bus;
use super::i2c::I2c;
use super::sai::{self, SaiRx};

/// Microseconds since boot.
pub fn now_us() -> u64 {
    Instant::now().as_micros()
}

/// Milliseconds since boot, wrapping.
pub fn now_ms() -> u32 {
    Instant::now().as_millis() as u32
}

/// The payload hardware.
pub struct Hw {
    /// The pins a payload may use as GPIO (`None` for the reserved ones).
    pub pins: [Option<Flex<'static>>; GPIOS],
    /// ADC1.
    pub adc: Adc,
    /// I2C bus 0.
    pub i2c: I2c,
}

/// Where [`Hw`] rests while no payload runs.
pub struct HwSlot(Mutex<CriticalSectionRawMutex, RefCell<Option<Hw>>>);

impl HwSlot {
    /// Empty.
    pub const fn new() -> Self {
        HwSlot(Mutex::new(RefCell::new(None)))
    }

    /// Stores the hardware.
    pub fn put(&self, hw: Hw) {
        self.0.lock(|c| c.replace(Some(hw)));
    }

    /// Takes the hardware, if it is there.
    pub fn take(&self) -> Option<Hw> {
        self.0.lock(|c| c.take())
    }
}

/// The payload hardware between runs.
pub static HW: HwSlot = HwSlot::new();

/// The device a payload runs against.
pub struct RtDevice {
    hw: Option<Hw>,
    modes: Pins,
    /// I2C bus 0 is open (its SDA and SCL pins are held).
    i2c_open: bool,
    timers: Timers,
    events: EventQueue<MAX_EVENTS>,
    bus: &'static Bus,
    profile: &'static Profile,
    node: SenderId,
    imu: Option<Imu>,
    mic: Option<Mic>,
}

/// The IMU route's handle.
const IMU_HANDLE: u8 = 0;

/// The microphone route's handle.
const MIC_HANDLE: u8 = 1;

/// LPI2C1's clock when a route opens the bus itself.
const ROUTE_I2C_HZ: u32 = 400_000;

/// A running microphone route.
struct Mic {
    route: MicRoute,
    rx: SaiRx,
    key: String<MAX_KEY_LEN>,
    /// Batches the bus refused.
    refused: u32,
    /// When the ring is drained next, in µs.
    next_us: u64,
}

/// A running IMU route.
struct Imu {
    route: ImuRoute,
    key: String<MAX_KEY_LEN>,
    /// Batches the bus refused (queue full or too large).
    refused: u32,
}

impl RtDevice {
    /// Takes the hardware from [`HW`]; `None` if it is missing. `node` is
    /// what `node_id` answers (the node `sys/info` reports).
    pub fn new(bus: &'static Bus, profile: &'static Profile, node: SenderId) -> Option<Self> {
        let hw = HW.take()?;
        Some(RtDevice {
            hw: Some(hw),
            modes: Pins::new(),
            i2c_open: false,
            timers: Timers::new(),
            events: EventQueue::new(),
            bus,
            profile,
            node,
            imu: None,
            mic: None,
        })
    }

    /// When the earliest running timer or route poll is due, in µs since
    /// boot.
    pub fn next_alarm_us(&self) -> Option<u64> {
        let imu = self.imu.as_ref().map(|i| i.route.due_us());
        let mic = self.mic.as_ref().map(|m| m.next_us);
        [self.timers.next_deadline(), imu, mic].into_iter().flatten().min()
    }

    /// Fires the timers due at `now` (µs) into the event queue and
    /// services a route whose poll is due.
    pub fn poll(&mut self, now: u64) {
        let (next, fired) = self.timers.expire(now);
        self.timers = next;
        for f in fired {
            // A full queue drops and counts the event.
            let _ = self.events.push(timer_event(f.id, f.periods, now as u32));
        }
        self.poll_imu(now);
        self.poll_mic(now);
    }

    /// Drains the microphone's ring into its route when due, publishing
    /// the batches it completes.
    fn poll_mic(&mut self, now: u64) {
        let Some(mic) = self.mic.as_mut() else { return };
        if mic.next_us > now {
            return;
        }
        mic.next_us = now.saturating_add(mic_route::POLL_US);
        let mut buf = [0i16; 256];
        let mut out = [0u8; MAX_BATCH];
        let lost_before = mic.route.lost();
        let mut published = 0u32;
        loop {
            let d = mic.rx.drain(now, &mut buf);
            mic.route.gap(d.lost);
            if d.samples == 0 {
                break;
            }
            let mut rest = buf.get(..d.samples).unwrap_or_default();
            while !rest.is_empty() {
                let fed = mic.route.feed(now, rest, &mut out);
                rest = rest.get(fed.consumed..).unwrap_or_default();
                let Some(len) = fed.batch else { continue };
                let ok = Key::new(&mic.key)
                    .ok()
                    .is_some_and(|key| self.bus.publish(key, out.get(..len).unwrap_or_default()).is_ok());
                if ok {
                    published += 1;
                } else {
                    mic.refused = mic.refused.saturating_add(1);
                }
            }
        }
        let newly_lost = mic.route.lost().saturating_sub(lost_before);
        route_events(&mut self.events, MIC_HANDLE, now, published, newly_lost);
    }

    /// Starts the microphone route: MCLK, the codec over I2C, then SAI1
    /// and its DMA.
    fn start_mic(&mut self, cfg: middleware::wasm_abi::route::MicConfig, key: Key<'_>) -> Result<u8, AbiError> {
        if self.mic.is_some() {
            return Err(AbiError::Busy);
        }
        let route = MicRoute::new(cfg, now_us())?;
        let key: String<MAX_KEY_LEN> = String::try_from(key.as_str()).map_err(|_| AbiError::InvalidKey)?;
        self.open_i2c_for_route()?;
        sai::start_clocks(route.rate())?;
        let pga = wm8960::pga_for_gain_db(route.gain_db());
        let hw = self.hw()?;
        for w in wm8960::record_plan(route.rate(), pga) {
            if let Err(e) = hw.i2c.transfer(WM8960_ADDR, &w.bytes(), &mut []) {
                sai::stop();
                return Err(e);
            }
        }
        let now = now_us();
        let rx = sai::start(route.rate(), now);
        self.mic = Some(Mic {
            route,
            rx,
            key,
            refused: 0,
            next_us: now.saturating_add(mic_route::POLL_US),
        });
        Ok(MIC_HANDLE)
    }

    /// Stops the microphone: the codec powered down, SAI1 and its DMA off.
    fn mic_stop(&mut self) {
        sai::stop();
        if self.i2c_open
            && let Some(hw) = self.hw.as_mut()
        {
            for w in wm8960::stop_plan() {
                let _ = hw.i2c.transfer(WM8960_ADDR, &w.bytes(), &mut []);
            }
        }
    }

    /// Reads `STATUS` and the samples in one transfer when the IMU route
    /// is due, and publishes a completed batch.
    fn poll_imu(&mut self, now: u64) {
        let Some(imu) = self.imu.as_mut() else { return };
        if imu.route.due_us() > now {
            return;
        }
        let Some(hw) = self.hw.as_mut() else { return };
        let mut burst = [0u8; 13];
        let n = imu.route.burst_len().min(burst.len());
        let read = hw.i2c.transfer(FXOS8700_ADDR, &[fxos8700::STATUS], &mut burst[..n]);
        if read.is_err() {
            imu.route.on_failure(now);
            return;
        }
        let mut out = [0u8; MAX_BATCH];
        let lost = imu.route.lost();
        let batch = imu.route.on_burst(now, &burst[..n], &mut out);
        let newly_lost = imu.route.lost().saturating_sub(lost);
        let published = batch.map(|len| {
            Key::new(&imu.key)
                .ok()
                .is_some_and(|key| self.bus.publish(key, &out[..len]).is_ok())
        });
        if published == Some(false) {
            imu.refused = imu.refused.saturating_add(1);
        }
        let n = u32::from(published == Some(true));
        route_events(&mut self.events, IMU_HANDLE, now, n, newly_lost);
    }

    /// Opens LPI2C1 for a route if the payload has not; `Busy` when the
    /// payload holds the bus pins as GPIO.
    fn open_i2c_for_route(&mut self) -> Result<(), AbiError> {
        if self.i2c_open {
            return Ok(());
        }
        wasm_host::Device::i2c_open(self, I2cConfig { bus: 0, freq_hz: ROUTE_I2C_HZ })
    }

    /// Writes the FXOS8700CQ's standby register (best effort).
    fn imu_standby(&mut self) {
        let (reg, value) = fxos8700::Config::stop();
        if self.i2c_open
            && let Some(hw) = self.hw.as_mut()
        {
            let _ = hw.i2c.transfer(FXOS8700_ADDR, &[reg, value], &mut []);
        }
    }

    /// Starts the IMU route: checks `WHO_AM_I`, writes the plan.
    fn start_imu(&mut self, cfg: middleware::wasm_abi::route::ImuConfig, key: Key<'_>) -> Result<u8, AbiError> {
        if self.imu.is_some() {
            return Err(AbiError::Busy);
        }
        let route = ImuRoute::new(cfg, now_us())?;
        let key: String<MAX_KEY_LEN> = String::try_from(key.as_str()).map_err(|_| AbiError::InvalidKey)?;
        self.open_i2c_for_route()?;
        let hw = self.hw()?;
        let mut who = [0u8; 1];
        hw.i2c.transfer(FXOS8700_ADDR, &[fxos8700::WHO_AM_I], &mut who)?;
        if !fxos8700::is_fxos8700(who[0]) {
            return Err(AbiError::Unsupported);
        }
        for (reg, value) in route.plan() {
            hw.i2c.transfer(FXOS8700_ADDR, &[reg, value], &mut [])?;
        }
        // The first poll is due a period from now.
        self.imu = Some(Imu { route, key, refused: 0 });
        Ok(IMU_HANDLE)
    }

    /// The next queued event.
    pub fn pop_event(&mut self) -> Option<Event> {
        self.events.pop()
    }

    /// Events dropped so far (not reset).
    pub fn overflow(&self) -> u32 {
        self.events.overflow()
    }

    fn pin(&mut self, pin: u8) -> Result<&mut Flex<'static>, AbiError> {
        self.hw
            .as_mut()
            .and_then(|hw| hw.pins.get_mut(usize::from(pin)))
            .and_then(Option::as_mut)
            .ok_or(AbiError::Unsupported)
    }

    fn hw(&mut self) -> Result<&mut Hw, AbiError> {
        self.hw.as_mut().ok_or(AbiError::Unsupported)
    }

    /// `Busy` for a pin I2C bus 0 holds.
    fn check_gpio(&self, pin: u8) -> Result<(), AbiError> {
        match self.profile.i2c_pins(0) {
            Ok((sda, scl)) if self.i2c_open && (pin == sda || pin == scl) => Err(AbiError::Busy),
            _ => Ok(()),
        }
    }
}

/// Queues a route's `RouteFrames` (batches published) and `RouteDropped`
/// (units lost) events. They coalesce in the queue, so a payload that
/// ignores them costs nothing.
fn route_events(events: &mut EventQueue<MAX_EVENTS>, handle: u8, now: u64, published: u32, lost: u32) {
    let event = |kind, value: u32| Event {
        kind,
        source: i32::from(handle),
        value: i32::try_from(value).unwrap_or(i32::MAX),
        time_us: now as u32,
    };
    if published > 0 {
        let _ = events.push(event(EventKind::RouteFrames, published));
    }
    if lost > 0 {
        let _ = events.push(event(EventKind::RouteDropped, lost));
    }
}

/// Drives `pin`'s output latch: a read-modify-write of its bank's `DR`
/// in a critical section. embassy-nxp's `Flex::set_level` writes the
/// `DR_SET` / `DR_CLEAR` registers, which the RT1062 has and the RT1052's
/// GPIO does not (on the board the writes were lost: DR stayed 0 while the
/// payload toggled D8, 2026-11-13).
fn write_level(profile: &Profile, pin: u8, high: bool) -> Result<(), AbiError> {
    use board_pins::imxrt::GpioBank;
    let line = profile.board_pin(pin)?.pad.gpio();
    let block = match line.bank() {
        GpioBank::Gpio1 => pac::GPIO1,
        GpioBank::Gpio2 => pac::GPIO2,
        GpioBank::Gpio3 => pac::GPIO3,
        GpioBank::Gpio4 => pac::GPIO4,
        GpioBank::Gpio5 => pac::GPIO5,
    };
    cortex_m::interrupt::free(|_| {
        block.dr().modify(|w| {
            w.0 = if high { w.0 | line.mask() } else { w.0 & !line.mask() };
        })
    });
    Ok(())
}

/// Returns `pin` to an input without pull.
fn release_pin(hw: &mut Hw, pin: u8) {
    if let Some(Some(f)) = hw.pins.get_mut(usize::from(pin)) {
        f.set_as_input();
        f.set_pull(HalPull::None);
    }
}

/// Muxes A5 (GPIO_AD_B1_00) and A4 (GPIO_AD_B1_01) back to GPIO (ALT5).
fn i2c_pads_to_gpio() {
    pac::IOMUXC.sw_mux_ctl_pad_gpio_ad_b1_00().write_value(pac::iomuxc::regs::MuxCtl(0x05));
    pac::IOMUXC.sw_mux_ctl_pad_gpio_ad_b1_01().write_value(pac::iomuxc::regs::MuxCtl(0x05));
}

impl Drop for RtDevice {
    fn drop(&mut self) {
        if self.imu.take().is_some() {
            self.imu_standby();
        }
        if self.mic.take().is_some() {
            self.mic_stop();
        }
        let Some(mut hw) = self.hw.take() else { return };
        if self.i2c_open {
            hw.i2c.close();
            i2c_pads_to_gpio();
            if let Ok((sda, scl)) = self.profile.i2c_pins(0) {
                release_pin(&mut hw, sda);
                release_pin(&mut hw, scl);
            }
        }
        for pin in self.modes.configured() {
            release_pin(&mut hw, pin);
        }
        HW.put(hw);
    }
}

impl wasm_host::Device for RtDevice {
    fn log(&mut self, line: &str) {
        self.bus.log(format_args!("{line}"));
    }

    fn publish(&mut self, key: Key<'_>, data: &[u8]) -> Result<(), AbiError> {
        if is_system_key(key) {
            return Err(AbiError::InvalidKey);
        }
        self.bus.publish(key, data).map_err(|_| AbiError::Transport)
    }

    fn now_ms(&mut self) -> u32 {
        now_ms()
    }

    fn now_us(&mut self) -> u32 {
        now_us() as u32
    }

    fn gpio_output(&mut self, pin: u8) -> Result<(), AbiError> {
        self.check_gpio(pin)?;
        let next = self.modes.output(self.profile, pin)?;
        self.pin(pin)?;
        write_level(self.profile, pin, false)?;
        self.pin(pin)?.set_as_output();
        self.modes = next;
        Ok(())
    }

    fn gpio_input(&mut self, pin: u8, pull: Pull) -> Result<(), AbiError> {
        self.check_gpio(pin)?;
        let next = self.modes.input(self.profile, pin)?;
        let f = self.pin(pin)?;
        f.set_as_input();
        f.set_pull(match pull {
            Pull::None => HalPull::None,
            Pull::Up => HalPull::Up100K,
            Pull::Down => HalPull::Down100K,
        });
        self.modes = next;
        Ok(())
    }

    fn gpio_write(&mut self, pin: u8, high: bool) -> Result<(), AbiError> {
        self.check_gpio(pin)?;
        self.modes.check_write(self.profile, pin)?;
        self.pin(pin)?;
        write_level(self.profile, pin, high)
    }

    fn gpio_read(&mut self, pin: u8) -> Result<bool, AbiError> {
        self.check_gpio(pin)?;
        let from = self.modes.read_from(self.profile, pin)?;
        let f = self.pin(pin)?;
        Ok(match from {
            ReadFrom::Pad => f.is_high(),
            ReadFrom::Latch => f.is_set_high(),
        })
    }

    fn irq_overflow(&mut self) -> u32 {
        self.events.take_overflow()
    }

    fn timer_start(&mut self, spec: TimerSpec) -> Result<(), AbiError> {
        self.events.purge(EventKind::Timer, i32::from(spec.id));
        self.timers = self.timers.start(spec, now_us());
        Ok(())
    }

    fn timer_stop(&mut self, id: u8) -> Result<(), AbiError> {
        self.timers = self.timers.stop(id);
        self.events.purge(EventKind::Timer, i32::from(id));
        Ok(())
    }

    fn adc_read(&mut self, pin: u8) -> Result<u16, AbiError> {
        let profile = self.profile;
        self.hw()?.adc.read(profile, pin)
    }

    /// Bus 0 only: its pins must not be configured as GPIO by the payload;
    /// configures LPI2C1 at the clock, then routes SDA and SCL.
    fn i2c_open(&mut self, cfg: I2cConfig) -> Result<(), AbiError> {
        let (sda, scl) = self.profile.i2c_pins(cfg.bus)?;
        if cfg.bus != 0 {
            return Err(AbiError::Unsupported);
        }
        let configured = |p: u8| self.modes.configured().any(|c| c == p);
        if configured(sda) || configured(scl) {
            return Err(AbiError::Busy);
        }
        self.hw()?.i2c.open(cfg.freq_hz)?;
        self.i2c_open = true;
        Ok(())
    }

    fn i2c_transfer(&mut self, bus: u8, addr: u8, tx: &[u8], rx: &mut [u8]) -> Result<(), AbiError> {
        if bus != 0 || !self.i2c_open {
            return Err(AbiError::InvalidArgument);
        }
        self.hw()?.i2c.transfer(addr, tx, rx)
    }

    /// Accepts any key a payload may serve; the controller routes its
    /// requests to `Payload::request`.
    fn serve(&mut self, key: Key<'_>) -> Result<(), AbiError> {
        esp_base::serve::servable(key)
    }

    fn node_id(&mut self) -> Result<SenderId, AbiError> {
        Ok(self.node)
    }

    /// One IMU route and one microphone route.
    fn route_samples(&mut self, source: SampleSource, key: Key<'_>) -> Result<u8, AbiError> {
        if is_system_key(key) {
            return Err(AbiError::InvalidKey);
        }
        match source {
            SampleSource::Imu(cfg) => self.start_imu(cfg, key),
            SampleSource::Microphone(cfg) => self.start_mic(cfg, key),
        }
    }

    fn sample_device(&self, source: &SampleSource) -> Option<I2cDevice> {
        Some(match source {
            SampleSource::Imu(_) => I2cDevice { bus: 0, addr: FXOS8700_ADDR },
            SampleSource::Microphone(_) => I2cDevice { bus: 0, addr: WM8960_ADDR },
        })
    }

    fn route_stop(&mut self, handle: u8) -> Result<(), AbiError> {
        match handle {
            IMU_HANDLE if self.imu.is_some() => {
                self.imu_standby();
                self.imu = None;
            }
            MIC_HANDLE if self.mic.is_some() => {
                self.mic_stop();
                self.mic = None;
            }
            _ => return Err(AbiError::Busy),
        }
        for kind in [EventKind::RouteFrames, EventKind::RouteDropped] {
            self.events.purge(kind, i32::from(handle));
        }
        Ok(())
    }

    fn route_stat(&mut self, handle: u8, stat: RouteStat) -> Result<u32, AbiError> {
        // (batches, lost, failed reads, refused)
        let (batches, lost, failures, refused) = match handle {
            IMU_HANDLE => {
                let i = self.imu.as_ref().ok_or(AbiError::Busy)?;
                (i.route.batches(), i.route.lost(), i.route.failures(), i.refused)
            }
            MIC_HANDLE => {
                let m = self.mic.as_ref().ok_or(AbiError::Busy)?;
                (m.route.batches(), m.route.lost(), 0, m.refused)
            }
            _ => return Err(AbiError::Busy),
        };
        Ok(match stat {
            RouteStat::Frames | RouteStat::Chunks => batches.saturating_sub(refused),
            RouteStat::FramesDropped => lost,
            RouteStat::ChunksDropped => failures.saturating_add(refused),
        })
    }
}
