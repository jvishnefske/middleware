//! Pure decision logic behind the payload SPI and I2C buses (ABI 2
//! `spi_*`, ABI 3 `i2c_*`) on the RP2040 and RP2350.
//!
//! The imperative shell (`src/shell/spi.rs`, `src/shell/i2c.rs`,
//! `src/shell/device.rs`) writes the peripheral registers; every decision
//! it makes comes from here, so it is unit tested on the host for both
//! chips:
//!
//! * [`func`] — the chips' fixed GPIO function tables (which SPI block and
//!   signal, which I2C block and line each GPIO can carry) and the
//!   resolution of a payload's `spi_open` pins onto one SPI block (V15).
//! * [`claims`] — which pins the SPI bus and the two I2C buses hold, so
//!   they never collide with each other, with the payload's GPIO pins or
//!   with the board's reserved and analog pins (V18).
//! * [`i2c`] — the I2C clock timing, the command sequence of one
//!   write / write-read / read transfer, its time bound and the error
//!   mapping (V16).
//! * [`spi`] — the SPI clock divider, the mode bits, the bounded FIFO pump
//!   of one transfer and its time bound (V17).
//!
//! The transfer state machine (`Idle → InFlight → Done`, read back once)
//! is esp-base's own [`esp_base::periph::dma::DmaState`], shared rather
//! than copied.

pub mod claims;
pub mod func;
pub mod i2c;
pub mod spi;
