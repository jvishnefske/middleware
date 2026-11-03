//! The RP2350 boot ROM and chip services of the firmware updater
//! ([`rp_base::ota::rom::Rom`]), with no `unsafe`:
//!
//! * `xip_map`: the QMI's `ATRANS0` register (embassy-rp's PAC; a register
//!   read is safe), where the boot ROM put the partition it booted;
//! * `reboot`: the boot ROM's `reboot` (embassy-rp's `rom_data::reboot`,
//!   a safe function), asynchronous: it returns and the watchdog resets
//!   the chip `delay_ms` later;
//! * `stop_watchdog`: embassy-rp's `Watchdog::stop` (the boot ROM's trial
//!   watchdog, once the image is bought; nothing else in the firmware
//!   touches the watchdog, so a hung trial image reverts);
//! * `uninterrupted`: `cortex_m::interrupt::free`.
//!
//! See `rp_base::ota::rom` for why `get_sys_info` and `explicit_buy` are
//! not called.

use embassy_rp::Peri;
use embassy_rp::pac;
use embassy_rp::peripherals::WATCHDOG;
use embassy_rp::rom_data;
use embassy_rp::watchdog::Watchdog;
use rp_base::ota::rom::{Reboot, Rom, XipMap};

/// Bytes per `ATRANS` unit (one sector).
const ATRANS_UNIT: u32 = 4096;

/// The chip's boot ROM, QMI and watchdog.
pub struct RpRom {
    watchdog: Watchdog,
}

impl RpRom {
    /// Takes the watchdog (not started, not fed: the boot ROM's trial
    /// watchdog keeps running until the buy).
    pub fn new(watchdog: Peri<'static, WATCHDOG>) -> Self {
        RpRom { watchdog: Watchdog::new(watchdog) }
    }
}

impl Rom for RpRom {
    type Error = i32;

    fn xip_map(&mut self) -> XipMap {
        let a = pac::QMI.atrans(0).read();
        XipMap { base: u32::from(a.base()) * ATRANS_UNIT, size: u32::from(a.size()) * ATRANS_UNIT }
    }

    fn reboot(&mut self, reboot: Reboot, delay_ms: u32) -> Result<(), i32> {
        let (flags, p0, p1) = reboot.args();
        let rc = rom_data::reboot(flags, delay_ms, p0, p1);
        if rc < 0 { Err(rc) } else { Ok(()) }
    }

    fn stop_watchdog(&mut self) {
        self.watchdog.stop();
    }

    fn uninterrupted<T>(&mut self, f: impl FnOnce() -> T) -> T {
        cortex_m::interrupt::free(|_| f())
    }
}

/// Maps the whole flash, untranslated, at XIP window 1 (`0x1040_0000`,
/// QMI `ATRANS1`: base 0, 4 MiB), where `rp_base::flash::Window` reads.
/// Window 0 (`0x1000_0000`) stays the boot ROM's mapping of the running
/// partition. Nothing has read window 1 before, so the XIP cache holds no
/// line of it.
pub fn map_untranslated_window() {
    pac::QMI.atrans(1).write(|w| {
        w.set_base(0);
        w.set_size(0x400);
    });
}
