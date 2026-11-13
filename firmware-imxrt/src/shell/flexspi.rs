//! The EVKB's HyperFlash (S26KS512S) on FlexSPI A, as an `embedded-storage`
//! `NorFlash`.
//!
//! [`HyperFlash::new`] brings the controller up the way NXP's SDK example
//! for this board does (`driver_examples/flexspi/hyper_flash`): FlexSPI
//! root clock from PLL3 PFD0 (480 × 18 / 26 = 332 MHz) / 4 = 83 MHz (DDR,
//! 42 MHz on the bus), DQS read sampling, ports A and B combined into eight
//! data lines, word addressed, the DLL overridden for that clock. Then it
//! loads [`crate::hyperflash::s26ks512s::LUT`], and every operation runs a
//! [`crate::hyperflash::plan`] as IP commands: reads in 128-byte pieces,
//! programs in write-buffer chunks with the unlock cycles, erases one
//! 256 KiB sector at a time, each wait for the status register bounded by
//! the plan's [`crate::hyperflash::status::PollBound`].
//!
//! Three orderings matter (each found on the board):
//!
//! * a FlexSPI left enabled by the boot ROM or a debugger is waited idle
//!   and disabled (MCR0.MDIS) **before** its clock gate and divider change:
//!   gating the clock of a busy controller stalls the bus;
//! * the LUT is written with the module **enabled** (writes while MDIS is
//!   set are ignored), after clearing all 64 words (leftover words of an
//!   earlier user would run as part of the sequences);
//! * an IP write is triggered first, then the TX FIFO is filled each time
//!   it reports room (INTR.IPTXWE), as the SDK does.
//!
//! No code may execute from the HyperFlash while it programs or erases:
//! the bootloader runs from ITCM and the firmware from SDRAM.

use crate::hyperflash::plan::{self, IpCommand, Step, Transfer};
use crate::hyperflash::s26ks512s::LUT;
use crate::hyperflash::status::{Status, WaitOutcome, wait_step};
use crate::hyperflash::{FLASH_SIZE, SECTOR_SIZE};
use embassy_nxp::pac;
use embassy_time::{Duration, block_for};
use embedded_storage::nor_flash::{
    ErrorType, NorFlash, NorFlashError, NorFlashErrorKind, ReadNorFlash,
};

/// Bounded polls of a FlexSPI status flag (each a register read).
const FLAG_POLLS: u32 = 2_000_000;

/// Why a HyperFlash operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashError {
    /// The controller did not become idle, or a flag did not come.
    Timeout,
    /// FlexSPI reported an IP command error (STS1).
    Command(u32),
    /// The flash's status register reported a program or erase error.
    Failed,
    /// The flash stayed busy past the plan's poll bound.
    Busy,
    /// The LUT did not take (left locked, or not written).
    Lut,
    /// Out of range or misaligned for the plan.
    Plan,
}

impl NorFlashError for FlashError {
    fn kind(&self) -> NorFlashErrorKind {
        match self {
            FlashError::Plan => NorFlashErrorKind::OutOfBounds,
            _ => NorFlashErrorKind::Other,
        }
    }
}

/// The HyperFlash. Owns FlexSPI (embassy-nxp names the RT1062's first instance `FLEXSPI1`; the RT1052 has only that one).
pub struct HyperFlash {
    _flexspi: embassy_nxp::Peri<'static, embassy_nxp::peripherals::FLEXSPI1>,
}

fn wait_idle() -> Result<(), FlashError> {
    for _ in 0..FLAG_POLLS {
        if pac::FLEXSPI.sts0().read().0 & 0b11 == 0b11 {
            return Ok(());
        }
    }
    Err(FlashError::Timeout)
}

/// Waits for INTR bit `bit`, failing on an IP command error.
fn flag(bit: u32) -> Result<(), FlashError> {
    let f = pac::FLEXSPI;
    for _ in 0..FLAG_POLLS {
        let i = f.intr().read().0;
        if i & (1 << 3) != 0 {
            return Err(FlashError::Command(f.sts1().read().0));
        }
        if i & (1 << bit) != 0 {
            return Ok(());
        }
    }
    Err(FlashError::Timeout)
}

macro_rules! pad {
    ($mux:ident, $ctl:ident) => {
        pac::IOMUXC
            .$mux()
            .write_value(pac::iomuxc::regs::MuxCtl(0x11)); // ALT1, SION
        pac::IOMUXC
            .$ctl()
            .write_value(pac::iomuxc::regs::Ctl(0x10F1));
    };
}

impl HyperFlash {
    /// Brings FlexSPI up for the HyperFlash and loads the driver's LUT.
    pub fn new(
        flexspi: embassy_nxp::Peri<'static, embassy_nxp::peripherals::FLEXSPI1>,
    ) -> Result<HyperFlash, FlashError> {
        let f = pac::FLEXSPI;
        if pac::CCM.ccgr6().read().0 & (0b11 << 10) != 0 {
            wait_idle()?;
            f.mcr0().modify(|w| w.set_mdis(true));
        }
        pac::CCM.ccgr6().modify(|w| w.set_cg5(0));
        pac::CCM_ANALOG.pfd_480().modify(|w| {
            w.set_pfd0_frac(26);
            w.set_pfd0_clkgate(false);
        });
        let cscmr1 = pac::CCM.cscmr1().read().0;
        let cscmr1 = (cscmr1 & !((0b11 << 29) | (0b111 << 23))) | (3 << 29) | (3 << 23);
        pac::CCM.cscmr1().write_value(pac::ccm::regs::Cscmr1(cscmr1));
        pac::CCM.ccgr6().modify(|w| w.set_cg5(3));

        pad!(sw_mux_ctl_pad_gpio_sd_b1_00, sw_pad_ctl_pad_gpio_sd_b1_00);
        pad!(sw_mux_ctl_pad_gpio_sd_b1_01, sw_pad_ctl_pad_gpio_sd_b1_01);
        pad!(sw_mux_ctl_pad_gpio_sd_b1_02, sw_pad_ctl_pad_gpio_sd_b1_02);
        pad!(sw_mux_ctl_pad_gpio_sd_b1_03, sw_pad_ctl_pad_gpio_sd_b1_03);
        pad!(sw_mux_ctl_pad_gpio_sd_b1_04, sw_pad_ctl_pad_gpio_sd_b1_04);
        pad!(sw_mux_ctl_pad_gpio_sd_b1_05, sw_pad_ctl_pad_gpio_sd_b1_05);
        pad!(sw_mux_ctl_pad_gpio_sd_b1_06, sw_pad_ctl_pad_gpio_sd_b1_06);
        pad!(sw_mux_ctl_pad_gpio_sd_b1_07, sw_pad_ctl_pad_gpio_sd_b1_07);
        pad!(sw_mux_ctl_pad_gpio_sd_b1_08, sw_pad_ctl_pad_gpio_sd_b1_08);
        pad!(sw_mux_ctl_pad_gpio_sd_b1_09, sw_pad_ctl_pad_gpio_sd_b1_09);
        pad!(sw_mux_ctl_pad_gpio_sd_b1_10, sw_pad_ctl_pad_gpio_sd_b1_10);
        pad!(sw_mux_ctl_pad_gpio_sd_b1_11, sw_pad_ctl_pad_gpio_sd_b1_11);

        // Software reset, then configure with the module disabled.
        f.mcr0().modify(|w| w.set_mdis(false));
        f.mcr0().modify(|w| w.set_swreset(true));
        let mut reset_done = false;
        for _ in 0..FLAG_POLLS {
            if !f.mcr0().read().swreset() {
                reset_done = true;
                break;
            }
        }
        if !reset_done {
            return Err(FlashError::Timeout);
        }
        f.mcr0().modify(|w| w.set_mdis(true));
        // MCR0: RXCLKSRC = 3 (DQS from the flash), COMBINATIONEN, grant waits.
        f.mcr0()
            .write_value(pac::flex_spi::regs::Mcr0(0xFFFF_0000 | (1 << 13) | (3 << 4) | (1 << 1)));
        // MCR2: SCKBDIFFOPT (SCLK B is the differential clock), CLRAHBBUFOPT 0.
        let mcr2 = f.mcr2().read().0;
        f.mcr2()
            .write_value(pac::flex_spi::regs::Mcr2((mcr2 & !(1 << 11)) | (1 << 19)));
        // AHB: prefetch, bufferable, cacheable.
        f.ahbcr()
            .write_value(pac::flex_spi::regs::Ahbcr((1 << 5) | (1 << 4) | (1 << 3)));
        f.flsha1cr0()
            .write_value(pac::flex_spi::regs::Flsha1cr0(FLASH_SIZE / 1024));
        // FLSHCR1A1: TCSS 3, TCSH 0, WA, CAS 3, CSINTERVAL 2.
        f.flshcr1(0)
            .write_value(pac::flex_spi::regs::Flshcr1(3 | (1 << 10) | (3 << 11) | (2 << 16)));
        // FLSHCR2A1: AHB read sequence 0, AHB write sequence 1, AWRWAIT 20.
        f.flshcr2(0)
            .write_value(pac::flex_spi::regs::Flshcr2((1 << 8) | (20 << 16)));
        // DLLACR: override, OVRDVAL 14 (1 ns data valid / 75 ps, rounded up).
        f.dllcr(0)
            .write_value(pac::flex_spi::regs::Dllcr((1 << 8) | (14 << 9)));
        f.mcr0().modify(|w| w.set_mdis(false));
        wait_idle()?;

        // The LUT, with the module enabled.
        f.lutkey()
            .write_value(pac::flex_spi::regs::Lutkey(0x5AF0_5AF0));
        f.lutcr().write_value(pac::flex_spi::regs::Lutcr(2));
        for (i, w) in LUT.words().iter().enumerate() {
            f.lut(i).write_value(pac::flex_spi::regs::Lut(*w));
        }
        f.lutkey()
            .write_value(pac::flex_spi::regs::Lutkey(0x5AF0_5AF0));
        f.lutcr().write_value(pac::flex_spi::regs::Lutcr(1));
        let words = LUT.words();
        if f.lut(4).read().0 != words[4] || f.lutcr().read().0 & 1 == 0 {
            return Err(FlashError::Lut);
        }
        Ok(HyperFlash { _flexspi: flexspi })
    }

    /// Issues one IP command. `data` is the caller's buffer for
    /// `WriteData`; `rx` receives `Read` bytes.
    fn issue(&mut self, c: &IpCommand, data: &[u8], rx: &mut [u8]) -> Result<(), FlashError> {
        let f = pac::FLEXSPI;
        wait_idle()?;
        f.iptxfcr().write_value(pac::flex_spi::regs::Iptxfcr(1)); // clear, watermark 8 B
        f.iprxfcr().write_value(pac::flex_spi::regs::Iprxfcr(1)); // clear, watermark 8 B
        // Done and error flags only: writing IPTXWE / IPRXWA pushes / pops.
        f.intr().write_value(pac::flex_spi::regs::Intr(0x1F));
        let len: u32 = match c.transfer {
            Transfer::None => 0,
            Transfer::Read { len } => len.into(),
            Transfer::WriteData { len, .. } => len.into(),
            Transfer::WriteWord(_) => 2,
        };
        let s = c.sequence;
        f.ipcr0().write_value(pac::flex_spi::regs::Ipcr0(c.address));
        f.ipcr1().write_value(pac::flex_spi::regs::Ipcr1(
            len | (u32::from(s.first().index()) << 16) | (u32::from(s.iseqnum()) << 24),
        ));
        f.ipcmd().write_value(pac::flex_spi::regs::Ipcmd(1));
        match c.transfer {
            Transfer::None => {}
            Transfer::WriteWord(w) => {
                flag(6)?;
                f.tfdr(0)
                    .write_value(pac::flex_spi::regs::Tfdr(u32::from_le_bytes([w[0], w[1], 0, 0])));
                f.tfdr(1).write_value(pac::flex_spi::regs::Tfdr(0));
                f.intr().write_value(pac::flex_spi::regs::Intr(1 << 6));
            }
            Transfer::WriteData { start, len } => {
                let src = data
                    .get(start..start + usize::from(len))
                    .ok_or(FlashError::Plan)?;
                for chunk in src.chunks(8) {
                    flag(6)?;
                    let mut b = [0xFFu8; 8];
                    b[..chunk.len()].copy_from_slice(chunk);
                    f.tfdr(0)
                        .write_value(pac::flex_spi::regs::Tfdr(u32::from_le_bytes([b[0], b[1], b[2], b[3]])));
                    f.tfdr(1)
                        .write_value(pac::flex_spi::regs::Tfdr(u32::from_le_bytes([b[4], b[5], b[6], b[7]])));
                    f.intr().write_value(pac::flex_spi::regs::Intr(1 << 6));
                }
            }
            Transfer::Read { len } => {
                let out = rx.get_mut(..usize::from(len)).ok_or(FlashError::Plan)?;
                for chunk in out.chunks_mut(8) {
                    // A last partial watermark only completes with the command.
                    if chunk.len() == 8 {
                        flag(5)?;
                    } else {
                        flag(0)?;
                    }
                    let w0 = f.rfdr(0).read().0.to_le_bytes();
                    let w1 = f.rfdr(1).read().0.to_le_bytes();
                    let all = [w0[0], w0[1], w0[2], w0[3], w1[0], w1[1], w1[2], w1[3]];
                    let n = chunk.len();
                    chunk.copy_from_slice(&all[..n]);
                    f.intr().write_value(pac::flex_spi::regs::Intr(1 << 5)); // pop
                }
            }
        }
        flag(0)
    }

    /// Runs one plan, `data` being the buffer its `WriteData` steps index.
    fn run(&mut self, steps: &[Step], data: &[u8]) -> Result<(), FlashError> {
        for st in steps {
            match st {
                Step::Issue(c) => self.issue(c, data, &mut [])?,
                Step::WaitReady(bound) => {
                    let mut poll = 0;
                    loop {
                        let mut b = [0u8; 2];
                        self.issue(&plan::read_status(), &[], &mut b)?;
                        match wait_step(*bound, poll, Status::from_rx_bytes(b)) {
                            WaitOutcome::Done => break,
                            WaitOutcome::Poll => {
                                block_for(Duration::from_micros(bound.interval_us().into()));
                                poll += 1;
                            }
                            WaitOutcome::Failed(_) => return Err(FlashError::Failed),
                            WaitOutcome::TimedOut => return Err(FlashError::Busy),
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

impl ErrorType for HyperFlash {
    type Error = FlashError;
}

impl ReadNorFlash for HyperFlash {
    /// HyperBus is word addressed: reads start on, and are whole, words.
    const READ_SIZE: usize = 2;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), FlashError> {
        if !offset.is_multiple_of(2) || !bytes.len().is_multiple_of(2) {
            return Err(FlashError::Plan);
        }
        for (k, chunk) in bytes.chunks_mut(128).enumerate() {
            let at = offset + 128 * k as u32;
            let c = plan::read(at, chunk.len() as u16).map_err(|_| FlashError::Plan)?;
            self.issue(&c, &[], chunk)?;
        }
        Ok(())
    }

    fn capacity(&self) -> usize {
        FLASH_SIZE as usize
    }
}

impl NorFlash for HyperFlash {
    const WRITE_SIZE: usize = 2;
    /// The S26KS512S's uniform sector (confirmed by its CFI on the board).
    const ERASE_SIZE: usize = SECTOR_SIZE as usize;

    fn erase(&mut self, from: u32, to: u32) -> Result<(), FlashError> {
        let sectors = plan::erase_sectors(from, (to - from) as usize).map_err(|_| FlashError::Plan)?;
        for s in sectors {
            self.run(&plan::erase_sector_steps(s), &[])?;
        }
        Ok(())
    }

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), FlashError> {
        for ch in plan::program_chunks(offset, bytes.len()).map_err(|_| FlashError::Plan)? {
            self.run(&ch.steps(), bytes)?;
        }
        Ok(())
    }
}
