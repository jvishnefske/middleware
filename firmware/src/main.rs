//! ESP base firmware binary.
//!
//! Built once per chip with exactly one of the cargo features `esp32`,
//! `esp32s3` or `esp32c3` (see `build.sh`); the library checks the
//! feature set. The binary only owns what must live in the binary crate,
//! the entry point and the ESP-IDF application descriptor, and runs the
//! library's shell ([`esp_base::shell::run`]) with the compiled chip's
//! default board ([`esp_base::board::Board::chip_default`] of
//! [`esp_base::shell::CHIP`]), reporting this crate's version in
//! `sys/info` (with `+no-wasm` on a build without the WASM runtime).
#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use esp_base::board::Board;
use esp_base::shell;

/// Firmware version reported in `sys/info`.
#[cfg(feature = "wasm")]
const FIRMWARE: &str = env!("CARGO_PKG_VERSION");
/// Firmware version reported in `sys/info`; the suffix marks a build
/// without the WASM runtime.
#[cfg(not(feature = "wasm"))]
const FIRMWARE: &str = concat!(env!("CARGO_PKG_VERSION"), "+no-wasm");

/// The compiled chip's default board.
static BOARD: Board = match Board::chip_default(&shell::CHIP, FIRMWARE) {
    Ok(board) => board,
    Err(e) => panic!("{}", e.as_str()),
};

// The application descriptor the ESP-IDF second-stage bootloader requires.
esp_bootloader_esp_idf::esp_app_desc!();

#[esp_rtos::main]
async fn main(spawner: embassy_executor::Spawner) -> ! {
    shell::run(spawner, &BOARD).await
}
