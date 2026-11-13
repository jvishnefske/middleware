//! The i.MX RT base firmware (MIMXRT1050-EVKB).
//!
//! Booted by `imxrt-boot`, which copied this image into SDRAM at its link
//! address, checked its SHA-256 and left the `Handoff` word in SRC_GPR8.
//! cortex-m-rt's `set-vtor` points VTOR at this image's vector table at
//! reset. The binary is the imperative shell ([`app`]) around the pure
//! core in the `imxrt_base` library: it serves the middleware serial
//! protocol on LPUART1 (the OpenSDA VCOM) through the request service.
//!
//! A panic resets the chip (`SCB::sys_reset`). No production path panics:
//! the only `unwrap` is the link task's spawn at boot.
#![no_std]
#![no_main]
#![forbid(unsafe_code)]

#[cfg(feature = "wasm")]
extern crate alloc;

mod app;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    cortex_m::peripheral::SCB::sys_reset()
}

#[embassy_executor::main]
async fn main(spawner: embassy_executor::Spawner) -> ! {
    app::run(spawner).await
}
