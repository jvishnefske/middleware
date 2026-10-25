//! ESP base firmware binary.
//!
//! Built once per chip with exactly one of the cargo features `esp32`,
//! `esp32s3` or `esp32c3` (see `build.sh`). The binary is the imperative
//! shell around the pure core in the `esp_base` library: [`shell`] brings up
//! the heap, the embassy executor (via `esp-rtos`), the serial ports, flash
//! and GPIO, then runs the controller.
#![no_std]
#![no_main]
#![deny(unsafe_code)]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

#[cfg(not(any(feature = "esp32", feature = "esp32s3", feature = "esp32c3")))]
compile_error!("enable exactly one chip feature: esp32, esp32s3 or esp32c3");

#[cfg(any(
    all(feature = "esp32", feature = "esp32s3"),
    all(feature = "esp32", feature = "esp32c3"),
    all(feature = "esp32s3", feature = "esp32c3"),
))]
compile_error!("enable exactly one chip feature: esp32, esp32s3 or esp32c3");

extern crate alloc;

use esp_backtrace as _;

mod shell;

// The application descriptor the ESP-IDF second-stage bootloader requires.
esp_bootloader_esp_idf::esp_app_desc!();

#[esp_rtos::main]
async fn main(spawner: embassy_executor::Spawner) -> ! {
    shell::run(spawner).await
}
