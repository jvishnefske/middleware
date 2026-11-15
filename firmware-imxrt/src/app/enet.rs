//! Ethernet shell (FL7): the ENET MAC and the KSZ8081RNB PHY as an
//! `embassy_net_driver::Driver`, through embassy-nxp's PAC only.
//!
//! Every value written comes from the pure core ([`imxrt_base::enet`]),
//! which reproduces the RAM probe that ran on the EVKB (PHY found, link up,
//! DHCP OFFER received). The bring-up follows the order documented there:
//! clock gates, ENET PLL at 50 MHz, the reference clock out, the RMII pads,
//! the PHY's hardware reset, the MAC reset and MDC, the PHY over MDIO
//! (identity, software reset, 50 MHz RMII clock mode, auto-negotiation),
//! the rings, the MAC registers and the enable.
//!
//! No interrupts: nothing here may have an `unsafe fn` handler. The driver
//! returns `None` when the receive ring is empty or the transmit ring is
//! full and registers the runner's waker in [`WAKER`]; [`poll_task`] looks
//! at the descriptors every [`POLL_INTERVAL`] and wakes it when a frame
//! has arrived or a descriptor has freed, and reads the PHY's link every
//! [`LINK_INTERVAL`]. The driver itself does no MDIO.
//!
//! The descriptor rings and frame buffers are `static` `AtomicU32` arrays
//! in `.bss`, which is DTCM: uncached, so no cache maintenance, and
//! reachable by the ENET DMA (FL5, checked on hardware).

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering::Relaxed};
use core::task::Context;

use embassy_net_driver::{Capabilities, HardwareAddress, LinkState};
use embassy_nxp::pac;
use embassy_sync::waitqueue::AtomicWaker;
use embassy_time::{Duration, Instant, Timer};
use imxrt_base::enet::desc::{self, RxRing, TxRing};
use imxrt_base::enet::ksz8081::{self, Duplex, Link, PhyError, ResetStep, Speed};
use imxrt_base::enet::{clock, mac, mdio};

/// Receive descriptors (and buffers): eight, so a burst of frames
/// between two polls of [`poll_task`] fits (with four, a Zenoh PCM stream
/// to a Wi-Fi host lost 0.7 s bursts, SC8).
const NRX: usize = 8;
/// Transmit descriptors (and buffers).
const NTX: usize = 4;
/// Words per frame buffer (MRBR bytes).
const BUF_WORDS: usize = desc::RX_BUFFER_BYTES / 4;
/// Bytes per frame buffer.
const FRAME: usize = desc::RX_BUFFER_BYTES;
/// Receive every frame (RCR.PROM). The probe ran promiscuous; the firmware
/// needs only its own address and broadcasts (IPv4, DHCP, ARP, TCP).
const PROMISCUOUS: bool = false;

/// How often [`poll_task`] looks at the descriptors.
pub const POLL_INTERVAL: Duration = Duration::from_micros(250);
/// How often [`poll_task`] reads the PHY's link status over MDIO.
pub const LINK_INTERVAL: Duration = Duration::from_millis(500);

#[repr(C, align(64))]
struct Desc<const N: usize>([AtomicU32; N]);
#[repr(C, align(64))]
struct Buf([AtomicU32; BUF_WORDS]);

static RX_DESC: Desc<{ desc::DESC_WORDS * NRX }> =
    Desc([const { AtomicU32::new(0) }; desc::DESC_WORDS * NRX]);
static TX_DESC: Desc<{ desc::DESC_WORDS * NTX }> =
    Desc([const { AtomicU32::new(0) }; desc::DESC_WORDS * NTX]);
static RX_BUF: [Buf; NRX] = [const { Buf([const { AtomicU32::new(0) }; BUF_WORDS]) }; NRX];
static TX_BUF: [Buf; NTX] = [const { Buf([const { AtomicU32::new(0) }; BUF_WORDS]) }; NTX];

/// The runner's waker, woken by [`poll_task`].
static WAKER: AtomicWaker = AtomicWaker::new();
/// The driver waits for a received frame.
static RX_WAIT: AtomicBool = AtomicBool::new(false);
/// The driver waits for a free transmit descriptor.
static TX_WAIT: AtomicBool = AtomicBool::new(false);
/// The PHY's link as [`poll_task`] last read it ([`encode_link`]).
static LINK: AtomicU8 = AtomicU8::new(0);
/// [`new`] has run (the statics have one owner).
static TAKEN: AtomicBool = AtomicBool::new(false);

static RX_FRAMES: AtomicU32 = AtomicU32::new(0);
static RX_DROPPED: AtomicU32 = AtomicU32::new(0);
static TX_FRAMES: AtomicU32 = AtomicU32::new(0);
static BUS_ERRORS: AtomicU32 = AtomicU32::new(0);

/// Why the Ethernet did not come up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnetError {
    /// [`new`] was called before.
    AlreadyTaken,
    /// The ENET PLL did not lock.
    PllLock,
    /// The MAC's reset did not complete.
    MacReset,
    /// An MDIO transfer did not complete.
    Mdio,
    /// The PHY's identity check failed.
    Phy(PhyError),
    /// The PHY's software reset did not complete.
    PhyReset,
    /// The rings could not be set up.
    Ring(desc::RingError),
}

/// Frame counters since boot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Frames handed to the stack.
    pub rx_frames: u32,
    /// Received frames dropped for a descriptor error.
    pub rx_dropped: u32,
    /// Frames given to the DMA to send.
    pub tx_frames: u32,
    /// DMA bus errors seen (EIR.EBERR).
    pub bus_errors: u32,
}

/// The frame counters.
pub fn stats() -> Stats {
    Stats {
        rx_frames: RX_FRAMES.load(Relaxed),
        rx_dropped: RX_DROPPED.load(Relaxed),
        tx_frames: TX_FRAMES.load(Relaxed),
        bus_errors: BUS_ERRORS.load(Relaxed),
    }
}

fn encode_link(link: Option<Link>) -> u8 {
    match link {
        None => 0,
        Some(Link { speed: Speed::Mbps10, duplex: Duplex::Half }) => 1,
        Some(Link { speed: Speed::Mbps100, duplex: Duplex::Half }) => 2,
        Some(Link { speed: Speed::Mbps10, duplex: Duplex::Full }) => 3,
        Some(Link { speed: Speed::Mbps100, duplex: Duplex::Full }) => 4,
    }
}

fn decode_link(v: u8) -> Option<Link> {
    let (speed, duplex) = match v {
        1 => (Speed::Mbps10, Duplex::Half),
        2 => (Speed::Mbps100, Duplex::Half),
        3 => (Speed::Mbps10, Duplex::Full),
        4 => (Speed::Mbps100, Duplex::Full),
        _ => return None,
    };
    Some(Link { speed, duplex })
}

fn rx_buffer(i: usize) -> &'static [AtomicU32] {
    RX_BUF.get(i).map_or(&[], |b| &b.0[..])
}

fn tx_buffer(i: usize) -> &'static [AtomicU32] {
    TX_BUF.get(i).map_or(&[], |b| &b.0[..])
}

fn rx_addrs() -> [u32; NRX] {
    core::array::from_fn(|i| rx_buffer(i).as_ptr() as u32)
}

fn tx_addrs() -> [u32; NTX] {
    core::array::from_fn(|i| tx_buffer(i).as_ptr() as u32)
}

/// One MDIO transfer of the frame `mmfr`; the data MMFR holds after it.
fn mdio_transfer(mmfr: u32) -> Option<u16> {
    let e = pac::ENET;
    e.eir().write_value(pac::enet::regs::Eir(mdio::EIR_MII));
    e.mmfr().write_value(pac::enet::regs::Mmfr(mmfr));
    for _ in 0..mdio::POLL_BOUND {
        if e.eir().read().0 & mdio::EIR_MII != 0 {
            e.eir().write_value(pac::enet::regs::Eir(mdio::EIR_MII));
            return Some(mdio::read_data(e.mmfr().read().0));
        }
    }
    None
}

fn mdio_read(reg: u8) -> Option<u16> {
    mdio_transfer(mdio::read_frame(ksz8081::PHY_ADDR, reg)?)
}

fn mdio_write(reg: u8, data: u16) -> Option<()> {
    mdio_transfer(mdio::write_frame(ksz8081::PHY_ADDR, reg, data)?).map(|_| ())
}

/// Clock gates (before any access to the blocks), the ENET PLL at 50 MHz
/// and the reference clock driven out.
fn clocks() -> Result<(), EnetError> {
    // IOMUXC (CCGR4 CG1) and IOMUXC_GPR (CG2): normally on from boot.
    pac::CCM.ccgr4().modify(|w| w.0 |= (3 << 2) | (3 << 4));
    pac::CCM.ccgr1().modify(|w| w.0 |= clock::CCGR1_ENET | clock::CCGR1_GPIO1);
    let pll = clock::pll_enet_50mhz(pac::CCM_ANALOG.pll_enet().read().0);
    pac::CCM_ANALOG.pll_enet().write_value(pac::ccm_analog::regs::PllEnet(pll));
    let mut locked = false;
    for _ in 0..clock::PLL_LOCK_POLL_BOUND {
        if clock::pll_enet_locked(pac::CCM_ANALOG.pll_enet().read().0) {
            locked = true;
            break;
        }
    }
    if !locked {
        return Err(EnetError::PllLock);
    }
    pac::IOMUXC_GPR.gpr1().modify(|w| w.0 = clock::gpr1_rmii_ref_out(w.0));
    Ok(())
}

/// The RMII, MDIO and PHY control pads, as the probe set them (NXP SDK
/// `evkbimxrt1050` `driver_examples/enet`).
fn pads() {
    /// RMII data, enable, error and MDC: ALT3/ALT4, 0xB0E9.
    const PAD_ENET: u32 = 0xB0E9;
    macro_rules! pad {
        ($m:ident, $c:ident, $alt:expr, $cfg:expr) => {
            pac::IOMUXC.$m().write_value(pac::iomuxc::regs::MuxCtl($alt));
            pac::IOMUXC.$c().write_value(pac::iomuxc::regs::Ctl($cfg));
        };
    }
    pad!(sw_mux_ctl_pad_gpio_b1_04, sw_pad_ctl_pad_gpio_b1_04, 3, PAD_ENET); // RX_DATA0
    pad!(sw_mux_ctl_pad_gpio_b1_05, sw_pad_ctl_pad_gpio_b1_05, 3, PAD_ENET); // RX_DATA1
    pad!(sw_mux_ctl_pad_gpio_b1_06, sw_pad_ctl_pad_gpio_b1_06, 3, PAD_ENET); // RX_EN
    pad!(sw_mux_ctl_pad_gpio_b1_07, sw_pad_ctl_pad_gpio_b1_07, 3, PAD_ENET); // TX_DATA0
    pad!(sw_mux_ctl_pad_gpio_b1_08, sw_pad_ctl_pad_gpio_b1_08, 3, PAD_ENET); // TX_DATA1
    pad!(sw_mux_ctl_pad_gpio_b1_09, sw_pad_ctl_pad_gpio_b1_09, 3, PAD_ENET); // TX_EN
    pad!(sw_mux_ctl_pad_gpio_b1_10, sw_pad_ctl_pad_gpio_b1_10, 6 | 0x10, 0x31); // REF_CLK, SION
    pad!(sw_mux_ctl_pad_gpio_b1_11, sw_pad_ctl_pad_gpio_b1_11, 3, PAD_ENET); // RX_ER
    pad!(sw_mux_ctl_pad_gpio_emc_40, sw_pad_ctl_pad_gpio_emc_40, 4, PAD_ENET); // MDC
    pad!(sw_mux_ctl_pad_gpio_emc_41, sw_pad_ctl_pad_gpio_emc_41, 4, 0xB829); // MDIO
    pad!(sw_mux_ctl_pad_gpio_ad_b0_09, sw_pad_ctl_pad_gpio_ad_b0_09, 5, 0xB0A9); // PHY reset
    pad!(sw_mux_ctl_pad_gpio_ad_b0_10, sw_pad_ctl_pad_gpio_ad_b0_10, 5, 0xB0A9); // PHY int
    let m = pac::IOMUXC;
    m.enet_ipg_clk_rmii_select_input()
        .write_value(pac::iomuxc::regs::EnetIpgClkRmiiSelectInput(1));
    m.enet_mdio_select_input().write_value(pac::iomuxc::regs::EnetMdioSelectInput(1));
    m.enet0_rxdata_select_input().write_value(pac::iomuxc::regs::Enet0RxdataSelectInput(1));
    m.enet1_rxdata_select_input().write_value(pac::iomuxc::regs::Enet1RxdataSelectInput(1));
    m.enet_rxen_select_input().write_value(pac::iomuxc::regs::EnetRxenSelectInput(1));
    m.enet_rxerr_select_input().write_value(pac::iomuxc::regs::EnetRxerrSelectInput(1));
}

/// The PHY's hardware reset: GPIO1_IO09 low, high, settle; GPIO1_IO10 (the
/// PHY's interrupt, unused) an input.
async fn phy_hw_reset() {
    let g = pac::GPIO1;
    g.gdir().modify(|w| w.0 = (w.0 | (1 << 9)) & !(1 << 10));
    g.dr().modify(|w| w.0 &= !(1 << 9));
    Timer::after_millis(u64::from(ksz8081::HW_RESET_LOW_MS)).await;
    g.dr().modify(|w| w.0 |= 1 << 9);
    Timer::after_millis(u64::from(ksz8081::HW_RESET_SETTLE_MS)).await;
}

/// The MAC's reset and the MDC clock.
fn mac_reset() -> Result<(), EnetError> {
    let e = pac::ENET;
    e.ecr().write_value(pac::enet::regs::Ecr(mac::ECR_RESET));
    let mut done = false;
    for _ in 0..mac::RESET_POLL_BOUND {
        if e.ecr().read().0 & mac::ECR_RESET == 0 {
            done = true;
            break;
        }
    }
    if !done {
        return Err(EnetError::MacReset);
    }
    // The slowest MDC (IPG / 128): below 2.5 MHz whatever the IPG clock.
    e.mscr().write_value(pac::enet::regs::Mscr(mdio::MSCR_PROBE));
    Ok(())
}

/// The PHY over MDIO: identity, software reset, 50 MHz RMII clock mode,
/// auto-negotiation started.
async fn phy_init() -> Result<(), EnetError> {
    let id1 = mdio_read(ksz8081::REG_PHY_ID1).ok_or(EnetError::Mdio)?;
    let id2 = mdio_read(ksz8081::REG_PHY_ID2).ok_or(EnetError::Mdio)?;
    ksz8081::identify(id1, id2).map_err(EnetError::Phy)?;
    mdio_write(ksz8081::REG_BMCR, ksz8081::RESET_COMMAND).ok_or(EnetError::Mdio)?;
    let mut reads = 0u32;
    loop {
        Timer::after_millis(1).await;
        reads = reads.saturating_add(1);
        match ksz8081::reset_step(mdio_read(ksz8081::REG_BMCR), reads) {
            ResetStep::Done => break,
            ResetStep::Poll => {}
            ResetStep::Failed => return Err(EnetError::PhyReset),
        }
    }
    let c2 = mdio_read(ksz8081::REG_PHY_CTRL2).ok_or(EnetError::Mdio)?;
    mdio_write(ksz8081::REG_PHY_CTRL2, ksz8081::ctrl2_rmii_50mhz(c2)).ok_or(EnetError::Mdio)?;
    mdio_write(ksz8081::REG_BMCR, ksz8081::AUTONEG_START).ok_or(EnetError::Mdio)?;
    Ok(())
}

/// Programs RCR and TCR for `link` and starts the MAC on freshly
/// initialised rings. The MAC must be disabled (ECR.ETHEREN clear).
fn mac_start(rx: &mut RxRing<'static>, tx: &mut TxRing<'static>, link: Link) -> Result<(), EnetError> {
    rx.init(&rx_addrs()).map_err(EnetError::Ring)?;
    tx.init(&tx_addrs()).map_err(EnetError::Ring)?;
    let e = pac::ENET;
    e.rcr().write_value(pac::enet::regs::Rcr(mac::rcr(link, PROMISCUOUS)));
    e.tcr().write_value(pac::enet::regs::Tcr(mac::tcr(link)));
    e.ecr().write_value(pac::enet::regs::Ecr(mac::ECR_ENABLE));
    e.rdar().write_value(pac::enet::regs::Rdar(mac::DESCRIPTOR_ACTIVE));
    Ok(())
}

/// The ENET MAC as an embassy-net driver.
pub struct EnetDriver {
    rx: RxRing<'static>,
    tx: TxRing<'static>,
    rx_frame: [u8; FRAME],
    tx_frame: [u8; FRAME],
    mac: [u8; 6],
    /// The link RCR and TCR are programmed for.
    programmed: Link,
}

/// The right to run [`poll_task`] (one per [`EnetDriver`]).
pub struct EnetPoller(());

/// Brings up the ENET and the PHY with station address `mac_addr`
/// ([`imxrt_base::enet::mac_from_uid`] of the chip's unique id). Takes
/// about 110 ms (the PHY's reset). The link comes up later, after
/// auto-negotiation, as [`poll_task`] reads it.
pub async fn new(mac_addr: [u8; 6]) -> Result<(EnetDriver, EnetPoller), EnetError> {
    if TAKEN.swap(true, Relaxed) {
        return Err(EnetError::AlreadyTaken);
    }
    clocks()?;
    pads();
    phy_hw_reset().await;
    mac_reset()?;
    phy_init().await?;

    let mut rx = RxRing::new(&RX_DESC.0).ok_or(EnetError::Ring(desc::RingError::BadRingMemory))?;
    let mut tx = TxRing::new(&TX_DESC.0).ok_or(EnetError::Ring(desc::RingError::BadRingMemory))?;
    let rdsr = RX_DESC.0.as_ptr() as u32;
    let tdsr = TX_DESC.0.as_ptr() as u32;
    if !desc::ring_base_ok(rdsr) || !desc::ring_base_ok(tdsr) {
        return Err(EnetError::Ring(desc::RingError::BufferAlignment));
    }
    let e = pac::ENET;
    e.eimr().write_value(pac::enet::regs::Eimr(0));
    e.eir().write_value(pac::enet::regs::Eir(mac::EIR_ALL));
    e.iaur().write_value(pac::enet::regs::Iaur(0));
    e.ialr().write_value(pac::enet::regs::Ialr(0));
    e.gaur().write_value(pac::enet::regs::Gaur(0));
    e.galr().write_value(pac::enet::regs::Galr(0));
    let (palr, paur) = mac::station_address(mac_addr);
    e.palr().write_value(pac::enet::regs::Palr(palr));
    e.paur().write_value(pac::enet::regs::Paur(paur));
    e.tfwr().write_value(pac::enet::regs::Tfwr(mac::TFWR_STORE_AND_FORWARD));
    e.mrbr().write_value(pac::enet::regs::Mrbr(mac::MRBR));
    e.rdsr().write_value(pac::enet::regs::Rdsr(rdsr));
    e.tdsr().write_value(pac::enet::regs::Tdsr(tdsr));
    let programmed = Link::FAST_FULL;
    mac_start(&mut rx, &mut tx, programmed)?;

    let driver = EnetDriver {
        rx,
        tx,
        rx_frame: [0; FRAME],
        tx_frame: [0; FRAME],
        mac: mac_addr,
        programmed,
    };
    Ok((driver, EnetPoller(())))
}

impl EnetDriver {
    /// Drops received frames with errors at the cursor (bounded by the
    /// ring); whether a good frame is now waiting.
    fn good_frame_waiting(&mut self) -> bool {
        for _ in 0..NRX {
            match self.rx.peek() {
                None => return false,
                Some(f) if f.good_len().is_some() => return true,
                Some(_) => {
                    let _ = self.rx.release();
                    RX_DROPPED.fetch_add(1, Relaxed);
                    pac::ENET.rdar().write_value(pac::enet::regs::Rdar(mac::DESCRIPTOR_ACTIVE));
                }
            }
        }
        matches!(self.rx.peek(), Some(f) if f.good_len().is_some())
    }

    /// Reprograms the MAC for the link the PHY came up with, if it is not
    /// the one RCR and TCR hold (frames in the rings are lost).
    fn follow_link(&mut self, link: Link) {
        if link == self.programmed {
            return;
        }
        pac::ENET.ecr().write_value(pac::enet::regs::Ecr(mac::ECR_DBSWP));
        if mac_start(&mut self.rx, &mut self.tx, link).is_ok() {
            self.programmed = link;
        }
    }
}

/// A received frame, copied out of its DMA buffer.
pub struct EnetRxToken<'a> {
    ring: &'a mut RxRing<'static>,
    frame: &'a mut [u8; FRAME],
}

/// A frame to send.
pub struct EnetTxToken<'a> {
    ring: &'a mut TxRing<'static>,
    frame: &'a mut [u8; FRAME],
}

impl embassy_net_driver::RxToken for EnetRxToken<'_> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let n = self
            .ring
            .peek()
            .and_then(|fr| fr.good_len().map(|len| desc::copy_frame(rx_buffer(fr.index), len, self.frame)))
            .unwrap_or(0);
        if self.ring.release().is_ok() {
            RX_FRAMES.fetch_add(1, Relaxed);
        }
        pac::ENET.rdar().write_value(pac::enet::regs::Rdar(mac::DESCRIPTOR_ACTIVE));
        match self.frame.get_mut(..n) {
            Some(data) => f(data),
            None => f(&mut []),
        }
    }
}

impl embassy_net_driver::TxToken for EnetTxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let len = len.min(desc::MAX_TX_FRAME);
        let send = desc::padded_len(len);
        if let Some(pad) = self.frame.get_mut(len..send) {
            pad.fill(0);
        }
        let r = match self.frame.get_mut(..len) {
            Some(data) => f(data),
            None => f(&mut []),
        };
        if let (Some(i), Some(data)) = (self.ring.free(), self.frame.get(..send))
            && desc::fill_frame(tx_buffer(i), data).is_ok()
            && self.ring.submit(send).is_ok()
        {
            TX_FRAMES.fetch_add(1, Relaxed);
            pac::ENET.tdar().write_value(pac::enet::regs::Tdar(mac::DESCRIPTOR_ACTIVE));
        }
        r
    }
}

impl embassy_net_driver::Driver for EnetDriver {
    type RxToken<'a> = EnetRxToken<'a>;
    type TxToken<'a> = EnetTxToken<'a>;

    fn receive(&mut self, cx: &mut Context) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        WAKER.register(cx.waker());
        if !self.good_frame_waiting() {
            RX_WAIT.store(true, Relaxed);
            return None;
        }
        if self.tx.free().is_none() {
            TX_WAIT.store(true, Relaxed);
            return None;
        }
        Some((
            EnetRxToken { ring: &mut self.rx, frame: &mut self.rx_frame },
            EnetTxToken { ring: &mut self.tx, frame: &mut self.tx_frame },
        ))
    }

    fn transmit(&mut self, cx: &mut Context) -> Option<Self::TxToken<'_>> {
        WAKER.register(cx.waker());
        if self.tx.free().is_none() {
            TX_WAIT.store(true, Relaxed);
            return None;
        }
        Some(EnetTxToken { ring: &mut self.tx, frame: &mut self.tx_frame })
    }

    fn link_state(&mut self, cx: &mut Context) -> LinkState {
        WAKER.register(cx.waker());
        match decode_link(LINK.load(Relaxed)) {
            Some(link) => {
                self.follow_link(link);
                if link == self.programmed { LinkState::Up } else { LinkState::Down }
            }
            None => LinkState::Down,
        }
    }

    fn capabilities(&self) -> Capabilities {
        let mut caps = Capabilities::default();
        // Ethernet medium: the MTU includes the 14-byte header.
        caps.max_transmission_unit = desc::MAX_TX_FRAME;
        caps.max_burst_size = Some(NTX);
        caps
    }

    fn hardware_address(&self) -> HardwareAddress {
        HardwareAddress::Ethernet(self.mac)
    }
}

fn rx_frame_waiting() -> bool {
    RX_DESC.0.iter().step_by(desc::DESC_WORDS).any(|w| w.load(Relaxed) & desc::RX_E == 0)
}

fn tx_descriptor_free() -> bool {
    TX_DESC.0.iter().step_by(desc::DESC_WORDS).any(|w| w.load(Relaxed) & desc::TX_R == 0)
}

/// The link from BMSR (read twice: its link bit latches low) and PHY
/// control 1; `None` while down, negotiating or unreadable.
fn read_link() -> Option<Link> {
    let _ = mdio_read(ksz8081::REG_BMSR)?;
    let bmsr = mdio_read(ksz8081::REG_BMSR)?;
    let ctrl1 = mdio_read(ksz8081::REG_PHY_CTRL1)?;
    match ksz8081::link_state(bmsr, ctrl1) {
        ksz8081::LinkState::Up(link) => Some(link),
        _ => None,
    }
}

/// Wakes the driver when the hardware has something for it: every
/// [`POLL_INTERVAL`] the descriptors (a received frame, a freed transmit
/// descriptor, a DMA bus error counted), every [`LINK_INTERVAL`] the PHY's
/// link over MDIO. Spawn it once, next to the embassy-net runner.
pub async fn poll_task(_poller: EnetPoller) -> ! {
    let mut next_link = Instant::now();
    loop {
        if RX_WAIT.load(Relaxed) && rx_frame_waiting() {
            RX_WAIT.store(false, Relaxed);
            WAKER.wake();
        }
        if TX_WAIT.load(Relaxed) && tx_descriptor_free() {
            TX_WAIT.store(false, Relaxed);
            WAKER.wake();
        }
        let eir = pac::ENET.eir().read().0;
        if eir & mac::EIR_EBERR != 0 {
            pac::ENET.eir().write_value(pac::enet::regs::Eir(mac::EIR_EBERR));
            BUS_ERRORS.fetch_add(1, Relaxed);
        }
        if Instant::now() >= next_link {
            next_link = Instant::now() + LINK_INTERVAL;
            let link = encode_link(read_link());
            if LINK.swap(link, Relaxed) != link {
                WAKER.wake();
            }
        }
        Timer::after(POLL_INTERVAL).await;
    }
}
