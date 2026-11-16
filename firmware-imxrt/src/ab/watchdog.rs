//! The trial watchdog and the software reset, as register values (RT15).
//!
//! A trial boot must end in `sys/fw/confirm` or roll back. As on the
//! RP2350 (whose boot ROM arms a watchdog for a try-before-you-buy image
//! the firmware never feeds), the RTWDOG is armed for a trial image
//! ([`trial_cs`], [`trial_toval`]: [`TRIAL_SECONDS`]), never fed, and a
//! successful `sys/fw/confirm` stops it ([`released_cs`]). The firmware
//! arms it at the start of `main` when the bootloader's handoff says
//! "trial": imxrt-rt's `__pre_init` disables the RTWDOG in every image's
//! startup, so a watchdog the bootloader armed would not survive the jump. A trial that hangs, crashes or is simply not
//! confirmed in time is reset by the watchdog, and the bootloader, which
//! counted the attempt, boots the confirmed slot.
//!
//! RTWDOG (RT1050 RM chapter "Watchdog Timer (RTWDOG)"): CS bits are
//! UPDATE 5, EN 7, CLK 9:8, ULK 11, PRES 12 (÷256), CMD32EN 13, FLG 14;
//! with CMD32EN the unlock and refresh are single 32-bit writes to CNT
//! ([`RTWDOG_UNLOCK`], [`RTWDOG_REFRESH`]), and the new configuration
//! must be written within 128 bus clocks of the unlock. The clock is the
//! LPO, [`LPO_HZ`] (CLK = 01), divided by 256: 128 ticks per second, so the
//! 16-bit TOVAL reaches 512 s.
//!
//! WDOG1's WCR ([`WDOG1_SOFTWARE_RESET`]): SRS (bit 4) = 0 asserts the
//! software reset; WDA (bit 5) = 1 keeps the WDOG_B pin deasserted. It is
//! the reset that brought this EVKB through the boot ROM into the
//! bootloader on the bench (2026-11-13).

/// WDOG1 WCR value that resets the chip: SRS = 0, WDA = 1.
pub const WDOG1_SOFTWARE_RESET: u16 = 0x0020;

/// RTWDOG CNT write that unlocks the configuration (CMD32EN).
pub const RTWDOG_UNLOCK: u32 = 0xD928_C520;

/// RTWDOG CNT write that refreshes the counter (CMD32EN).
pub const RTWDOG_REFRESH: u32 = 0xB480_A602;

/// The RTWDOG's LPO clock.
pub const LPO_HZ: u32 = 32_768;

/// RTWDOG ticks per second with the ÷256 prescaler.
pub const TICKS_PER_SECOND: u32 = LPO_HZ / 256;

/// How long a trial image may run unconfirmed before the watchdog resets
/// it: time for `mwctl fw update` to see the board come back and confirm.
pub const TRIAL_SECONDS: u32 = 60;

const UPDATE: u32 = 1 << 5;
const EN: u32 = 1 << 7;
const CLK_LPO: u32 = 0b01 << 8;
const PRES: u32 = 1 << 12;
const CMD32EN: u32 = 1 << 13;
const FLG: u32 = 1 << 14;

/// The RTWDOG CS value of a trial: enabled, LPO ÷256, 32-bit commands,
/// configuration still updatable (so the firmware can stop it), no
/// interrupt, not running in debug halt, wait or stop.
pub const fn trial_cs() -> u32 {
    EN | CLK_LPO | PRES | CMD32EN | UPDATE
}

/// The TOVAL for `seconds` (saturating at the 16-bit counter's 512 s).
pub const fn trial_toval(seconds: u32) -> u32 {
    let t = seconds.saturating_mul(TICKS_PER_SECOND);
    if t > 0xFFFF { 0xFFFF } else { t }
}

/// Whether the RTWDOG with control `cs` is running.
pub const fn enabled(cs: u32) -> bool {
    cs & EN != 0
}

/// The CS value that stops a running RTWDOG configured as `cs`: EN clear,
/// UPDATE and CMD32EN kept so it can be configured again, and FLG (write
/// 1 to clear) not written back.
pub const fn released_cs(cs: u32) -> u32 {
    (cs & !(EN | FLG)) | UPDATE | CMD32EN
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rt15_a_trial_arms_an_unfed_rtwdog_for_sixty_seconds() {
        let cs = trial_cs();
        assert!(enabled(cs));
        assert_eq!(cs, 0x0000_31A0, "EN, CLK=LPO, PRES, CMD32EN, UPDATE");
        assert_eq!(trial_toval(TRIAL_SECONDS), 60 * 128);
        assert_eq!(trial_toval(10_000), 0xFFFF, "saturates at 512 s");
    }

    #[test]
    fn rt15_confirm_stops_the_watchdog_and_keeps_it_configurable() {
        let running = trial_cs() | FLG;
        let stopped = released_cs(running);
        assert!(!enabled(stopped));
        assert_eq!(stopped & FLG, 0, "FLG is write-one-to-clear: not written back");
        assert_ne!(stopped & UPDATE, 0);
        assert_ne!(stopped & CMD32EN, 0);
        assert_eq!(stopped & (CLK_LPO | PRES), CLK_LPO | PRES);
    }

    #[test]
    fn rt15_the_software_reset_is_srs_low_with_wdog_b_quiet() {
        assert_eq!(WDOG1_SOFTWARE_RESET & (1 << 4), 0, "SRS asserted (active low)");
        assert_ne!(WDOG1_SOFTWARE_RESET & (1 << 5), 0, "WDA deasserted");
    }
}
