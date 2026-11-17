//! I2C bus 0: LPI2C1 on the Arduino A5 (SCL, GPIO_AD_B1_00) and A4 (SDA,
//! GPIO_AD_B1_01) pins, shared with the FXOS8700CQ and the WM8960 (`wasm`
//! only).
//!
//! The timing and the transfer are [`imxrt_base::periph::i2c`]'s plans:
//! the LPI2C clock root is the 24 MHz oscillator (`LPI2C_CLK_HZ`), each
//! transfer is the plan's `MTDR` command words, every wait a bounded poll
//! ([`i2c::poll`]), so a stuck bus answers `Transport` instead of hanging
//! the controller. A fault clears the flags and the FIFOs and, when the
//! controller still holds the bus, sends a STOP; a controller still busy
//! after that is reset and reprogrammed.
//!
//! The pads are routed to LPI2C1 (ALT3, SION, the input daisy chains, open
//! drain with the 22 kΩ pull-up: the SDK's `0xD8B0`) on [`I2c::open`] and
//! back to GPIO by the device on close.

use embassy_nxp::pac;
use imxrt_base::periph::i2c::{self, Cmd, Fault, Timing, mcr, msr};
use middleware::wasm_abi::AbiError;

/// The SDK's pad configuration for the LPI2C pins.
const PAD_I2C: u32 = 0xD8B0;

/// The open controller's clock.
struct Open {
    timing: Timing,
    freq_hz: u32,
}

/// LPI2C1, closed until a payload opens bus 0.
///
/// The HAL has no LPI2C1 singleton on this chip; the controller is
/// constructed once, in the shell's boot, and only through the PAC.
pub struct I2c {
    open: Option<Open>,
}

impl I2c {
    /// The controller, its clock root set (oscillator, ÷1) and its gate
    /// on, held in reset.
    pub fn new() -> Self {
        // CSCDR2: LPI2C_CLK_SEL (bit 18) = oscillator, LPI2C_CLK_PODF
        // (bits 24:19) = ÷1; the gate (CCGR2 CG3) off while it changes.
        pac::CCM.ccgr2().modify(|w| w.set_cg3(0));
        pac::CCM
            .cscdr2()
            .modify(|w| w.0 = (w.0 & !(0x3F << 19)) | (1 << 18));
        pac::CCM.ccgr2().modify(|w| w.set_cg3(0b11));
        pac::LPI2C1
            .mcr()
            .write_value(pac::lpi2c::regs::Mcr(mcr::RST));
        pac::LPI2C1.mcr().write_value(pac::lpi2c::regs::Mcr(0));
        I2c { open: None }
    }

    fn program(t: &Timing) {
        let r = pac::LPI2C1;
        r.mcr().write_value(pac::lpi2c::regs::Mcr(mcr::RST));
        r.mcr().write_value(pac::lpi2c::regs::Mcr(0));
        r.mcfgr1().write_value(pac::lpi2c::regs::Mcfgr1(t.mcfgr1()));
        r.mccr0().write_value(pac::lpi2c::regs::Mccr0(t.mccr0()));
        r.mcfgr3().write_value(pac::lpi2c::regs::Mcfgr3(t.mcfgr3()));
        r.mcr().write_value(pac::lpi2c::regs::Mcr(mcr::MEN));
    }

    /// Opens bus 0 at `freq_hz` and routes the pads.
    pub fn open(&mut self, freq_hz: u32) -> Result<(), AbiError> {
        let timing = Timing::new(i2c::LPI2C_CLK_HZ, freq_hz)?;
        Self::program(&timing);
        let m = pac::IOMUXC;
        m.lpi2c1_scl_select_input()
            .write_value(pac::iomuxc::regs::Lpi2c1SclSelectInput(1));
        m.lpi2c1_sda_select_input()
            .write_value(pac::iomuxc::regs::Lpi2c1SdaSelectInput(1));
        m.sw_pad_ctl_pad_gpio_ad_b1_00()
            .write_value(pac::iomuxc::regs::Ctl(PAD_I2C));
        m.sw_pad_ctl_pad_gpio_ad_b1_01()
            .write_value(pac::iomuxc::regs::Ctl(PAD_I2C));
        m.sw_mux_ctl_pad_gpio_ad_b1_00()
            .write_value(pac::iomuxc::regs::MuxCtl(0x13)); // ALT3, SION
        m.sw_mux_ctl_pad_gpio_ad_b1_01()
            .write_value(pac::iomuxc::regs::MuxCtl(0x13));
        self.open = Some(Open { timing, freq_hz });
        Ok(())
    }

    /// Closes the bus: controller disabled and in reset (the device returns
    /// the pads to GPIO).
    pub fn close(&mut self) {
        pac::LPI2C1
            .mcr()
            .write_value(pac::lpi2c::regs::Mcr(mcr::RST));
        pac::LPI2C1.mcr().write_value(pac::lpi2c::regs::Mcr(0));
        self.open = None;
    }

    /// Writes `tx`, then reads `rx.len()` bytes, at 7-bit `addr`.
    pub fn transfer(&mut self, addr: u8, tx: &[u8], rx: &mut [u8]) -> Result<(), AbiError> {
        let Some(open) = &self.open else {
            return Err(AbiError::InvalidArgument);
        };
        i2c::check_lengths(tx.len(), rx.len())?;
        let (timing, freq) = (open.timing, open.freq_hz);
        match run(addr, tx, rx, freq) {
            Ok(()) => Ok(()),
            Err(f) => {
                recover(f, &timing);
                Err(f.abi())
            }
        }
    }
}

/// Moves what the receive FIFO holds into `rx` from `got`.
fn drain(rx: &mut [u8], got: &mut usize) {
    while *got < rx.len() {
        match i2c::rx_byte(pac::LPI2C1.mrdr().read().0) {
            Some(b) => {
                if let Some(slot) = rx.get_mut(*got) {
                    *slot = b;
                }
                *got += 1;
            }
            None => break,
        }
    }
}

fn run(addr: u8, tx: &[u8], rx: &mut [u8], freq: u32) -> Result<(), Fault> {
    let r = pac::LPI2C1;
    let mut budget = i2c::budget(freq, tx.len(), rx.len());
    r.msr().write_value(pac::lpi2c::regs::Msr(msr::CLEAR));
    r.mcr()
        .write_value(pac::lpi2c::regs::Mcr(mcr::MEN | mcr::RTF | mcr::RRF));
    let mut got = 0;
    for cmd in i2c::commands(addr, tx, rx.len()) {
        loop {
            drain(rx, &mut got);
            let room = r.mfsr().read().txcount() < i2c::FIFO_DEPTH;
            if i2c::poll(r.msr().read().0, room, &mut budget)? {
                break;
            }
        }
        r.mtdr().write_value(pac::lpi2c::regs::Mtdr(cmd.mtdr()));
        if cmd == Cmd::Stop {
            break;
        }
    }
    loop {
        drain(rx, &mut got);
        let m = r.msr().read().0;
        if i2c::poll(m, m & msr::SDF != 0 && got >= rx.len(), &mut budget)? {
            return Ok(());
        }
    }
}

/// After a fault: flags and FIFOs cleared, a STOP if the controller still
/// holds the bus, a reset if it is still busy after that.
fn recover(f: Fault, timing: &Timing) {
    let r = pac::LPI2C1;
    r.msr().write_value(pac::lpi2c::regs::Msr(msr::CLEAR));
    r.mcr()
        .write_value(pac::lpi2c::regs::Mcr(mcr::MEN | mcr::RTF | mcr::RRF));
    if f.needs_stop(r.msr().read().0) {
        r.mtdr()
            .write_value(pac::lpi2c::regs::Mtdr(Cmd::Stop.mtdr()));
        let mut budget = imxrt_base::periph::Budget::from_us(1_000);
        while r.msr().read().0 & msr::MBF != 0 && budget.spend() {}
    }
    if r.msr().read().0 & msr::MBF != 0 {
        I2c::program(timing);
    }
}
