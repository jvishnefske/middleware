//! Pin claims: which pins the SPI bus, the secondary UART, GPIO
//! interrupts, PWM outputs, the I2C bus and the camera hold.
//!
//! A [`Claims`] value is one pin bitmask per [`Owner`]. Every
//! transition is a function from a claims value to a new one, applied only
//! when it succeeds:
//!
//! * [`Claims::claim_spi`] / [`Claims::claim_uart`] replace the owner's
//!   previous pins (re-opening reconfigures), check every new pin against
//!   the chip [`Profile`] in the direction the signal needs (SCK, MOSI, CS
//!   and UART TX drive the pin; MISO and UART RX read it), and refuse pins
//!   held by another owner. The profile's reserved table already covers
//!   flash/PSRAM, USB D-/D+ and the UART0 console pins, so UART1 can never
//!   land on them.
//! * [`Claims::set_irq`] adds or removes a GPIO interrupt pin; a pin held by
//!   SPI or UART cannot take an interrupt.
//! * [`Claims::claim_pwm`], [`Claims::claim_i2c`] and
//!   [`Claims::claim_camera`] (ABI 3 PWM and I2C, R15 camera) take the
//!   board-mapped PWM output pin, the I2C bus's SDA and SCL (both driven
//!   open drain and read back) and the camera's eleven input pins; each is
//!   checked against the profile in its direction and refused when another
//!   owner holds it. [`Claims::release`] drops one owner's pins.
//! * [`Claims::check_gpio`] guards plain `gpio_*` calls: pins routed to a
//!   peripheral (SPI, UART, PWM, I2C, camera) are off limits until the
//!   payload stops or the peripheral lets them go.
//!
//! Errors follow the ABI: a pin that does not exist is
//! [`AbiError::InvalidArgument`], a reserved pin (or an output on an
//! input-only pin) is [`AbiError::Unsupported`], and a pin another
//! peripheral holds is [`AbiError::InvalidArgument`].

use middleware::wasm_abi::AbiError;
use middleware::wasm_abi::periph::{SpiConfig, UartConfig};
use middleware::wasm_abi::route::CameraPins;

use crate::chip::{Direction, Profile};

/// Who holds a pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// A GPIO interrupt (`gpio_irq` with an edge other than `Off`).
    Irq,
    /// SPI bus 0.
    Spi,
    /// Secondary UART 1.
    Uart,
    /// A PWM output (ABI 3).
    Pwm,
    /// I2C bus 0's SDA and SCL (ABI 3).
    I2c,
    /// The camera's parallel port (R15).
    Camera,
}

/// Pin ownership for one payload session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Claims {
    irq: u64,
    spi: u64,
    uart: u64,
    pwm: u64,
    i2c: u64,
    camera: u64,
}

const fn bit(pin: u8) -> u64 {
    if pin < 64 { 1u64 << pin } else { 0 }
}

/// The pins of `mask`, ascending.
pub fn pins(mask: u64) -> impl Iterator<Item = u8> {
    (0u8..64).filter(move |&p| mask & bit(p) != 0)
}

impl Claims {
    /// Nothing claimed.
    pub const fn new() -> Self {
        Claims { irq: 0, spi: 0, uart: 0, pwm: 0, i2c: 0, camera: 0 }
    }

    /// Who holds `pin`, if anyone.
    pub const fn owner(&self, pin: u8) -> Option<Owner> {
        let b = bit(pin);
        if b == 0 {
            None
        } else if self.irq & b != 0 {
            Some(Owner::Irq)
        } else if self.spi & b != 0 {
            Some(Owner::Spi)
        } else if self.uart & b != 0 {
            Some(Owner::Uart)
        } else if self.pwm & b != 0 {
            Some(Owner::Pwm)
        } else if self.i2c & b != 0 {
            Some(Owner::I2c)
        } else if self.camera & b != 0 {
            Some(Owner::Camera)
        } else {
            None
        }
    }

    /// The pins `owner` holds.
    pub const fn pins_of(&self, owner: Owner) -> u64 {
        match owner {
            Owner::Irq => self.irq,
            Owner::Spi => self.spi,
            Owner::Uart => self.uart,
            Owner::Pwm => self.pwm,
            Owner::I2c => self.i2c,
            Owner::Camera => self.camera,
        }
    }

    /// The claims without any of `owner`'s pins.
    pub const fn release(self, owner: Owner) -> Self {
        match owner {
            Owner::Irq => Claims { irq: 0, ..self },
            Owner::Spi => Claims { spi: 0, ..self },
            Owner::Uart => Claims { uart: 0, ..self },
            Owner::Pwm => Claims { pwm: 0, ..self },
            Owner::I2c => Claims { i2c: 0, ..self },
            Owner::Camera => Claims { camera: 0, ..self },
        }
    }

    /// Pins with a GPIO interrupt enabled.
    pub const fn irq_pins(&self) -> u64 {
        self.irq
    }

    /// Pins routed to SPI bus 0.
    pub const fn spi_pins(&self) -> u64 {
        self.spi
    }

    /// Pins routed to UART 1.
    pub const fn uart_pins(&self) -> u64 {
        self.uart
    }

    /// Checks `wanted` (pin, direction) pairs for `owner`, ignoring pins the
    /// owner already holds; returns the new mask.
    fn check(
        &self,
        profile: &Profile,
        owner: Owner,
        wanted: &[(Option<u8>, Direction)],
    ) -> Result<u64, AbiError> {
        let mut mask = 0u64;
        for &(pin, dir) in wanted {
            let Some(pin) = pin else { continue };
            profile.check_pin(pin, dir)?;
            match self.owner(pin) {
                Some(o) if o != owner => return Err(AbiError::InvalidArgument),
                _ => mask |= bit(pin),
            }
        }
        Ok(mask)
    }

    /// Claims the pins of an SPI configuration, replacing the bus's previous
    /// pins.
    pub fn claim_spi(self, profile: &Profile, cfg: &SpiConfig) -> Result<Self, AbiError> {
        let spi = self.check(
            profile,
            Owner::Spi,
            &[
                (Some(cfg.sck), Direction::Output),
                (cfg.mosi, Direction::Output),
                (cfg.miso, Direction::Input),
                (cfg.cs, Direction::Output),
            ],
        )?;
        Ok(Claims { spi, ..self })
    }

    /// Claims the pins of a UART configuration, replacing the port's previous
    /// pins.
    pub fn claim_uart(self, profile: &Profile, cfg: &UartConfig) -> Result<Self, AbiError> {
        let uart = self.check(
            profile,
            Owner::Uart,
            &[(cfg.tx, Direction::Output), (cfg.rx, Direction::Input)],
        )?;
        Ok(Claims { uart, ..self })
    }

    /// Adds PWM output `pin` (a timer channel's board-mapped pin; it is
    /// driven). Other PWM pins stay claimed.
    pub fn claim_pwm(self, profile: &Profile, pin: u8) -> Result<Self, AbiError> {
        let add = self.check(profile, Owner::Pwm, &[(Some(pin), Direction::Output)])?;
        Ok(Claims { pwm: self.pwm | add, ..self })
    }

    /// Claims I2C bus 0's `sda` and `scl`, replacing its previous pins.
    /// Both are driven (open drain) and read back, so both must be able to
    /// drive.
    pub fn claim_i2c(self, profile: &Profile, sda: u8, scl: u8) -> Result<Self, AbiError> {
        if sda == scl {
            return Err(AbiError::InvalidArgument);
        }
        let i2c = self.check(
            profile,
            Owner::I2c,
            &[(Some(sda), Direction::Output), (Some(scl), Direction::Output)],
        )?;
        Ok(Claims { i2c, ..self })
    }

    /// Claims the camera's eleven input pins (data, VSYNC, HREF, PCLK),
    /// replacing its previous pins; input-only pins are fine.
    pub fn claim_camera(self, profile: &Profile, pins: &CameraPins) -> Result<Self, AbiError> {
        let all = pins.validated()?.all();
        let mut wanted = [(None, Direction::Input); 11];
        for (slot, pin) in wanted.iter_mut().zip(all) {
            *slot = (Some(pin), Direction::Input);
        }
        let camera = self.check(profile, Owner::Camera, &wanted)?;
        Ok(Claims { camera, ..self })
    }

    /// Enables (`on`) or disables the interrupt claim on `pin`.
    pub fn set_irq(self, profile: &Profile, pin: u8, on: bool) -> Result<Self, AbiError> {
        if !on {
            return Ok(Claims { irq: self.irq & !bit(pin), ..self });
        }
        let irq = self.check(profile, Owner::Irq, &[(Some(pin), Direction::Input)])?;
        Ok(Claims { irq: self.irq | irq, ..self })
    }

    /// Whether a plain `gpio_*` call may touch `pin` (pins routed to a
    /// peripheral are refused; interrupt pins are allowed, the shell drops
    /// the interrupt when the pin is reconfigured).
    pub const fn check_gpio(&self, pin: u8) -> Result<(), AbiError> {
        match self.owner(pin) {
            Some(Owner::Spi | Owner::Uart | Owner::Pwm | Owner::I2c | Owner::Camera) => {
                Err(AbiError::InvalidArgument)
            }
            Some(Owner::Irq) | None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chip::{ESP32, ESP32C3, ESP32S3};

    fn spi(sck: i32, mosi: i32, miso: i32, cs: i32) -> SpiConfig {
        SpiConfig::parse(0, sck, mosi, miso, cs, 1000, 0).unwrap()
    }

    fn uart(tx: i32, rx: i32) -> UartConfig {
        UartConfig::parse(1, tx, rx, 115_200).unwrap()
    }

    #[test]
    fn d6_spi_pins_are_checked_against_the_chip() {
        let c = Claims::new().claim_spi(&ESP32S3, &spi(12, 11, 13, 10)).unwrap();
        assert_eq!(c.owner(12), Some(Owner::Spi));
        assert_eq!(pins(c.spi_pins()).collect::<heapless::Vec<u8, 4>>(), [10, 11, 12, 13]);
        // Flash, USB and console pins are reserved.
        assert_eq!(Claims::new().claim_spi(&ESP32S3, &spi(30, 11, 13, -1)), Err(AbiError::Unsupported));
        assert_eq!(Claims::new().claim_spi(&ESP32C3, &spi(6, 18, -1, -1)), Err(AbiError::Unsupported));
        assert_eq!(Claims::new().claim_spi(&ESP32, &spi(6, 13, -1, -1)), Err(AbiError::Unsupported));
        // Missing pin.
        assert_eq!(Claims::new().claim_spi(&ESP32, &spi(20, 13, -1, -1)), Err(AbiError::InvalidArgument));
        // ESP32 GPIO34-39 can be MISO (input) but not SCK/MOSI/CS.
        assert!(Claims::new().claim_spi(&ESP32, &spi(14, 13, 34, 15)).is_ok());
        assert_eq!(Claims::new().claim_spi(&ESP32, &spi(34, 13, 12, 15)), Err(AbiError::Unsupported));
        assert_eq!(Claims::new().claim_spi(&ESP32, &spi(14, 13, 12, 35)), Err(AbiError::Unsupported));
    }

    #[test]
    fn d6_reopening_spi_replaces_its_pins() {
        let c = Claims::new().claim_spi(&ESP32S3, &spi(12, 11, 13, 10)).unwrap();
        let c = c.claim_spi(&ESP32S3, &spi(12, 4, -1, -1)).unwrap();
        assert_eq!(c.owner(11), None);
        assert_eq!(c.owner(13), None);
        assert_eq!(c.owner(4), Some(Owner::Spi));
        assert_eq!(c.check_gpio(11), Ok(()));
        assert_eq!(c.check_gpio(4), Err(AbiError::InvalidArgument));
    }

    #[test]
    fn d7_uart_pins_avoid_console_flash_usb_and_other_owners() {
        // UART0 console pins per chip.
        for p in [&ESP32, &ESP32S3, &ESP32C3] {
            assert_eq!(
                Claims::new().claim_uart(p, &uart(i32::from(p.uart0_tx), -1)),
                Err(AbiError::Unsupported),
                "{}",
                p.name
            );
            assert_eq!(
                Claims::new().claim_uart(p, &uart(-1, i32::from(p.uart0_rx))),
                Err(AbiError::Unsupported),
                "{}",
                p.name
            );
        }
        assert_eq!(Claims::new().claim_uart(&ESP32C3, &uart(19, 4)), Err(AbiError::Unsupported), "USB D+");
        assert_eq!(Claims::new().claim_uart(&ESP32, &uart(10, 4)), Err(AbiError::Unsupported), "flash");
        assert_eq!(Claims::new().claim_uart(&ESP32, &uart(36, 4)), Err(AbiError::Unsupported), "input-only TX");
        let c = Claims::new().claim_uart(&ESP32, &uart(4, 36)).unwrap();
        assert_eq!(c.owner(36), Some(Owner::Uart));
        // SPI cannot take a UART pin, nor UART an SPI pin.
        assert_eq!(c.claim_spi(&ESP32, &spi(4, 13, -1, -1)), Err(AbiError::InvalidArgument));
        let c = c.claim_spi(&ESP32, &spi(14, 13, 12, 15)).unwrap();
        assert_eq!(c.claim_uart(&ESP32, &uart(13, 36)), Err(AbiError::InvalidArgument));
        // A failed claim leaves the previous claims untouched (values).
        assert_eq!(c.owner(4), Some(Owner::Uart));
    }

    #[test]
    fn d5_irq_pins_do_not_collide() {
        let c = Claims::new().set_irq(&ESP32S3, 5, true).unwrap();
        assert_eq!(c.owner(5), Some(Owner::Irq));
        assert_eq!(c.check_gpio(5), Ok(()), "irq pins stay readable");
        assert_eq!(c.claim_spi(&ESP32S3, &spi(5, 11, -1, -1)), Err(AbiError::InvalidArgument));
        assert_eq!(c.claim_uart(&ESP32S3, &uart(-1, 5)), Err(AbiError::InvalidArgument));
        let c = c.claim_uart(&ESP32S3, &uart(17, 18)).unwrap();
        assert_eq!(c.set_irq(&ESP32S3, 18, true), Err(AbiError::InvalidArgument));
        assert_eq!(c.set_irq(&ESP32S3, 43, true), Err(AbiError::Unsupported), "console");
        assert_eq!(c.set_irq(&ESP32S3, 22, true), Err(AbiError::InvalidArgument), "missing");
        // ESP32 input-only pins can take interrupts.
        assert!(Claims::new().set_irq(&ESP32, 39, true).is_ok());
        let off = c.set_irq(&ESP32S3, 5, false).unwrap();
        assert_eq!(off.owner(5), None);
        assert_eq!(pins(off.irq_pins()).count(), 0);
        assert_eq!(Claims::new().owner(200), None);
    }

    #[test]
    fn r15_pwm_i2c_and_camera_pins_are_owned_and_exclusive() {
        let cam = CameraPins::AI_THINKER;
        let c = Claims::new().claim_camera(&ESP32, &cam).unwrap();
        // Input-only data pins (34-39) are fine for the camera.
        for p in cam.all() {
            assert_eq!(c.owner(p), Some(Owner::Camera), "pin {p}");
            assert_eq!(c.check_gpio(p), Err(AbiError::InvalidArgument));
        }
        assert_eq!(pins(c.pins_of(Owner::Camera)).count(), 11);
        let c = c.claim_pwm(&ESP32, 0).unwrap().claim_i2c(&ESP32, 26, 27).unwrap();
        assert_eq!(c.owner(0), Some(Owner::Pwm));
        assert_eq!(c.owner(26), Some(Owner::I2c));
        assert_eq!(c.check_gpio(27), Err(AbiError::InvalidArgument));
        // Nobody takes another owner's pin.
        assert_eq!(c.claim_pwm(&ESP32, 22), Err(AbiError::InvalidArgument), "PCLK");
        assert_eq!(c.claim_i2c(&ESP32, 0, 13), Err(AbiError::InvalidArgument), "XCLK");
        assert_eq!(c.claim_spi(&ESP32, &spi(14, 13, 26, 15)), Err(AbiError::InvalidArgument));
        assert_eq!(c.set_irq(&ESP32, 25, true), Err(AbiError::InvalidArgument), "VSYNC");
        assert_eq!(Claims::new().claim_pwm(&ESP32, 36), Err(AbiError::Unsupported), "input-only");
        assert_eq!(Claims::new().claim_i2c(&ESP32, 26, 34), Err(AbiError::Unsupported));
        assert_eq!(Claims::new().claim_i2c(&ESP32, 26, 26), Err(AbiError::InvalidArgument));
        assert_eq!(Claims::new().claim_camera(&ESP32, &CameraPins { pclk: 1, ..cam }), Err(AbiError::Unsupported), "console");
        assert_eq!(Claims::new().claim_camera(&ESP32, &CameraPins { pclk: 5, ..cam }), Err(AbiError::InvalidArgument), "repeat");
        // A second PWM pin adds; re-claiming I2C or the camera replaces.
        let c = c.claim_pwm(&ESP32, 4).unwrap();
        assert_eq!(pins(c.pins_of(Owner::Pwm)).collect::<heapless::Vec<u8, 2>>(), [0, 4]);
        let c = c.claim_i2c(&ESP32, 14, 15).unwrap();
        assert_eq!(c.owner(26), None);
        // Release drops one owner only.
        let c = c.release(Owner::Camera);
        assert_eq!(c.owner(22), None);
        assert_eq!(c.check_gpio(22), Ok(()));
        assert_eq!(c.owner(0), Some(Owner::Pwm));
        assert_eq!(c.release(Owner::Pwm).owner(4), None);
    }
}
