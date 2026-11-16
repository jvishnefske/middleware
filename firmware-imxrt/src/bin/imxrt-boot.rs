//! The A/B bootloader of the i.MX RT base firmware (MIMXRT1050-EVKB).
//!
//! The boot ROM boots it from HyperFlash block 0 (FCB, IVT, the DCD that
//! brings up SDRAM, then this image, copied to ITCM by imxrt-rt's startup).
//! It never executes from the HyperFlash, so it may write the update
//! record. The decision is the pure core's (`imxrt_base::ab::boot`):
//!
//! 1. bring up LPUART1 (messages) and FlexSPI (the HyperFlash driver);
//! 2. read the update record (`ab::record::load`);
//! 3. [`decide`]: check slots in the record's order, each check copying the
//!    slot's image into [`APP_IMAGE`] (SDRAM at the firmware's link
//!    address) and hashing that copy;
//! 4. a trial boot persists its attempt first (`ab::record::save`); if that
//!    write fails, decide again with the attempt counted in RAM, which
//!    rolls back;
//! 5. leave the [`Handoff`] word in SRC_GPR8 (which slot, on trial or not;
//!    the firmware reads and clears it), stop the PIT, mask every NVIC
//!    interrupt, and enter the copy ([`enter`]).
//!
//! With neither slot valid it boots nothing and prints each slot's reason
//! on LPUART1 every few seconds (recovery). A trial boot's watchdog is
//! armed by the firmware itself (`imxrt_base::ab::watchdog`): imxrt-rt's
//! startup in every image disables the RTWDOG, so one armed here would not
//! survive the jump.
//!
//! # `unsafe`
//!
//! The crate is `#![deny(unsafe_code)]` with one exception, agreed for the
//! bootloader only (design.md "Boot, A/B slots and the bootloader"):
//! [`jump`], the transfer of control to the firmware, for which Rust has
//! no safe API.
#![no_std]
#![no_main]
#![deny(unsafe_code)]

use core::fmt::Write;
use core::sync::atomic::{AtomicU32, Ordering::Relaxed};

use embassy_nxp::pac;
use imxrt_base::ab::boot::{Decision, Handoff, check_slot, decide};
use imxrt_base::ab::header::ImageDest;
use imxrt_base::ab::record;
use imxrt_base::layout::{RT1050_EVKB, SDRAM_IMAGE};
use imxrt_base::shell::console::Console;
use imxrt_base::shell::flexspi::HyperFlash;

/// Words of the SDRAM image region (4 MiB, `layout::SDRAM_IMAGE`).
const IMAGE_WORDS: usize = 4 * 1024 * 1024 / 4;

/// The SDRAM image region the bootloader copies a slot into: placed at
/// 0x8000_0000, the firmware's link address, by `boot-image.x`, which
/// fails the link otherwise. NOLOAD (the startup does not zero 4 MiB of
/// SDRAM): every byte the bootloader reads back it wrote first. Atomics,
/// so the region is written and read without `unsafe`.
static APP_IMAGE: [AtomicU32; IMAGE_WORDS] = [const { AtomicU32::new(0) }; IMAGE_WORDS];

/// [`ImageDest`] over [`APP_IMAGE`], little-endian bytes in each word.
struct Sdram(&'static [AtomicU32; IMAGE_WORDS]);

impl ImageDest for Sdram {
    fn capacity(&self) -> usize {
        IMAGE_WORDS * 4
    }

    fn write(&mut self, at: usize, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            let p = at + i;
            if let Some(w) = self.0.get(p / 4) {
                let mut bytes = w.load(Relaxed).to_le_bytes();
                bytes[p % 4] = b;
                w.store(u32::from_le_bytes(bytes), Relaxed);
            }
        }
    }

    fn read(&self, at: usize, buf: &mut [u8]) {
        for (i, b) in buf.iter_mut().enumerate() {
            let p = at + i;
            *b = self.0.get(p / 4).map_or(0, |w| w.load(Relaxed).to_le_bytes()[p % 4]);
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    cortex_m::peripheral::SCB::sys_reset()
}

/// Transfers control to the vector table at `vector_table`: sets MSP to
/// its first word and branches to its second (the firmware sets VTOR
/// itself, cortex-m-rt `set-vtor`).
///
/// The firmware's one `unsafe` operation. [`enter`] calls it only after
/// checking: the image was copied to [`APP_IMAGE`] and its SHA-256 matched
/// the slot header (the decision is `Boot`), `vector_table` is the copy's
/// start, the initial stack pointer is inside DTCM and the reset vector is
/// a Thumb address inside the copy.
#[allow(unsafe_code)]
fn jump(vector_table: u32) -> ! {
    // SAFETY: the invariants above hold (checked by `enter`); nothing of
    // the bootloader is used after the branch.
    unsafe { cortex_m::asm::bootload(vector_table as *const u32) }
}

/// Zeroes [`APP_IMAGE`] after the first `len` bytes (from the next word).
fn zero_after(len: u32) {
    let first = (len as usize).div_ceil(4);
    for w in APP_IMAGE.iter().skip(first) {
        w.store(0, Relaxed);
    }
}

/// DTCM, where the firmware's initial stack pointer must point.
const DTCM: core::ops::RangeInclusive<u32> = 0x2000_0000..=0x2002_0000;

/// Checks the copy's vector table, quiets the interrupts the bootloader
/// used, and jumps. Returns only if the vector table is unusable.
fn enter(len: u32, console: &mut Console) {
    let sp = APP_IMAGE[0].load(Relaxed);
    let reset = APP_IMAGE[1].load(Relaxed);
    let base = SDRAM_IMAGE.base;
    let in_image = (base..base + len).contains(&(reset & !1));
    if !DTCM.contains(&sp) || reset & 1 == 0 || !in_image {
        let _ = writeln!(
            console,
            "imxrt-boot: refusing to enter: SP {sp:#010x}, reset {reset:#010x}\r"
        );
        return;
    }
    // Zero the rest of the image region: the firmware's SDRAM statics
    // (`.sdram_bss` in memory-app.x, the payload heap) live there, NOLOAD,
    // and start zeroed like .bss only because of this.
    zero_after(len);
    // Stop the PIT (embassy's time driver) and mask, and clear, the
    // interrupts embassy-nxp's `init` enabled, so no handler of the
    // bootloader's runs inside the firmware's startup. PRIMASK stays
    // clear: the firmware's critical sections restore it.
    console.flush();
    pac::PIT.mcr().modify(|w| w.set_mdis(true));
    use cortex_m::peripheral::NVIC;
    use pac::Interrupt as I;
    for irq in [
        I::PIT,
        I::GPIO1_COMBINED_0_15,
        I::GPIO1_COMBINED_16_31,
        I::GPIO2_COMBINED_0_15,
        I::GPIO5_COMBINED_0_15,
    ] {
        NVIC::mask(irq);
        NVIC::unpend(irq);
    }
    jump(base);
}

#[cortex_m_rt::entry]
fn main() -> ! {
    let p = embassy_nxp::init(Default::default());
    let mut console = Console::new(p.LPUART1);
    let _ = writeln!(console, "\r\nimxrt-boot {}\r", env!("CARGO_PKG_VERSION"));
    let map = RT1050_EVKB;
    let ab = map.ab();
    let mut flash = match HyperFlash::new(p.FLEXSPI1) {
        Ok(f) => f,
        Err(e) => recovery(&mut console, format_args!("HyperFlash unavailable: {e:?}")),
    };
    let stored = match record::load(&mut flash, ab.record) {
        Ok(s) => s,
        Err(_) => recovery(&mut console, format_args!("update record unreadable")),
    };
    let _ = writeln!(console, "imxrt-boot: record {:?}\r", stored.record);
    let mut dest = Sdram(&APP_IMAGE);
    let mut decision = decide(&stored.record, |slot| check_slot(&mut flash, &ab, slot, &mut dest));
    if let Decision::Boot { persist: Some(next), .. } = decision
        && record::save(&mut flash, ab.record, &stored, next).is_err()
    {
        let _ = writeln!(console, "imxrt-boot: trial attempt not persisted; rolling back\r");
        decision = decide(&stored.record.with_attempt(), |slot| {
            check_slot(&mut flash, &ab, slot, &mut dest)
        });
    }
    match decision {
        Decision::Boot { slot, trial, header, .. } => {
            let _ = writeln!(
                console,
                "imxrt-boot: slot {slot:?}{} v{}.{}, {} bytes, SHA-256 ok\r",
                if trial { " on trial" } else { "" },
                header.version.0 >> 16,
                header.version.0 & 0xFFFF,
                header.len,
            );
            if let Some(h) = decision.handoff() {
                pac::SRC.gpr8().write_value(h.encode());
            }
            enter(header.len, &mut console);
            recovery(&mut console, format_args!("slot {slot:?} has an unusable vector table"))
        }
        Decision::Recovery { a, b } => recovery(
            &mut console,
            format_args!("no bootable slot: A {}, B {}", a.as_str(), b.as_str()),
        ),
    }
}

/// Boots nothing: prints `why` every few seconds, forever (bounded waits).
fn recovery(console: &mut Console, why: core::fmt::Arguments<'_>) -> ! {
    pac::SRC.gpr8().write_value(0);
    loop {
        let _ = writeln!(console, "imxrt-boot: RECOVERY: {why}\r");
        embassy_time::block_for(embassy_time::Duration::from_secs(3));
    }
}

/// The handoff word is the core's; keep the type in the docs' scope.
#[allow(dead_code)]
type HandoffWord = Handoff;
