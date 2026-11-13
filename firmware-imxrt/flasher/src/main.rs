//! RAM-loaded HyperFlash programmer for the MIMXRT1050-EVKB.
//!
//! `tools/ramload.py` loads it into DTCM over SWD and starts it (the board
//! in the boot ROM's serial downloader mode, or halted in any image); then
//! `tools/flash.py` drives it through [`MAILBOX`] and [`BUFFER`] (their
//! addresses come from this ELF's symbols), with the core halted while the
//! host writes them (the host finds both by their demangled names with
//! `llvm-nm`):
//!
//! | word | host writes            | flasher writes            |
//! |------|------------------------|---------------------------|
//! | 0    | command sequence (+1)  |                           |
//! | 1    | command: 1 erase, 2 program and verify | |
//! | 2    | flash offset           |                           |
//! | 3    | length in bytes        |                           |
//! | 4    |                        | sequence done             |
//! | 5    |                        | status: 1 ok, else error  |
//! | 6    |                        | detail (error code, first mismatch) |
//!
//! Erase takes a 256 KiB-aligned range; program writes `length` bytes of
//! [`BUFFER`] at the offset, then reads them back and compares. All flash
//! access is `imxrt_base::shell::flexspi::HyperFlash`, the firmware's own
//! driver. Progress is printed on LPUART1.
#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use core::fmt::Write;
use core::sync::atomic::{AtomicU32, Ordering::Relaxed, fence, Ordering::SeqCst};

use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use imxrt_base::shell::console::Console;
use imxrt_base::shell::flexspi::{FlashError, HyperFlash};

/// Words of [`BUFFER`]: one 256 KiB sector.
const BUFFER_WORDS: usize = 128 * 1024 / 4;

/// The command mailbox (see the module docs).
pub static MAILBOX: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];
/// The data to program, little-endian bytes in each word.
pub static BUFFER: [AtomicU32; BUFFER_WORDS] = [const { AtomicU32::new(0) }; BUFFER_WORDS];

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    cortex_m::peripheral::SCB::sys_reset()
}

fn byte(i: usize) -> u8 {
    BUFFER[i / 4].load(Relaxed).to_le_bytes()[i % 4]
}

fn code(e: FlashError) -> u32 {
    match e {
        FlashError::Timeout => 2,
        FlashError::Command(s) => 0x1000_0000 | (s & 0x0FFF_FFFF),
        FlashError::Failed => 3,
        FlashError::Busy => 4,
        FlashError::Lut => 5,
        FlashError::Plan => 6,
    }
}

fn program(flash: &mut HyperFlash, offset: u32, len: usize) -> Result<(), u32> {
    if len > BUFFER_WORDS * 4 || len % 2 != 0 {
        return Err(7);
    }
    let mut chunk = [0u8; 512];
    let mut at = 0;
    while at < len {
        let n = 512.min(len - at);
        for (k, b) in chunk[..n].iter_mut().enumerate() {
            *b = byte(at + k);
        }
        flash.write(offset + at as u32, &chunk[..n]).map_err(code)?;
        at += n;
    }
    // Read back and compare.
    let mut at = 0;
    while at < len {
        let n = 512.min(len - at);
        flash.read(offset + at as u32, &mut chunk[..n]).map_err(code)?;
        if let Some(k) = (0..n).find(|&k| chunk[k] != byte(at + k)) {
            return Err(0x8000_0000 | (at + k) as u32);
        }
        at += n;
    }
    Ok(())
}

#[cortex_m_rt::entry]
fn main() -> ! {
    let p = embassy_nxp::init(Default::default());
    let mut console = Console::new(p.LPUART1);
    let mut flash = match HyperFlash::new(p.FLEXSPI1) {
        Ok(f) => f,
        Err(e) => {
            MAILBOX[5].store(code(e), Relaxed);
            let _ = writeln!(console, "imxrt-flasher: HyperFlash unavailable: {e:?}\r");
            loop {
                cortex_m::asm::wfi();
            }
        }
    };
    // Only the host writes MAILBOX's command words and BUFFER, over SWD,
    // which the compiler cannot see: it would treat them as never changing
    // (a static nothing in the program stores to is folded to its
    // initializer, and the command loop compiled to a branch to itself).
    // Letting their addresses escape tells it something else may write
    // them, so every atomic load is a real load.
    core::hint::black_box(MAILBOX.as_ptr());
    core::hint::black_box(BUFFER.as_ptr());
    for w in BUFFER.iter() {
        w.store(0, Relaxed);
    }
    let _ = writeln!(console, "imxrt-flasher: ready\r");
    MAILBOX[5].store(1, Relaxed);
    let mut done = MAILBOX[0].load(Relaxed);
    MAILBOX[4].store(done, Relaxed);
    loop {
        let seq = MAILBOX[0].load(Relaxed);
        if seq == done {
            continue;
        }
        fence(SeqCst);
        let (cmd, offset, len) = (MAILBOX[1].load(Relaxed), MAILBOX[2].load(Relaxed), MAILBOX[3].load(Relaxed));
        let result = match cmd {
            1 => flash.erase(offset, offset.saturating_add(len)).map_err(code),
            2 => program(&mut flash, offset, len as usize),
            _ => Err(8),
        };
        let (status, detail) = match result {
            Ok(()) => (1, 0),
            Err(c) => (0x100, c),
        };
        let _ = writeln!(console, "imxrt-flasher: cmd {cmd} at {offset:#x} len {len:#x}: {}\r", if status == 1 { "ok" } else { "FAILED" });
        // Drop FlexSPI's AHB read buffers, so the host's read-back through
        // the memory-mapped window (0x6000_0000) sees the flash as it now is.
        embassy_nxp::pac::FLEXSPI.ahbcr().modify(|w| w.0 |= 1 << 1);
        MAILBOX[6].store(detail, Relaxed);
        MAILBOX[5].store(status, Relaxed);
        fence(SeqCst);
        MAILBOX[4].store(seq, Relaxed);
        done = seq;
    }
}
