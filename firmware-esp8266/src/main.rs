//! ESP8266 base firmware: the imperative shell.
//!
//! Boot sequence, then a blocking super-loop with no executor:
//!
//! 1. Take the peripherals; arm the hardware watchdog (~26.8 s stage).
//! 2. UART0 on GPIO1 (TX) / GPIO3 (RX) at 115200 baud. Wait ~10 ms for
//!    the line to go idle after the ROM's 74880-baud boot log, then write
//!    one `0x00` delimiter ([`Link::prime`], as `firmware/` does). Nothing
//!    is ever printed as text: logs are `sys/log` publications.
//! 3. SPI0 flash behind [`HalFlash`] (the [`RawFlash`] seam) and
//!    [`RegionFlash`] (the payload region `0x100000..0x140000`). The
//!    [`Controller`] verifies a stored image and announces the boot.
//! 4. Loop forever: feed the watchdog, poll UART RX (at most
//!    [`firmware_esp8266::link::RX_BUDGET`] bytes per pass, so the ~128-byte
//!    RX FIFO is drained often), and hand each complete frame to the
//!    controller, which writes its reply before the next poll.
//!
//! All buffers (the frame splitter and the transmit buffers, about 3 KiB)
//! live on `main`'s stack and are passed down; there is no heap and no
//! static state.
//!
//! The longest blocking step is `sys/loader/begin` for the largest image:
//! 64 sector erases, measured at about 1.1 s on the board and within the
//! 26.8 s watchdog stage even at the flash's worst-case 400 ms per sector.
//! The host is waiting for the reply meanwhile, so no input is lost.
//!
//! The panic handler spins; the watchdog then resets the chip. Production
//! paths do not panic (no `unwrap`, all indexing checked).
//!
//! # `wdt-test` feature (hardware test only)
//!
//! Off by default; the shipped image does not contain it. With
//! `./build.sh --features wdt-test` the super-loop runs normally for
//! `WDT_TEST_RUN_SECS` (5 s) after boot (counted in CCOUNT cycles with
//! `wrapping_sub`, so the 53.7 s counter wrap is harmless), logs
//! `wdt-test: starving watchdog` on `sys/log`, then stops feeding the
//! watchdog and spins. The watchdog stage then expires and resets the chip
//! 26.84 s later, and it boots again and repeats the cycle (measured: 31.87 s
//! reset to reset). The ROM boot log reports `rst cause:4` (watchdog); after
//! a watchdog reset the PLL is still set up, so that log arrives at the app's
//! 115200 baud, not the 74880 baud of a power-on boot.

#![no_std]
#![no_main]
#![deny(unsafe_code)]

use core::panic::PanicInfo;

use esp_base::chip::ESP8266;
use esp8266_hal::flash::ESPFlash;
use esp8266_hal::prelude::*;
use esp8266_hal::target::Peripherals;
use esp8266_hal::watchdog::StageTimeout;
use firmware_esp8266::controller::Controller;
use firmware_esp8266::flash::{DeviceError, RawFlash, RegionFlash};
use firmware_esp8266::link::{Link, Port, Rx, TxBuffers};
use middleware::serial::frame::{MAX_WIRE_FRAME, Splitter};

/// CPU clock after ROM/PLL init; CCOUNT increments once per CPU cycle.
const CPU_HZ: u32 = 80_000_000;

/// `wdt-test` only: seconds of normal operation before the watchdog is
/// starved. 5 s is 400 M cycles, well inside CCOUNT's 32-bit range.
#[cfg(feature = "wdt-test")]
const WDT_TEST_RUN_SECS: u32 = 5;

/// The HAL's SPI0 flash as a [`RawFlash`]. Its operations cannot fail
/// (`Void` errors); erases go one sector per call because the HAL's
/// multi-sector erase is broken.
struct HalFlash(ESPFlash);

impl RawFlash for HalFlash {
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), DeviceError> {
        self.0.read(addr, buf).map_err(|_| DeviceError)
    }

    fn program(&mut self, addr: u32, data: &[u8]) -> Result<(), DeviceError> {
        self.0.write_bytes(addr, data).map_err(|_| DeviceError)
    }

    fn erase_sector(&mut self, addr: u32) -> Result<(), DeviceError> {
        self.0.erase_sectors(addr, 1).map_err(|_| DeviceError)
    }
}

#[entry]
fn main() -> ! {
    let Some(dp) = Peripherals::take() else {
        // Only reachable if peripherals were already taken; nothing to report on.
        loop {
            core::hint::spin_loop();
        }
    };

    #[cfg(feature = "wdt-test")]
    let boot_cycles = xtensa_lx::timer::get_cycle_count();

    let mut watchdog = dp.WDT.watchdog();
    watchdog.start(StageTimeout::Stage2684Sec);
    watchdog.feed();

    let pins = dp.GPIO.split();
    let serial = dp
        .UART0
        .serial(pins.gpio1.into_uart(), pins.gpio3.into_uart());
    let mut link = Link::new(serial);
    let mut tx = TxBuffers::new();
    let mut splitter: Splitter<MAX_WIRE_FRAME> = Splitter::new();

    // Let the line go idle after the ROM's boot log (~10 ms).
    xtensa_lx::timer::delay(CPU_HZ / 100);
    // A failed delimiter only risks the first frame; the host retries.
    let _ = link.prime();

    let flash = RegionFlash::new(HalFlash(dp.SPI0.flash()));
    let mut controller = Controller::boot(flash, &ESP8266, &mut Port::new(&mut link, &mut tx));

    loop {
        watchdog.feed();
        // Receive errors (framing, overrun) only cost the current frame;
        // the splitter resynchronises at the next delimiter.
        if let Ok(Rx::Frame(Ok(frame))) = link.poll(&mut splitter) {
            controller.on_frame(&frame, &mut Port::new(&mut link, &mut tx));
        }
        #[cfg(feature = "wdt-test")]
        if xtensa_lx::timer::get_cycle_count().wrapping_sub(boot_cycles)
            >= WDT_TEST_RUN_SECS * CPU_HZ
        {
            firmware_esp8266::controller::log(
                &mut Port::new(&mut link, &mut tx),
                format_args!("wdt-test: starving watchdog"),
            );
            starve_watchdog();
        }
    }
}

/// `wdt-test` only: spin without feeding the watchdog, so it resets the
/// chip. The one deliberately unbounded loop besides the panic handler.
#[cfg(feature = "wdt-test")]
fn starve_watchdog() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    // The watchdog resets the chip.
    loop {
        core::hint::spin_loop();
    }
}
