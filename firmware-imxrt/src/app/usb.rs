//! USB OTG1 (ChipIdea) as an `embassy_usb_driver::Driver`, without
//! interrupts or `unsafe`, and the CDC-ACM serial link on it (FL6).
//!
//! An interrupt handler would need an `unsafe fn` in this crate (the HAL's
//! `Handler::on_interrupt`), which `#![forbid(unsafe_code)]` refuses, so
//! every future polls: it checks the controller's registers or the
//! transfer descriptor it waits on and, if nothing happened, sleeps
//! ([`BUS_POLL_US`], [`CONTROL_POLL_US`], [`DATA_POLL_US`] µs) so other tasks run. USB
//! gives a device far longer than that: a setup stage must be taken
//! within 50 ms of the host's previous transaction, data and status
//! stages within 500 ms, and a host retries a NAKed bulk transaction
//! until it completes.
//!
//! Every encoding and decision is the pure core's
//! ([`imxrt_base::usb`]): queue heads and transfer descriptors
//! ([`dma`]), register values ([`regs`]), the setup tripwire and bus
//! reset plan ([`control`]), PLL and PHY values ([`phy`]). This module
//! only moves them between memory and registers.
//!
//! DMA memory: the queue head list ([`QHS`], 2048-byte aligned), one
//! transfer descriptor per endpoint direction ([`DTDS`]) and one 512-byte
//! bounce buffer per endpoint direction ([`BUFS`]) are `static` atomics in
//! `.bss`, which this firmware links in DTCM. The TCM is not cached (the
//! D-cache covers SDRAM, where callers' buffers may be), so packets are
//! copied between the caller's buffer and the bounce buffer, and no cache
//! maintenance is needed. Each store is `SeqCst` (a `DMB` after it on the
//! Cortex-M7), so a descriptor is in memory before `ENDPTPRIME` is
//! written. That the controller's DMA reaches DTCM is FL5's hardware
//! check.
//!
//! Transfers: one dTD per endpoint direction, primed while that endpoint
//! is idle. A data endpoint remembers a primed dTD across a cancelled
//! `read` or `write` (the link's `select` drops a pending read whenever a
//! publication goes out), so a packet that arrives meanwhile is returned
//! by the next `read`, and a cancelled write still goes out before the
//! next one is copied in. A bus reset or an endpoint disable advances
//! [`EPOCH`]; a transfer primed before it is over (flushed) and its
//! future returns `Disabled`.
//!
//! Speed: the device describes a high-speed configuration (bulk 512).
//! Enumerated at full speed (a full-speed hub or host), the data
//! endpoints run at 64-byte packets ([`regs::PortSpeed::data_max_packet`]),
//! which a host clamping the descriptors (Linux does) accepts.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering::SeqCst};

use embassy_executor::Spawner;
use embassy_nxp::pac;
use embassy_nxp::pac::usb::regs as r;
use embassy_time::Timer;
use embassy_usb::class::cdc_acm::device::{self as cdc_acm, BufferedReceiver, CdcAcmClass, Sender};
use embassy_usb::driver::{
    self as drv, Direction, EndpointAddress, EndpointAllocError, EndpointConfig, EndpointError, EndpointInfo,
    EndpointType, Event, Unsupported,
};
use embassy_usb::{Builder, UsbDevice, UsbDeviceSpeed};
use embedded_io_async::{ErrorKind, ErrorType, Write};
use imxrt_base::usb::control::{self, SetupTripwire, Tripwire, WaitStep};
use imxrt_base::usb::dma::{self, Completion, DtdStatus, QhConfig};
use imxrt_base::usb::regs::{self, EpType, PortSpeed};
use imxrt_base::usb::{self as core_usb, Dir, Ep, needs_zlp, phy};
use middleware::serial::link::{Duplex, SerialLink};
use static_cell::{ConstStaticCell, StaticCell};

/// How often [`Bus::poll`] reads `USBSTS` for reset, suspend and resume.
pub const BUS_POLL_US: u64 = 250;
/// How often endpoint 0 is polled for a setup packet or a stage's end.
pub const CONTROL_POLL_US: u64 = 250;
/// How often a data endpoint's transfer is polled: one high-speed
/// microframe.
pub const DATA_POLL_US: u64 = 125;
/// How often a disabled endpoint is polled for being enabled.
const ENABLE_POLL_US: u64 = 1000;

/// Endpoint numbers the driver hands out: 0 (control) to 3, enough for
/// CDC-ACM (interrupt IN, bulk OUT, bulk IN).
const NUMBERS: u8 = 4;
/// Endpoint directions with a dTD and a buffer: OUT and IN per number,
/// indexed like the queue heads ([`Ep::qh_index`]).
const SLOTS: usize = NUMBERS as usize * 2;
/// Bytes of a bounce buffer: one high-speed bulk packet.
const BUF_BYTES: usize = core_usb::HS_BULK_MAX_PACKET as usize;
/// Words of a bounce buffer.
const BUF_WORDS: usize = BUF_BYTES / 4;
/// Flush attempts before giving up on an endpoint that stays primed
/// (the reference manual: a flush can lose to a packet in progress).
const FLUSH_TRIES: usize = 3;

/// The queue head list, aligned as `ENDPTLISTADDR` requires.
#[repr(C, align(2048))]
struct QhList([AtomicU32; dma::QH_LIST_WORDS]);

/// One dTD per endpoint direction (slot = queue head index).
#[repr(C, align(32))]
struct DtdPool([AtomicU32; SLOTS * dma::DTD_WORDS]);

/// One bounce buffer per endpoint direction.
#[repr(C, align(32))]
struct Buffers([AtomicU32; SLOTS * BUF_WORDS]);

/// The queue heads the controller reads (DTCM).
static QHS: QhList = QhList([const { AtomicU32::new(0) }; dma::QH_LIST_WORDS]);
/// The transfer descriptors (DTCM).
static DTDS: DtdPool = DtdPool([const { AtomicU32::new(0) }; SLOTS * dma::DTD_WORDS]);
/// The packet buffers (DTCM).
static BUFS: Buffers = Buffers([const { AtomicU32::new(0) }; SLOTS * BUF_WORDS]);

/// The driver was created (the controller and its statics are one).
static TAKEN: AtomicBool = AtomicBool::new(false);
/// Enabled endpoint directions, as [`Ep::bit`]s.
static ENABLED: AtomicU32 = AtomicU32::new(0);
/// Advanced by every bus reset and endpoint disable: a transfer primed
/// in an earlier epoch was flushed.
static EPOCH: AtomicU32 = AtomicU32::new(0);
/// The port speed after the last reset: 0 high (the default until
/// known), 1 full or low.
static FULL_SPEED: AtomicU8 = AtomicU8::new(0);

/// The controller.
const USB: pac::usb::Usb = pac::USB1;

/// The endpoint direction of queue head `slot`.
fn slot_ep(slot: usize) -> Option<Ep> {
    let number = u8::try_from(slot / 2).ok()?;
    Ep::new(number, if slot.is_multiple_of(2) { Dir::Out } else { Dir::In }).ok()
}

/// The bus address of a static's first word.
fn addr_of(words: &[AtomicU32]) -> u32 {
    words.as_ptr().addr() as u32
}

/// The bounce buffer of `slot`.
fn buffer(slot: usize) -> &'static [AtomicU32] {
    let start = slot.saturating_mul(BUF_WORDS);
    BUFS.0.get(start..start.saturating_add(BUF_WORDS)).unwrap_or(&[])
}

/// The bus address of `slot`'s bounce buffer.
fn buffer_addr(slot: usize) -> u32 {
    addr_of(&BUFS.0).wrapping_add((slot.saturating_mul(BUF_BYTES)) as u32)
}

/// The bus address of `slot`'s dTD.
fn dtd_addr(slot: usize) -> u32 {
    dma::dtd_address(addr_of(&DTDS.0), slot).unwrap_or(0)
}

/// The port speed as last seen.
fn speed() -> PortSpeed {
    if FULL_SPEED.load(SeqCst) != 0 { PortSpeed::Full } else { PortSpeed::High }
}

/// The packet size of the CDC data endpoints at the current speed.
pub fn bulk_packet() -> u16 {
    speed().data_max_packet(core_usb::HS_BULK_MAX_PACKET)
}

/// Reads a register until `value()` is zero, within the pure core's
/// bound; `false` when it never settled.
fn spin_until_zero(value: impl Fn() -> u32) -> bool {
    let mut polls = 0u32;
    loop {
        match control::wait_step(value(), polls) {
            WaitStep::Done => return true,
            WaitStep::TimedOut => return false,
            WaitStep::Again => polls = polls.saturating_add(1),
        }
    }
}

/// Cancels whatever is primed on the endpoints in `mask`.
fn flush(mask: u32) {
    for _ in 0..FLUSH_TRIES {
        USB.endptflush().write_value(r::Endptflush(mask));
        spin_until_zero(|| USB.endptflush().read().0 & mask);
        if USB.endptstat().read().0 & mask == 0 {
            break;
        }
    }
}

/// `ENDPTCTRLn`'s value.
fn ctrl_read(n: u8) -> u32 {
    match n {
        0 => USB.endptctrl0().read().0,
        1 => USB.endptctrl1().read().0,
        2 => USB.endptctrl2().read().0,
        3 => USB.endptctrl3().read().0,
        4 => USB.endptctrl4().read().0,
        5 => USB.endptctrl5().read().0,
        6 => USB.endptctrl6().read().0,
        7 => USB.endptctrl7().read().0,
        _ => 0,
    }
}

/// Writes `ENDPTCTRLn`.
fn ctrl_write(n: u8, v: u32) {
    match n {
        0 => USB.endptctrl0().write_value(r::Endptctrl0(v)),
        1 => USB.endptctrl1().write_value(r::Endptctrl1(v)),
        2 => USB.endptctrl2().write_value(r::Endptctrl2(v)),
        3 => USB.endptctrl3().write_value(r::Endptctrl3(v)),
        4 => USB.endptctrl4().write_value(r::Endptctrl4(v)),
        5 => USB.endptctrl5().write_value(r::Endptctrl5(v)),
        6 => USB.endptctrl6().write_value(r::Endptctrl6(v)),
        7 => USB.endptctrl7().write_value(r::Endptctrl7(v)),
        _ => {}
    }
}

/// Whether `ep` is enabled.
fn is_enabled(ep: Ep) -> bool {
    ENABLED.load(SeqCst) & ep.bit() != 0
}

/// Whether a setup packet waits on endpoint 0.
fn setup_pending() -> bool {
    USB.endptsetupstat().read().0 & 1 != 0
}

/// Encodes `ep`'s dTD for `len` bytes of its bounce buffer and primes it;
/// `false` if the core refused (never for the sizes used here).
fn prime_transfer(ep: Ep, len: u32) -> bool {
    let slot = ep.qh_index();
    let Ok(dtd) = dma::dtd(&DTDS.0, slot) else { return false };
    let transfer = dma::Transfer { buffer: buffer_addr(slot), len, ioc: true, next: None };
    if dma::encode_dtd(dtd, transfer).is_err() {
        return false;
    }
    match dma::prime(&QHS.0, ep, dtd_addr(slot)) {
        Ok(bits) => {
            USB.endptprime().write_value(r::Endptprime(bits));
            true
        }
        Err(_) => false,
    }
}

/// How `ep`'s transfer of `len` bytes stands.
fn completion(ep: Ep, len: u32) -> Completion {
    match dma::dtd(&DTDS.0, ep.qh_index()).and_then(dma::dtd_status) {
        Ok(status) => status.completion(len),
        Err(_) => Completion::Failed(DtdStatus::from_token(dma::TOKEN_HALTED)),
    }
}

/// Clears `ep`'s `ENDPTCOMPLETE` bit (write 1 to clear; only its own).
fn clear_complete(ep: Ep) {
    USB.endptcomplete().write_value(r::Endptcomplete(ep.bit()));
}

/// Sets every queue head up idle: endpoint 0's control heads, data heads
/// at `max_packet` per slot (64 where nothing is allocated).
fn init_queue_heads(max_packet: &[u16; SLOTS]) {
    for slot in 0..dma::QH_COUNT {
        let Some(ep) = slot_ep(slot) else { continue };
        let Ok(qh) = dma::qh(&QHS.0, ep) else { continue };
        let mp = max_packet.get(slot).copied().unwrap_or(core_usb::FS_BULK_MAX_PACKET);
        let _ = dma::init_qh(qh, control::reset_qh_config(slot, mp));
    }
}

/// Clock gate, USB PLL and PHY (NXP SDK order).
fn power_up() {
    pac::CCM.ccgr6().modify(|w| w.set_cg0(phy::CCGR6_CG0_ON));
    match phy::pll_usb1(pac::CCM_ANALOG.pll_usb1().read().0) {
        phy::PllUsb1::Running(v) => pac::CCM_ANALOG.pll_usb1().modify(|w| w.0 = v),
        phy::PllUsb1::Start { bypass, start } => {
            pac::CCM_ANALOG.pll_usb1().modify(|w| w.0 = bypass);
            pac::CCM_ANALOG.pll_usb1().modify(|w| w.0 = start);
            spin_until_zero(|| !pac::CCM_ANALOG.pll_usb1().read().0 & phy::PLL_LOCK);
            pac::CCM_ANALOG.pll_usb1().modify(|w| w.0 = phy::pll_usb1_unbypass(start));
        }
    }
    let usbphy = pac::USBPHY1;
    usbphy.ctrl_set().write_value(pac::usbphy::regs::CtrlSet(phy::PHY_SFTRST));
    usbphy.ctrl_clr().write_value(pac::usbphy::regs::CtrlClr(phy::PHY_CTRL_CLEAR));
    usbphy.pwd().write_value(pac::usbphy::regs::Pwd(phy::PHY_PWD_ON));
    usbphy.ctrl_set().write_value(pac::usbphy::regs::CtrlSet(phy::PHY_CTRL_SET));
    usbphy.tx().modify(|w| w.0 = phy::phy_tx(w.0));
}

/// Resets the controller into device mode, stopped (detached): queue
/// heads idle, the list address set, interrupts off (everything is
/// polled), status cleared.
fn controller_init(max_packet: &[u16; SLOTS]) {
    USB.usbcmd().modify(|w| w.0 = regs::usbcmd_stop(w.0));
    USB.usbcmd().modify(|w| w.0 |= regs::USBCMD_RST);
    spin_until_zero(|| USB.usbcmd().read().0 & regs::USBCMD_RST);
    USB.usbmode().write_value(r::Usbmode(regs::USBMODE_DEVICE));
    init_queue_heads(max_packet);
    if let Ok(list) = regs::endpointlistaddr(addr_of(&QHS.0)) {
        USB.endptlistaddr().write_value(r::Endptlistaddr(list));
    }
    USB.usbintr().write_value(r::Usbintr(0));
    USB.usbsts().write_value(r::Usbsts(u32::MAX));
    ctrl_write(0, regs::ENDPTCTRL0);
    ENABLED.store(Ep::EP0_OUT.bit() | Ep::EP0_IN.bit(), SeqCst);
}

/// An allocated data endpoint direction.
#[derive(Clone, Copy)]
struct EpCfg {
    ty: EpType,
    max_packet: u16,
}

/// The pure core's endpoint type.
fn ep_type(t: EndpointType) -> EpType {
    match t {
        EndpointType::Control => EpType::Control,
        EndpointType::Isochronous => EpType::Isochronous,
        EndpointType::Bulk => EpType::Bulk,
        EndpointType::Interrupt => EpType::Interrupt,
    }
}

/// The USB OTG1 device driver (one per firmware: [`Driver::new`]).
pub struct Driver {
    used: [u8; 2],
    eps: [Option<EpCfg>; SLOTS],
}

impl Driver {
    /// Powers the controller and PHY and resets the controller into device
    /// mode, detached until embassy-usb enables the bus. `None` if a
    /// driver was created before.
    pub fn new() -> Option<Driver> {
        if TAKEN.swap(true, SeqCst) {
            return None;
        }
        power_up();
        let driver = Driver { used: [0; 2], eps: [None; SLOTS] };
        controller_init(&driver.max_packets());
        Some(driver)
    }

    fn max_packets(&self) -> [u16; SLOTS] {
        let mut mp = [core_usb::FS_BULK_MAX_PACKET; SLOTS];
        for (m, e) in mp.iter_mut().zip(self.eps.iter()) {
            if let Some(e) = e {
                *m = e.max_packet;
            }
        }
        mp
    }

    fn alloc(&mut self, config: &EndpointConfig, dir: Dir) -> Result<(Ep, EndpointInfo), EndpointAllocError> {
        if usize::from(config.max_packet_size) > BUF_BYTES {
            return Err(EndpointAllocError);
        }
        let side = match dir {
            Dir::Out => 0,
            Dir::In => 1,
        };
        let used = self.used.get(side).copied().unwrap_or(0xFF);
        let requested = match config.addr {
            Some(a) => Some(u8::try_from(a.index()).map_err(|_| EndpointAllocError)?),
            None => None,
        };
        let n = core_usb::pick_number(used, requested, NUMBERS).ok_or(EndpointAllocError)?;
        let ep = Ep::new(n, dir).map_err(|_| EndpointAllocError)?;
        if let Some(u) = self.used.get_mut(side) {
            *u |= 1 << n;
        }
        if let Some(slot) = self.eps.get_mut(ep.qh_index()) {
            *slot = Some(EpCfg { ty: ep_type(config.ep_type), max_packet: config.max_packet_size });
        }
        let direction = match dir {
            Dir::Out => Direction::Out,
            Dir::In => Direction::In,
        };
        let info = EndpointInfo {
            addr: EndpointAddress::from_parts(usize::from(n), direction),
            ep_type: config.ep_type,
            max_packet_size: config.max_packet_size,
            interval_ms: config.interval_ms,
            max_burst: 0,
        };
        Ok((ep, info))
    }
}

impl<'a> drv::Driver<'a> for Driver {
    type EndpointOut = EndpointOut;
    type EndpointIn = EndpointIn;
    type ControlPipe = ControlPipe;
    type Bus = Bus;

    fn alloc_endpoint_out(&mut self, config: &EndpointConfig) -> Result<EndpointOut, EndpointAllocError> {
        let (ep, info) = self.alloc(config, Dir::Out)?;
        Ok(EndpointOut { data: DataEp { info, ep, pending: None } })
    }

    fn alloc_endpoint_in(&mut self, config: &EndpointConfig) -> Result<EndpointIn, EndpointAllocError> {
        let (ep, info) = self.alloc(config, Dir::In)?;
        Ok(EndpointIn { data: DataEp { info, ep, pending: None } })
    }

    fn start(self, _control_max_packet_size: u16) -> (Bus, ControlPipe) {
        let max_packet = self.max_packets();
        init_queue_heads(&max_packet);
        (Bus { eps: self.eps, max_packet, inited: false, suspended: false }, ControlPipe)
    }
}

/// The bus: attach, bus events, endpoint enable and stall.
pub struct Bus {
    eps: [Option<EpCfg>; SLOTS],
    max_packet: [u16; SLOTS],
    inited: bool,
    suspended: bool,
}

impl Bus {
    /// The bus reset plan ([`control::BUS_RESET`]).
    fn bus_reset(&mut self) {
        for step in control::BUS_RESET {
            match step {
                control::ResetStep::ClearSetupStat => {
                    USB.endptsetupstat().write_value(r::Endptsetupstat(USB.endptsetupstat().read().0))
                }
                control::ResetStep::ClearComplete => {
                    USB.endptcomplete().write_value(r::Endptcomplete(USB.endptcomplete().read().0))
                }
                control::ResetStep::WaitPrimeIdle => {
                    spin_until_zero(|| USB.endptprime().read().0);
                }
                control::ResetStep::FlushAll => flush(control::RESET_FLUSH),
                control::ResetStep::ClearAddress => USB.deviceaddr().write_value(r::Deviceaddr(0)),
                control::ResetStep::ResetEndpoints => {
                    ENABLED.store(Ep::EP0_OUT.bit() | Ep::EP0_IN.bit(), SeqCst);
                    EPOCH.fetch_add(1, SeqCst);
                    for n in 1..core_usb::ENDPOINTS {
                        ctrl_write(n, 0);
                    }
                    init_queue_heads(&self.max_packet);
                }
                control::ResetStep::CheckPortReset => {
                    if control::bus_reset_outcome(USB.portsc1().read().0) == control::BusReset::TooLate {
                        controller_init(&self.max_packet);
                        USB.usbcmd().modify(|w| w.0 = regs::usbcmd_run(w.0));
                    }
                }
            }
        }
    }

    /// Writes `ENDPTCTRLn` from the enabled halves of endpoint `n`.
    fn write_ctrl(&self, n: u8) {
        let half = |dir| {
            let ep = Ep::new(n, dir).ok()?;
            if !is_enabled(ep) {
                return None;
            }
            self.eps.get(ep.qh_index()).copied().flatten().map(|c| c.ty)
        };
        if let Ok(v) = regs::endptctrl(n, half(Dir::Out), half(Dir::In)) {
            ctrl_write(n, v);
        }
    }
}

impl drv::Bus for Bus {
    async fn enable(&mut self) {
        USB.usbcmd().modify(|w| w.0 = regs::usbcmd_run(w.0));
    }

    async fn disable(&mut self) {
        USB.usbcmd().modify(|w| w.0 = regs::usbcmd_stop(w.0));
    }

    async fn poll(&mut self) -> Event {
        if !self.inited {
            // No VBUS sensing: the device is powered from the port it is
            // plugged into (or the board supply), so it attaches at once.
            self.inited = true;
            return Event::PowerDetected;
        }
        loop {
            let raw = USB.usbsts().read().0;
            // Bits that are only read here or not used (transfers are
            // polled on their descriptors): clear them so they cannot pile up.
            let unused = raw & (regs::INT_UI | regs::INT_UEI | regs::INT_SEI | regs::INT_SRI);
            if unused != 0 {
                USB.usbsts().write_value(r::Usbsts(unused));
            }
            let status = regs::Status::from_usbsts(raw);
            if status.reset {
                USB.usbsts().write_value(r::Usbsts(regs::INT_URI));
                self.bus_reset();
                self.suspended = false;
                return Event::Reset;
            }
            if status.port_change {
                USB.usbsts().write_value(r::Usbsts(regs::INT_PCI));
                let portsc = USB.portsc1().read();
                let full = !matches!(PortSpeed::from_portsc1(portsc.0), Some(PortSpeed::High));
                FULL_SPEED.store(u8::from(full), SeqCst);
                if self.suspended && !portsc.susp() {
                    self.suspended = false;
                    return Event::Resume;
                }
            }
            if status.suspend {
                USB.usbsts().write_value(r::Usbsts(regs::INT_SLI));
                if !self.suspended {
                    self.suspended = true;
                    return Event::Suspend;
                }
            }
            Timer::after_micros(BUS_POLL_US).await;
        }
    }

    fn endpoint_set_enabled(&mut self, ep_addr: EndpointAddress, enabled: bool) {
        let Ok(ep) = Ep::from_address(ep_addr.into()) else { return };
        if ep.number() == 0 {
            return;
        }
        let Some(cfg) = self.eps.get(ep.qh_index()).copied().flatten() else { return };
        if enabled {
            if let Ok(qh) = dma::qh(&QHS.0, ep) {
                let _ = dma::init_qh(qh, QhConfig::data(speed().data_max_packet(cfg.max_packet)));
            }
            ENABLED.fetch_or(ep.bit(), SeqCst);
        } else {
            ENABLED.fetch_and(!ep.bit(), SeqCst);
            EPOCH.fetch_add(1, SeqCst);
            flush(ep.bit());
        }
        self.write_ctrl(ep.number());
    }

    fn endpoint_set_stalled(&mut self, ep_addr: EndpointAddress, stalled: bool) {
        if let Ok(ep) = Ep::from_address(ep_addr.into()) {
            set_stalled(ep, stalled);
        }
    }

    fn endpoint_is_stalled(&mut self, ep_addr: EndpointAddress) -> bool {
        Ep::from_address(ep_addr.into()).is_ok_and(|ep| regs::is_stalled(ctrl_read(ep.number()), ep))
    }

    async fn remote_wakeup(&mut self) -> Result<(), Unsupported> {
        Err(Unsupported)
    }
}

/// Stalls or unstalls `ep` (endpoint 0: both directions).
fn set_stalled(ep: Ep, stalled: bool) {
    let n = ep.number();
    ctrl_write(n, regs::set_stall(ctrl_read(n), ep, stalled));
}

/// Waits for endpoint 0's transfer on `ep` of `len` bytes, primed in
/// `epoch`; a new setup packet or a bus reset ends it (`Disabled`).
async fn ep0_wait(ep: Ep, len: u32, epoch: u32) -> Result<u32, EndpointError> {
    loop {
        match completion(ep, len) {
            Completion::Done { transferred } => {
                clear_complete(ep);
                return Ok(transferred);
            }
            Completion::Failed(_) => {
                flush(ep.bit());
                return Err(EndpointError::Disabled);
            }
            Completion::Pending => {}
        }
        if setup_pending() || EPOCH.load(SeqCst) != epoch {
            flush(ep.bit());
            return Err(EndpointError::Disabled);
        }
        Timer::after_micros(CONTROL_POLL_US).await;
    }
}

/// Copies the setup packet out under the tripwire ([`control::SETUP`]);
/// `None` when none waits (or the tripwire gave up on it).
fn take_setup() -> Option<[u8; 8]> {
    if !setup_pending() {
        return None;
    }
    let mut packet = None;
    for step in control::SETUP {
        match step {
            control::SetupStep::ClearSetupStat => {
                USB.endptsetupstat().write_value(r::Endptsetupstat(1));
                spin_until_zero(|| USB.endptsetupstat().read().0 & 1);
            }
            // The tripwire loop runs whole at `SetTripwire`.
            control::SetupStep::SetTripwire => {
                let qh = dma::qh(&QHS.0, Ep::EP0_OUT).ok()?;
                let mut tw = SetupTripwire::new();
                packet = loop {
                    USB.usbcmd().modify(|w| w.0 |= regs::USBCMD_SUTW);
                    let bytes = dma::qh_setup_bytes(qh).unwrap_or([0; 8]);
                    let still_set = USB.usbcmd().read().0 & regs::USBCMD_SUTW != 0;
                    match tw.after_copy(still_set, bytes) {
                        Tripwire::Done(p) => break Some(p),
                        Tripwire::Again => continue,
                        Tripwire::GaveUp => break None,
                    }
                };
            }
            control::SetupStep::Copy | control::SetupStep::CheckTripwire => {}
            control::SetupStep::ClearTripwire => USB.usbcmd().modify(|w| w.0 &= !regs::USBCMD_SUTW),
            control::SetupStep::FlushEp0 => flush(regs::flush_both(0).unwrap_or(0)),
        }
    }
    packet.map(|p| p.to_bytes())
}

/// Endpoint 0.
pub struct ControlPipe;

impl drv::ControlPipe for ControlPipe {
    fn max_packet_size(&self) -> usize {
        usize::from(core_usb::EP0_MAX_PACKET)
    }

    async fn setup(&mut self) -> [u8; 8] {
        loop {
            if let Some(packet) = take_setup() {
                return packet;
            }
            Timer::after_micros(CONTROL_POLL_US).await;
        }
    }

    async fn data_out(&mut self, buf: &mut [u8], _first: bool, _last: bool) -> Result<usize, EndpointError> {
        let len = buf.len().min(usize::from(core_usb::EP0_MAX_PACKET)) as u32;
        let epoch = EPOCH.load(SeqCst);
        if !prime_transfer(Ep::EP0_OUT, len) {
            return Err(EndpointError::Disabled);
        }
        let n = ep0_wait(Ep::EP0_OUT, len, epoch).await? as usize;
        let out = buf.get_mut(..n).ok_or(EndpointError::BufferOverflow)?;
        dma::copy_from_words(buffer(Ep::EP0_OUT.qh_index()), out).map_err(|_| EndpointError::BufferOverflow)?;
        Ok(n)
    }

    async fn data_in(&mut self, data: &[u8], _first: bool, last: bool) -> Result<(), EndpointError> {
        if data.len() > usize::from(core_usb::EP0_MAX_PACKET) {
            return Err(EndpointError::BufferOverflow);
        }
        dma::copy_to_words(buffer(Ep::EP0_IN.qh_index()), data).map_err(|_| EndpointError::BufferOverflow)?;
        let epoch = EPOCH.load(SeqCst);
        let len = data.len() as u32;
        if !prime_transfer(Ep::EP0_IN, len) {
            return Err(EndpointError::Disabled);
        }
        ep0_wait(Ep::EP0_IN, len, epoch).await?;
        if last {
            // The status stage: the host's zero-length OUT, taken in the
            // background (the next setup flushes it if it never comes).
            prime_transfer(Ep::EP0_OUT, 0);
        }
        Ok(())
    }

    async fn accept(&mut self) {
        let epoch = EPOCH.load(SeqCst);
        if prime_transfer(Ep::EP0_IN, 0) {
            let _ = ep0_wait(Ep::EP0_IN, 0, epoch).await;
        }
    }

    async fn reject(&mut self) {
        set_stalled(Ep::EP0_IN, true);
    }

    async fn accept_set_address(&mut self, addr: u8) {
        // With USBADRA the controller takes the address once the status
        // stage's IN completes, so it is written before.
        if let Ok(v) = regs::deviceaddr(addr, true) {
            USB.deviceaddr().write_value(r::Deviceaddr(v));
        }
        self.accept().await;
    }
}

/// A transfer primed on a data endpoint.
#[derive(Clone, Copy)]
struct Pending {
    epoch: u32,
    len: u32,
}

/// What both directions of a data endpoint share.
struct DataEp {
    info: EndpointInfo,
    ep: Ep,
    /// A dTD primed by a `read` or `write` whose future was dropped.
    pending: Option<Pending>,
}

impl DataEp {
    /// Waits for the pending transfer; `Disabled` if the endpoint was
    /// disabled or the bus reset meanwhile, or the transfer failed.
    async fn wait(&mut self, p: Pending) -> Result<u32, EndpointError> {
        loop {
            match completion(self.ep, p.len) {
                Completion::Done { transferred } => {
                    self.pending = None;
                    clear_complete(self.ep);
                    return Ok(transferred);
                }
                Completion::Failed(_) => {
                    self.pending = None;
                    flush(self.ep.bit());
                    return Err(EndpointError::Disabled);
                }
                Completion::Pending => {}
            }
            if !is_enabled(self.ep) || EPOCH.load(SeqCst) != p.epoch {
                self.pending = None;
                flush(self.ep.bit());
                return Err(EndpointError::Disabled);
            }
            Timer::after_micros(DATA_POLL_US).await;
        }
    }

    /// Primes `len` bytes of the bounce buffer.
    fn prime(&mut self, len: u32) -> Result<Pending, EndpointError> {
        let p = Pending { epoch: EPOCH.load(SeqCst), len };
        if !prime_transfer(self.ep, len) {
            return Err(EndpointError::Disabled);
        }
        self.pending = Some(p);
        Ok(p)
    }

    async fn wait_enabled(&mut self) {
        while !is_enabled(self.ep) {
            Timer::after_micros(ENABLE_POLL_US).await;
        }
    }

    fn stalled(&self) -> bool {
        regs::is_stalled(ctrl_read(self.ep.number()), self.ep)
    }
}

/// A data OUT endpoint (host to device).
pub struct EndpointOut {
    data: DataEp,
}

/// A data IN endpoint (device to host).
pub struct EndpointIn {
    data: DataEp,
}

impl drv::Endpoint for EndpointOut {
    fn info(&self) -> &EndpointInfo {
        &self.data.info
    }

    async fn wait_enabled(&mut self) {
        self.data.wait_enabled().await
    }

    fn set_stall(&mut self, stalled: bool) -> Result<(), Unsupported> {
        set_stalled(self.data.ep, stalled);
        Ok(())
    }

    fn is_stalled(&self) -> Result<bool, Unsupported> {
        Ok(self.data.stalled())
    }
}

impl drv::Endpoint for EndpointIn {
    fn info(&self) -> &EndpointInfo {
        &self.data.info
    }

    async fn wait_enabled(&mut self) {
        self.data.wait_enabled().await
    }

    fn set_stall(&mut self, stalled: bool) -> Result<(), Unsupported> {
        set_stalled(self.data.ep, stalled);
        Ok(())
    }

    fn is_stalled(&self) -> Result<bool, Unsupported> {
        Ok(self.data.stalled())
    }
}

impl drv::EndpointOut for EndpointOut {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, EndpointError> {
        let d = &mut self.data;
        if !is_enabled(d.ep) {
            d.pending = None;
            return Err(EndpointError::Disabled);
        }
        let epoch = EPOCH.load(SeqCst);
        let p = match d.pending {
            Some(p) if p.epoch == epoch => p,
            _ => {
                // At most one packet, and never more than `buf` holds (a
                // longer packet fails the dTD instead of overrunning).
                let packet = usize::from(speed().data_max_packet(d.info.max_packet_size));
                d.prime(packet.min(buf.len()).min(BUF_BYTES) as u32)?
            }
        };
        let n = d.wait(p).await? as usize;
        let out = buf.get_mut(..n).ok_or(EndpointError::BufferOverflow)?;
        dma::copy_from_words(buffer(d.ep.qh_index()), out).map_err(|_| EndpointError::BufferOverflow)?;
        Ok(n)
    }
}

impl drv::EndpointIn for EndpointIn {
    async fn write(&mut self, buf: &[u8]) -> Result<(), EndpointError> {
        let d = &mut self.data;
        if buf.len() > usize::from(d.info.max_packet_size) || buf.len() > BUF_BYTES {
            return Err(EndpointError::BufferOverflow);
        }
        if !is_enabled(d.ep) {
            d.pending = None;
            return Err(EndpointError::Disabled);
        }
        // A write whose future was dropped still owns the buffer.
        if let Some(p) = d.pending {
            d.wait(p).await?;
        }
        dma::copy_to_words(buffer(d.ep.qh_index()), buf).map_err(|_| EndpointError::BufferOverflow)?;
        let p = d.prime(buf.len() as u32)?;
        d.wait(p).await?;
        Ok(())
    }
}

/// The driver type the CDC class runs on.
pub type UsbDriver = Driver;

/// A CDC write failed: the host has not configured the interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotConnected;

impl core::fmt::Display for NotConnected {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("USB CDC not connected")
    }
}

impl core::error::Error for NotConnected {}

impl embedded_io_async::Error for NotConnected {
    fn kind(&self) -> ErrorKind {
        ErrorKind::NotConnected
    }
}

/// The CDC sender as a byte writer (firmware-rp's): one packet per
/// `write` at the current speed's packet size ([`bulk_packet`]) and a
/// zero-length packet on `flush` after a full one ([`needs_zlp`]), so a
/// frame ending on a packet boundary reaches the host at once.
pub struct CdcWriter {
    tx: Sender<'static, UsbDriver>,
    /// Bytes in the last packet written since the previous flush.
    last: usize,
}

impl CdcWriter {
    /// Wraps the sending half.
    pub fn new(tx: Sender<'static, UsbDriver>) -> Self {
        CdcWriter { tx, last: 0 }
    }

    async fn packet(&mut self, data: &[u8]) -> Result<(), NotConnected> {
        self.tx.write_packet(data).await.map_err(|_| NotConnected)
    }
}

impl ErrorType for CdcWriter {
    type Error = NotConnected;
}

impl Write for CdcWriter {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        let n = buf.len().min(usize::from(bulk_packet()));
        let packet = buf.get(..n).unwrap_or_default();
        self.packet(packet).await?;
        self.last = n;
        Ok(n)
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        if needs_zlp(self.last, bulk_packet()) {
            self.packet(&[]).await?;
        }
        self.last = 0;
        Ok(())
    }
}

/// Inbox of the link: none (as the LPUART link).
const INBOX: usize = 0;

/// The serial protocol link over the CDC port.
pub type UsbLink = SerialLink<Duplex<BufferedReceiver<'static, UsbDriver>, CdcWriter>, INBOX>;

/// Descriptor and control buffers of the device, and the receive buffer.
struct UsbMemory {
    config: [u8; 256],
    bos: [u8; 256],
    msos: [u8; 0],
    control: [u8; 64],
    rx: [u8; BUF_BYTES],
}

static USB_MEMORY: ConstStaticCell<UsbMemory> = ConstStaticCell::new(UsbMemory {
    config: [0; 256],
    bos: [0; 256],
    msos: [0; 0],
    control: [0; 64],
    rx: [0; BUF_BYTES],
});

static CDC_STATE: StaticCell<cdc_acm::State<'static>> = StaticCell::new();

/// Why [`start`] did not start USB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartError {
    /// Started before.
    Taken,
    /// The device task could not be spawned.
    Spawn,
}

#[embassy_executor::task]
async fn usb_task(mut device: UsbDevice<'static, UsbDriver>) -> ! {
    device.run().await
}

/// Brings up USB OTG1 as a high-speed CDC-ACM device (identity
/// [`core_usb::VID`], [`core_usb::PID`], `serial` as the serial number),
/// spawns the device task and returns the serial link over the CDC port
/// for the caller to serve.
pub fn start(spawner: Spawner, serial: &'static str) -> Result<UsbLink, StartError> {
    let mem = USB_MEMORY.try_take().ok_or(StartError::Taken)?;
    let state = CDC_STATE.try_init(cdc_acm::State::new()).ok_or(StartError::Taken)?;
    let driver = Driver::new().ok_or(StartError::Taken)?;
    let mut config = embassy_usb::Config::new(core_usb::VID, core_usb::PID);
    config.manufacturer = Some(core_usb::MANUFACTURER);
    config.product = Some(core_usb::PRODUCT);
    config.serial_number = Some(serial);
    config.max_power = 100;
    config.max_packet_size_0 = 64;
    config.max_speed = UsbDeviceSpeed::High;
    let mut builder = Builder::new(driver, config, &mut mem.config, &mut mem.bos, &mut mem.msos, &mut mem.control);
    let class = CdcAcmClass::new(&mut builder, state, cdc_acm::Config::new());
    let device = builder.build();
    let (tx, rx) = class.split();
    spawner.spawn(usb_task(device).map_err(|_| StartError::Spawn)?);
    Ok(SerialLink::with_inbox(Duplex::new(rx.into_buffered(&mut mem.rx), CdcWriter::new(tx))))
}
