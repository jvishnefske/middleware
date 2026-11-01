//! RP base firmware binary.
//!
//! Built once per chip with exactly one of the cargo features `rp2040`
//! (Raspberry Pi Pico, `thumbv6m-none-eabi`) or `rp2350` (Pico 2,
//! `thumbv8m.main-none-eabihf`); see `build.sh`. The binary is the
//! imperative shell around the pure core in the `rp_base` library:
//! [`shell`] brings up the heap (with the payload runtime), USB, flash and
//! the payload pins, then runs the controller.
//!
//! A panic resets the chip (`SCB::sys_reset`). No production path panics:
//! the only `unwrap`s are on task spawns at boot, and the heap check keeps
//! the interpreter's allocations within the arena.
#![no_std]
#![no_main]
#![forbid(unsafe_code)]

#[cfg(not(any(feature = "rp2040", feature = "rp2350")))]
compile_error!("enable exactly one chip feature: rp2040 or rp2350");

#[cfg(all(feature = "rp2040", feature = "rp2350"))]
compile_error!("enable exactly one chip feature: rp2040 or rp2350");

#[cfg(feature = "wasm")]
extern crate alloc;

mod shell;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    cortex_m::peripheral::SCB::sys_reset()
}

#[embassy_executor::main]
async fn main(spawner: embassy_executor::Spawner) -> ! {
    shell::run(spawner).await
}
