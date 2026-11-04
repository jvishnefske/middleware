//! I2C buses 0 and 1 for payloads (ABI 3 `i2c_open` / `i2c_transfer`):
//! controllers I2C0 and I2C1 on the board-mapped pins (`rp_base::chip`:
//! SDA GPIO 4 / SCL GPIO 5, SDA GPIO 6 / SCL GPIO 7).
//!
//! The controllers are driven through embassy-rp's PAC rather than its
//! `I2c` driver, for two reasons: the driver takes its pins by type, which
//! the pin bank (one `Flex` per payload pin, kept for the firmware's life)
//! cannot give it, and its blocking loops wait without a bound on a target
//! that holds SCL low. Every wait here ends at the transfer's deadline
//! (`rp_base::periph::i2c::timeout_us`, embassy-time); every decision
//! (timing, the command sequence, the error) comes from
//! `rp_base::periph::i2c`.
//!
//! * [`I2cBuses::open`] resets the controller, configures it as a
//!   fast-mode controller with 7-bit addresses at the payload's clock, and
//!   enables it; the device routes the pins ([`super::pads`], pull-ups on).
//!   Reopening applies a new clock.
//! * [`I2cBuses::transfer`] sets the target address, then for each command
//!   waits for FIFO room, writes it, and waits for the byte to leave (a
//!   write) or arrive (a read), checking the abort source while it waits;
//!   after the last, for the stop. A fault or the deadline resets and
//!   reconfigures the controller (so a wedged one never carries over to
//!   the next transfer) and returns `Transport`.
//! * [`I2cBuses::close`] (payload stop) holds both controllers in reset.

use embassy_rp::Peri;
use embassy_rp::pac;
use embassy_rp::peripherals::{I2C0, I2C1};
use embassy_time::{Duration, Instant};
use middleware::wasm_abi::AbiError;
use rp_base::periph::i2c::{Cmd, FIFO_DEPTH, Fault, Timing, check_lengths, commands, timeout_us};

/// Longest wait for a controller to leave reset.
const RESET_WAIT: Duration = Duration::from_millis(1);

/// The two I2C controllers and the clock each was opened at.
pub struct I2cBuses {
    _i2c0: Peri<'static, I2C0>,
    _i2c1: Peri<'static, I2C1>,
    /// The clock of each bus a payload opened (`None`: closed).
    freq: [Option<u32>; 2],
}

/// The registers of `bus`.
fn regs(bus: u8) -> Result<pac::i2c::I2c, AbiError> {
    match bus {
        0 => Ok(pac::I2C0),
        1 => Ok(pac::I2C1),
        _ => Err(AbiError::Unsupported),
    }
}

/// Sets or clears `bus`'s reset.
fn hold_reset(bus: u8, held: bool) {
    pac::RESETS.reset().modify(|w| match bus {
        0 => w.set_i2c0(held),
        _ => w.set_i2c1(held),
    });
}

/// Resets `bus` and waits (bounded) for it to come out of reset.
fn reset(bus: u8) -> Result<(), AbiError> {
    hold_reset(bus, true);
    hold_reset(bus, false);
    let deadline = Instant::now() + RESET_WAIT;
    loop {
        let done = pac::RESETS.reset_done().read();
        if if bus == 0 { done.i2c0() } else { done.i2c1() } {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(AbiError::Unsupported);
        }
    }
}

/// Resets and configures `bus` for `timing`, enabled.
fn configure(bus: u8, timing: &Timing) -> Result<(), AbiError> {
    reset(bus)?;
    let r = regs(bus)?;
    r.ic_enable().write(|w| w.set_enable(false));
    r.ic_con().write(|w| {
        w.set_speed(pac::i2c::vals::Speed::FAST);
        w.set_master_mode(true);
        w.set_ic_slave_disable(true);
        w.set_ic_restart_en(true);
        w.set_tx_empty_ctrl(true);
    });
    r.ic_tx_tl().write(|w| w.set_tx_tl(0));
    r.ic_rx_tl().write(|w| w.set_rx_tl(0));
    r.ic_fs_scl_hcnt().write(|w| w.set_ic_fs_scl_hcnt(timing.hcnt));
    r.ic_fs_scl_lcnt().write(|w| w.set_ic_fs_scl_lcnt(timing.lcnt));
    r.ic_fs_spklen().write(|w| w.set_ic_fs_spklen(timing.spklen));
    r.ic_sda_hold().modify(|w| w.set_ic_sda_tx_hold(timing.sda_hold));
    r.ic_enable().write(|w| w.set_enable(true));
    Ok(())
}

/// Waits until `ready` or the deadline; checks the abort source on every
/// poll. Bounded by `deadline`.
fn wait(r: pac::i2c::I2c, deadline: Instant, ready: impl Fn(pac::i2c::I2c) -> bool) -> Result<(), Fault> {
    loop {
        if let Some(fault) = Fault::from_abort_source(r.ic_tx_abrt_source().read().0) {
            r.ic_clr_tx_abrt().read();
            return Err(fault);
        }
        if ready(r) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Fault::Timeout);
        }
    }
}

/// Runs one transfer on an enabled controller.
fn run(r: pac::i2c::I2c, addr: u8, tx: &[u8], rx: &mut [u8], deadline: Instant) -> Result<(), Fault> {
    r.ic_enable().write(|w| w.set_enable(false));
    r.ic_tar().write(|w| w.set_ic_tar(u16::from(addr)));
    r.ic_enable().write(|w| w.set_enable(true));
    r.ic_clr_tx_abrt().read();
    r.ic_clr_stop_det().read();
    let mut slots = rx.iter_mut();
    for Cmd { read, byte, restart, stop } in commands(tx, slots.len()) {
        wait(r, deadline, |r| r.ic_txflr().read().txflr() < FIFO_DEPTH)?;
        r.ic_data_cmd().write(|w| {
            w.set_restart(restart);
            w.set_stop(stop);
            w.set_cmd(read);
            w.set_dat(byte);
        });
        if read {
            wait(r, deadline, |r| r.ic_rxflr().read().rxflr() > 0)?;
            let value = r.ic_data_cmd().read().dat();
            if let Some(slot) = slots.next() {
                *slot = value;
            }
        } else {
            wait(r, deadline, |r| r.ic_raw_intr_stat().read().tx_empty())?;
        }
    }
    wait(r, deadline, |r| r.ic_raw_intr_stat().read().stop_det())?;
    r.ic_clr_stop_det().read();
    Ok(())
}

impl I2cBuses {
    /// Takes both controllers (held in reset until a payload opens one).
    pub fn new(i2c0: Peri<'static, I2C0>, i2c1: Peri<'static, I2C1>) -> Self {
        hold_reset(0, true);
        hold_reset(1, true);
        I2cBuses { _i2c0: i2c0, _i2c1: i2c1, freq: [None; 2] }
    }

    /// Opens (or retunes) `bus` at `freq_hz`; `Unsupported` for a clock the
    /// controller cannot make.
    pub fn open(&mut self, bus: u8, freq_hz: u32) -> Result<(), AbiError> {
        let slot = self.freq.get_mut(usize::from(bus)).ok_or(AbiError::Unsupported)?;
        let timing = Timing::new(embassy_rp::clocks::clk_peri_freq(), freq_hz)?;
        configure(bus, &timing)?;
        *slot = Some(freq_hz);
        Ok(())
    }

    /// One blocking transfer (see the module docs): `Busy` before
    /// `open`, `Unsupported` for an address-only probe, `Transport` for any
    /// bus fault or the deadline.
    pub fn transfer(&mut self, bus: u8, addr: u8, tx: &[u8], rx: &mut [u8]) -> Result<(), AbiError> {
        let freq = self.freq.get(usize::from(bus)).copied().flatten().ok_or(AbiError::Busy)?;
        check_lengths(tx.len(), rx.len())?;
        let r = regs(bus)?;
        let budget = Duration::from_micros(u64::from(timeout_us(freq, tx.len(), rx.len())));
        match run(r, addr, tx, rx, Instant::now() + budget) {
            Ok(()) => Ok(()),
            Err(fault) => {
                // Start the next transfer from a clean controller.
                if let Ok(timing) = Timing::new(embassy_rp::clocks::clk_peri_freq(), freq) {
                    let _ = configure(bus, &timing);
                }
                Err(fault.abi())
            }
        }
    }

    /// Closes both buses (payload stop): controllers held in reset.
    pub fn close(&mut self) {
        hold_reset(0, true);
        hold_reset(1, true);
        self.freq = [None; 2];
    }
}
