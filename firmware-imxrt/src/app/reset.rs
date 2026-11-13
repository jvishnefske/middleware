//! Chip reset and the trial watchdog.
//!
//! **Reset** asserts WDOG1's software reset
//! (`imxrt_base::ab::watchdog::WDOG1_SOFTWARE_RESET`): the reset that
//! brings the board back through the boot ROM into the bootloader on this
//! EVKB (`SCB::sys_reset` is the fallback). WDOG1's clock gate (CCGR3 CG8)
//! is enabled first: an access to a gated block stalls the bus.
//!
//! **Trial watchdog** (RTWDOG, RT15, never fed): [`arm_trial`] arms it
//! first thing on a trial boot, [`release`] stops it once `sys/fw/confirm`
//! succeeded (a no-op when it is not armed). The firmware arms it rather
//! than the bootloader: imxrt-rt's `__pre_init`, which runs in the
//! firmware's startup before `main`, disables the RTWDOG unconditionally,
//! so a bootloader-armed watchdog would not survive into the firmware.

use embassy_nxp::pac;
use imxrt_base::ab::watchdog;

/// Resets the chip into the bootloader. Never returns.
pub fn reset() -> ! {
    pac::CCM.ccgr3().modify(|w| w.set_cg8(0b11));
    pac::WDOG1.wcr().write_value(pac::wdog::regs::Wcr(watchdog::WDOG1_SOFTWARE_RESET));
    // The reset takes effect within a few cycles; should it not, the
    // core's own reset request.
    cortex_m::asm::delay(10_000);
    cortex_m::peripheral::SCB::sys_reset()
}

/// Whether the bootloader armed the trial watchdog. RTWDOG (WDOG3) is
/// clocked by CCGR5 CG2; with the gate off it was never armed (and must
/// not be read).
pub fn armed() -> bool {
    pac::CCM.ccgr5().read().cg2() != 0 && watchdog::enabled(pac::RTWDOG.cs().read().0)
}

/// Arms the trial watchdog: [`watchdog::TRIAL_SECONDS`] until it resets
/// the chip, unless [`release`] runs first. Unlock, then TOVAL and CS
/// inside the unlock window (interrupts off).
pub fn arm_trial() {
    pac::CCM.ccgr5().modify(|w| w.set_cg2(0b11));
    let toval = watchdog::trial_toval(watchdog::TRIAL_SECONDS);
    cortex_m::interrupt::free(|_| {
        pac::RTWDOG.cnt().write_value(pac::rtwdog::regs::Cnt(watchdog::RTWDOG_UNLOCK));
        pac::RTWDOG.toval().write_value(pac::rtwdog::regs::Toval(toval));
        pac::RTWDOG.cs().write_value(pac::rtwdog::regs::Cs(watchdog::trial_cs()));
    });
}

/// Stops the trial watchdog, if armed: unlock, then rewrite CS with EN
/// clear inside the unlock window.
pub fn release() {
    if armed() {
        let cs = pac::RTWDOG.cs().read().0;
        cortex_m::interrupt::free(|_| {
            pac::RTWDOG.cnt().write_value(pac::rtwdog::regs::Cnt(watchdog::RTWDOG_UNLOCK));
            pac::RTWDOG.cs().write_value(pac::rtwdog::regs::Cs(watchdog::released_cs(cs)));
        });
    }
}
