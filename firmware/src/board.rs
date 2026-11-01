//! Board profiles: a chip [`Profile`] adapted to one board, and the boot
//! decisions it drives (E24–E27).
//!
//! A [`Board`] is what `esp_base::shell` (feature `shell`) runs with. It
//! wraps the chip's [`Profile`] and adds what only the board knows:
//!
//! * the name `sys/info` reports as the chip (e.g. `esp32s3-conagotchi`)
//!   and the firmware version it reports, which come from the caller, not
//!   from the crate that compiles the shell;
//! * pins the board keeps from payloads ([`BoardBuilder::reserve`]), and
//!   pins the chip profile refuses only because some modules wire them to
//!   flash or PSRAM, which a board without that memory may hand to
//!   payloads ([`BoardBuilder::release`]: ESP32-S3 GPIO33–37, octal
//!   flash/PSRAM; ESP32 GPIO16/17, PSRAM on WROVER modules);
//! * the presence (re-key) button: GPIO0 on ESP32, ESP32-S3 and ESP8266,
//!   GPIO9 on ESP32-C3 by default;
//! * an optional safe-mode pin ([`BoardBuilder::safe_mode`]), sampled once
//!   at boot as an input with the pull-up enabled, active low;
//! * the largest payload image accepted ([`BoardBuilder::max_image`]) and
//!   the fuel each call into the payload may burn
//!   ([`BoardBuilder::fuel_per_call`], default
//!   [`FUEL_PER_CALL`]).
//!
//! [`Board::chip_default`] reproduces the firmware's behaviour before
//! board profiles existed, for every chip: the chip's name, its pin policy,
//! presence button and image cap, no safe-mode pin, the ABI's fuel.
//!
//! # Construction
//!
//! [`Board::builder`] starts from a chip profile; every setter is a
//! `const fn`, so a board is a `static` item checked when the crate
//! compiles. [`BoardBuilder::build`] returns the first problem as a
//! [`BoardError`]: a pin that does not exist, a pin the shell itself owns
//! (the console UART, USB D-/D+) or that only the chip may decide on
//! (flash), a presence or safe-mode pin that is not a payload pin, a name
//! longer than `sys/info` carries, an image cap beyond one payload slot or
//! the heap budget, or zero fuel. The heap plan cannot be changed: its
//! regions are statics sized when the shell compiles.
//!
//! ```
//! use esp_base::board::Board;
//! use esp_base::chip::{Direction, ESP32S3};
//!
//! static BADGE: Board = match Board::builder(&ESP32S3, "0.4.0")
//!     .name("esp32s3-conagotchi")
//!     .release(&[33, 34, 35, 36, 37])
//!     .reserve(&[41])
//!     .safe_mode(21)
//!     .max_image(16 * 1024)
//!     .fuel_per_call(50_000)
//!     .build()
//! {
//!     Ok(board) => board,
//!     Err(e) => panic!("{}", e.as_str()),
//! };
//!
//! assert_eq!(BADGE.profile().name, "esp32s3-conagotchi");
//! assert_eq!(BADGE.profile().check_pin(35, Direction::Output), Ok(()));
//! assert!(BADGE.profile().is_reserved(41));
//! assert_eq!(BADGE.safe_mode(), Some(21));
//! ```
//!
//! (The `panic!` runs only while the compiler evaluates the `static`: a bad
//! board is a build error, never a run-time panic.)
//!
//! # Safe mode
//!
//! [`autostart`] decides at boot whether the stored payload starts, from
//! whether one is startable and the safe-mode pin's one sample
//! ([`SafeMode::from_sample`]). A held pin keeps the payload installed but
//! stopped; `sys/payload/start` still starts it. A pin that cannot be read
//! does not block the start: safe mode needs a positive reading.

use middleware::loader::MAX_NAME;
use middleware::wasm_abi::FUEL_PER_CALL;

use crate::chip::{Chip, INTERPRETER_RESERVE, Profile};
use crate::image::Region;

/// Size of the payload partition every ESP32-family `partitions.csv` uses
/// (and the ESP8266's payload region): two slots.
pub const PAYLOAD_PARTITION_BYTES: u32 = 0x4_0000;

/// Why a [`BoardBuilder`] does not describe a usable board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardError {
    /// The pin does not exist on the chip.
    NoSuchPin(u8),
    /// The shell owns the pin: UART0 TX/RX (the protocol port) or USB
    /// D-/D+ on a chip with USB-Serial-JTAG.
    ShellPin(u8),
    /// The chip profile refuses the pin for a reason no board can lift
    /// (SPI flash), so [`BoardBuilder::release`] cannot open it.
    NotReleasable(u8),
    /// The presence button is not a pin payloads may use (missing or
    /// reserved), so it could never be read.
    PresencePin(u8),
    /// The safe-mode pin is not a pin payloads may use.
    SafeModePin(u8),
    /// The `sys/info` chip name is empty or longer than
    /// [`MAX_NAME`] bytes.
    Name,
    /// The firmware version is empty or longer than [`MAX_NAME`] bytes.
    Firmware,
    /// The image cap is zero, larger than one slot of the
    /// [`PAYLOAD_PARTITION_BYTES`] partition, or (with the runtime) leaves
    /// the heap too small for linear memory, the image, the interpreter
    /// and the radio.
    MaxImage(u32),
    /// Fuel per call is zero: every call would trap.
    Fuel,
}

impl BoardError {
    /// A fixed description, usable in a `const` panic message.
    pub const fn as_str(&self) -> &'static str {
        match self {
            BoardError::NoSuchPin(_) => "board: pin does not exist on the chip",
            BoardError::ShellPin(_) => "board: pin belongs to the shell (console UART or USB)",
            BoardError::NotReleasable(_) => {
                "board: pin is reserved for flash and cannot be released"
            }
            BoardError::PresencePin(_) => "board: presence button is not a payload pin",
            BoardError::SafeModePin(_) => "board: safe-mode pin is not a payload pin",
            BoardError::Name => "board: sys/info name is empty or too long",
            BoardError::Firmware => "board: firmware version is empty or too long",
            BoardError::MaxImage(_) => {
                "board: max_image is zero or exceeds a payload slot or the heap"
            }
            BoardError::Fuel => "board: fuel per call is zero",
        }
    }
}

impl core::fmt::Display for BoardError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())?;
        match self {
            BoardError::NoSuchPin(p)
            | BoardError::ShellPin(p)
            | BoardError::NotReleasable(p)
            | BoardError::PresencePin(p)
            | BoardError::SafeModePin(p) => write!(f, " (GPIO{p})"),
            BoardError::MaxImage(n) => write!(f, " ({n} B)"),
            BoardError::Name | BoardError::Firmware | BoardError::Fuel => Ok(()),
        }
    }
}

/// A validated board profile (see the module docs). Only
/// [`BoardBuilder::build`] makes one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Board {
    profile: Profile,
    firmware: &'static str,
    presence: u8,
    safe_mode: Option<u8>,
    fuel_per_call: u64,
}

impl Board {
    /// Starts a board from `chip` (copied) reporting firmware version
    /// `firmware` in `sys/info`, with the chip defaults of
    /// [`Board::chip_default`].
    pub const fn builder(chip: &Profile, firmware: &'static str) -> BoardBuilder {
        BoardBuilder {
            board: Board {
                profile: *chip,
                firmware,
                presence: default_presence(chip.chip),
                safe_mode: None,
                fuel_per_call: FUEL_PER_CALL,
            },
            error: None,
        }
    }

    /// The chip's behaviour without board knowledge: `chip`'s name, pins,
    /// image cap and presence button, no safe-mode pin,
    /// [`FUEL_PER_CALL`]. Fails only for a bad `firmware` string.
    pub const fn chip_default(chip: &Profile, firmware: &'static str) -> Result<Board, BoardError> {
        Board::builder(chip, firmware).build()
    }

    /// The board's pin policy, limits and `sys/info` name.
    pub const fn profile(&self) -> &Profile {
        &self.profile
    }

    /// The firmware version `sys/info` reports.
    pub const fn firmware(&self) -> &'static str {
        self.firmware
    }

    /// The presence (re-key) button's GPIO, a payload pin.
    pub const fn presence(&self) -> u8 {
        self.presence
    }

    /// The safe-mode pin's GPIO, a payload pin, if the board has one.
    pub const fn safe_mode(&self) -> Option<u8> {
        self.safe_mode
    }

    /// Fuel per call into the payload (never zero).
    pub const fn fuel_per_call(&self) -> u64 {
        self.fuel_per_call
    }
}

/// The presence button by chip: the BOOT button.
const fn default_presence(chip: Chip) -> u8 {
    match chip {
        Chip::Esp32 | Chip::Esp32s3 | Chip::Esp8266 => 0,
        Chip::Esp32c3 => 9,
    }
}

/// USB D-/D+ by chip, when the chip has USB-Serial-JTAG.
const fn usb_pins(chip: Chip) -> Option<(u8, u8)> {
    match chip {
        Chip::Esp32s3 => Some((19, 20)),
        Chip::Esp32c3 => Some((18, 19)),
        Chip::Esp32 | Chip::Esp8266 => None,
    }
}

/// Builds a [`Board`]; see the module docs. Setters record the first error
/// and [`build`](BoardBuilder::build) returns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a builder does nothing until `build`"]
pub struct BoardBuilder {
    board: Board,
    error: Option<BoardError>,
}

impl BoardBuilder {
    const fn fail(mut self, e: BoardError) -> Self {
        if self.error.is_none() {
            self.error = Some(e);
        }
        self
    }

    const fn shell_pin(&self, pin: u8) -> bool {
        let p = &self.board.profile;
        if pin == p.uart0_tx || pin == p.uart0_rx {
            return true;
        }
        match usb_pins(p.chip) {
            Some((dm, dp)) => pin == dm || pin == dp,
            None => false,
        }
    }

    /// Sets the chip name `sys/info` reports (1 to [`MAX_NAME`] bytes).
    pub const fn name(mut self, name: &'static str) -> Self {
        self.board.profile.name = name;
        self
    }

    /// Keeps `pins` from payloads, on top of what the chip reserves.
    pub const fn reserve(mut self, pins: &[u8]) -> Self {
        let mut i = 0;
        while i < pins.len() {
            let pin = pins[i];
            if !self.board.profile.exists(pin) {
                return self.fail(BoardError::NoSuchPin(pin));
            }
            self.board.profile = self.board.profile.with_reserved(pin, true);
            i += 1;
        }
        self
    }

    /// Opens `pins`, which the chip profile reserves only for module flash
    /// or PSRAM the board does not have ([`Profile::is_releasable`]), to
    /// payloads. A pin the shell owns or that is not releasable is an
    /// error; releasing a pin that is not reserved changes nothing.
    pub const fn release(mut self, pins: &[u8]) -> Self {
        let mut i = 0;
        while i < pins.len() {
            let pin = pins[i];
            if !self.board.profile.exists(pin) {
                return self.fail(BoardError::NoSuchPin(pin));
            }
            if self.shell_pin(pin) {
                return self.fail(BoardError::ShellPin(pin));
            }
            if self.board.profile.is_reserved(pin) && !self.board.profile.is_releasable(pin) {
                return self.fail(BoardError::NotReleasable(pin));
            }
            self.board.profile = self.board.profile.with_reserved(pin, false);
            i += 1;
        }
        self
    }

    /// Sets the presence (re-key) button's GPIO. It must be a payload pin:
    /// the shell reads it through the payload pin bank.
    pub const fn presence(mut self, pin: u8) -> Self {
        self.board.presence = pin;
        self
    }

    /// Sets the safe-mode pin: held low at boot, the stored payload is not
    /// autostarted. It must be a payload pin.
    pub const fn safe_mode(mut self, pin: u8) -> Self {
        self.board.safe_mode = Some(pin);
        self
    }

    /// Sets the largest payload image accepted, in bytes.
    pub const fn max_image(mut self, bytes: u32) -> Self {
        self.board.profile.max_image = bytes;
        self
    }

    /// Sets the fuel each call into the payload may burn.
    pub const fn fuel_per_call(mut self, fuel: u64) -> Self {
        self.board.fuel_per_call = fuel;
        self
    }

    /// The board, or the first problem found (see [`BoardError`]).
    pub const fn build(self) -> Result<Board, BoardError> {
        if let Some(e) = self.error {
            return Err(e);
        }
        let b = self.board;
        let p = &b.profile;
        if p.name.is_empty() || p.name.len() > MAX_NAME {
            return Err(BoardError::Name);
        }
        if b.firmware.is_empty() || b.firmware.len() > MAX_NAME {
            return Err(BoardError::Firmware);
        }
        if !payload_pin(p, b.presence) {
            return Err(BoardError::PresencePin(b.presence));
        }
        if let Some(pin) = b.safe_mode
            && !payload_pin(p, pin)
        {
            return Err(BoardError::SafeModePin(pin));
        }
        if !image_fits(p) {
            return Err(BoardError::MaxImage(p.max_image));
        }
        if b.fuel_per_call == 0 {
            return Err(BoardError::Fuel);
        }
        Ok(b)
    }
}

const fn payload_pin(p: &Profile, pin: u8) -> bool {
    p.exists(pin) && !p.is_reserved(pin)
}

/// The image cap fits one payload slot and, with the runtime, the heap
/// plan (linear memory, image, interpreter reserve and radio budget).
const fn image_fits(p: &Profile) -> bool {
    let slot = Region::new(PAYLOAD_PARTITION_BYTES).capacity();
    if p.max_image == 0 || p.max_image > slot {
        return false;
    }
    !p.runtime
        || p.max_memory_bytes as usize + p.max_image as usize + INTERPRETER_RESERVE + p.radio_heap
            <= p.heap_total()
}

/// One boot-time sample of the safe-mode pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafeMode {
    /// The board has no safe-mode pin.
    NoPin,
    /// The pin read high (pull-up, not held).
    Released,
    /// The pin read low: safe mode.
    Held,
    /// The pin could not be read.
    Unreadable,
}

impl SafeMode {
    /// From a pin sample: `Some(true)` reads low (held), `Some(false)`
    /// high, `None` unreadable.
    pub const fn from_sample(low: Option<bool>) -> SafeMode {
        match low {
            Some(true) => SafeMode::Held,
            Some(false) => SafeMode::Released,
            None => SafeMode::Unreadable,
        }
    }
}

/// What the controller does with the stored payload at boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Autostart {
    /// Start it.
    Start,
    /// Keep it installed but stopped: the safe-mode pin was held.
    SafeMode,
    /// Nothing to start (no valid image).
    Nothing,
}

/// The boot decision: start the stored payload if it is `startable`
/// (`dispatch::State::startable`), unless the safe-mode pin was held.
pub const fn autostart(startable: bool, safe: SafeMode) -> Autostart {
    match (startable, safe) {
        (false, _) => Autostart::Nothing,
        (true, SafeMode::Held) => Autostart::SafeMode,
        (true, SafeMode::NoPin | SafeMode::Released | SafeMode::Unreadable) => Autostart::Start,
    }
}

/// The board to run when the compiled shell is for `compiled`: `board`
/// itself, or `Err` with `board`'s chip when it was built from another
/// chip's profile (the shell's heap regions and pin lists are the
/// compiled chip's).
pub const fn for_chip(board: &Board, compiled: Chip) -> Result<&Board, Chip> {
    let chip = board.profile.chip;
    if chip as u8 == compiled as u8 {
        Ok(board)
    } else {
        Err(chip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chip::{Direction, ESP32, ESP32C3, ESP32S3, ESP8266};
    use middleware::wasm_abi::AbiError;

    const ALL: [&Profile; 4] = [&ESP32, &ESP32S3, &ESP32C3, &ESP8266];

    fn ok(b: BoardBuilder) -> Board {
        b.build().expect("valid board")
    }

    #[test]
    fn e25_chip_default_reproduces_every_chip_profile() {
        for p in ALL {
            let b = Board::chip_default(p, "0.5.0").expect("default");
            assert_eq!(b.profile(), p, "{}", p.name);
            assert_eq!(b.firmware(), "0.5.0");
            assert_eq!(b.safe_mode(), None);
            assert_eq!(b.fuel_per_call(), FUEL_PER_CALL);
            for pin in 0..=64 {
                for dir in [Direction::Input, Direction::Output] {
                    assert_eq!(
                        b.profile().check_pin(pin, dir),
                        p.check_pin(pin, dir),
                        "{} {pin}",
                        p.name
                    );
                }
            }
        }
        let presence = ALL.map(|p| Board::chip_default(p, "x").expect("default").presence());
        assert_eq!(presence, [0, 0, 9, 0]);
    }

    #[test]
    fn e25_board_names_itself_and_reports_the_callers_firmware() {
        let b = ok(Board::builder(&ESP32S3, "0.4.0").name("esp32s3-conagotchi"));
        assert_eq!(b.profile().name, "esp32s3-conagotchi");
        assert_eq!(b.firmware(), "0.4.0");
        assert_eq!(b.profile().chip, Chip::Esp32s3);
        assert_eq!(
            Board::builder(&ESP32S3, "x").name("").build(),
            Err(BoardError::Name)
        );
        let long = "a-name-that-is-longer-than-thirty-two-bytes";
        assert_eq!(
            Board::builder(&ESP32S3, "x").name(long).build(),
            Err(BoardError::Name)
        );
        assert_eq!(
            Board::builder(&ESP32S3, long).build(),
            Err(BoardError::Firmware)
        );
        assert_eq!(Board::chip_default(&ESP32S3, ""), Err(BoardError::Firmware));
    }

    #[test]
    fn e25_reserve_keeps_pins_from_payloads() {
        let b = ok(Board::builder(&ESP32S3, "x").reserve(&[41, 42]));
        assert_eq!(
            b.profile().check_pin(41, Direction::Output),
            Err(AbiError::Unsupported)
        );
        assert_eq!(
            b.profile().check_pin(42, Direction::Input),
            Err(AbiError::Unsupported)
        );
        assert_eq!(b.profile().check_pin(40, Direction::Output), Ok(()));
        assert_eq!(
            b.profile().payload_pins().count(),
            ESP32S3.payload_pins().count() - 2
        );
        assert_eq!(
            Board::builder(&ESP32S3, "x").reserve(&[22]).build(),
            Err(BoardError::NoSuchPin(22))
        );
    }

    #[test]
    fn e25_release_opens_module_psram_pins_only() {
        let b = ok(Board::builder(&ESP32S3, "x").release(&[33, 34, 35, 36, 37]));
        for pin in 33..=37 {
            assert_eq!(
                b.profile().check_pin(pin, Direction::Output),
                Ok(()),
                "{pin}"
            );
        }
        for pin in 26..=32 {
            assert_eq!(
                Board::builder(&ESP32S3, "x").release(&[pin]).build(),
                Err(BoardError::NotReleasable(pin)),
                "flash {pin}"
            );
        }
        for pin in [19, 20, 43, 44] {
            assert_eq!(
                Board::builder(&ESP32S3, "x").release(&[pin]).build(),
                Err(BoardError::ShellPin(pin))
            );
        }
        let esp32 = ok(Board::builder(&ESP32, "x").release(&[16, 17]));
        assert_eq!(esp32.profile().check_pin(16, Direction::Output), Ok(()));
        assert_eq!(
            Board::builder(&ESP32, "x").release(&[6]).build(),
            Err(BoardError::NotReleasable(6))
        );
        assert_eq!(
            Board::builder(&ESP32, "x").release(&[1]).build(),
            Err(BoardError::ShellPin(1))
        );
        for pin in [18, 19, 20, 21] {
            assert_eq!(
                Board::builder(&ESP32C3, "x").release(&[pin]).build(),
                Err(BoardError::ShellPin(pin))
            );
        }
        assert_eq!(
            Board::builder(&ESP32C3, "x").release(&[12]).build(),
            Err(BoardError::NotReleasable(12))
        );
        // Releasing a payload pin is a no-op; a missing pin is an error.
        assert_eq!(
            ok(Board::builder(&ESP32S3, "x").release(&[4])).profile(),
            &ESP32S3
        );
        assert_eq!(
            Board::builder(&ESP32S3, "x").release(&[49]).build(),
            Err(BoardError::NoSuchPin(49))
        );
        // A board can reserve a pin again after releasing it.
        let again = ok(Board::builder(&ESP32S3, "x").release(&[33]).reserve(&[33]));
        assert!(again.profile().is_reserved(33));
    }

    #[test]
    fn e25_presence_and_safe_mode_must_be_payload_pins() {
        let b = ok(Board::builder(&ESP32S3, "x").presence(0).safe_mode(21));
        assert_eq!((b.presence(), b.safe_mode()), (0, Some(21)));
        assert_eq!(
            Board::builder(&ESP32S3, "x").safe_mode(26).build(),
            Err(BoardError::SafeModePin(26))
        );
        assert_eq!(
            Board::builder(&ESP32S3, "x").safe_mode(22).build(),
            Err(BoardError::SafeModePin(22))
        );
        assert_eq!(
            Board::builder(&ESP32S3, "x").presence(43).build(),
            Err(BoardError::PresencePin(43))
        );
        assert_eq!(
            Board::builder(&ESP32S3, "x").reserve(&[0]).build(),
            Err(BoardError::PresencePin(0))
        );
        assert_eq!(
            Board::builder(&ESP32S3, "x")
                .safe_mode(21)
                .reserve(&[21])
                .build(),
            Err(BoardError::SafeModePin(21))
        );
        // A released pin may carry safe mode.
        assert_eq!(
            ok(Board::builder(&ESP32S3, "x").release(&[35]).safe_mode(35)).safe_mode(),
            Some(35)
        );
    }

    #[test]
    fn e25_max_image_fits_a_slot_and_the_heap() {
        assert_eq!(
            ok(Board::builder(&ESP32S3, "x").max_image(16 * 1024))
                .profile()
                .max_image,
            16 * 1024
        );
        assert_eq!(
            Board::builder(&ESP32S3, "x").max_image(0).build(),
            Err(BoardError::MaxImage(0))
        );
        // The ESP32-S3 heap leaves about 72 KiB above memory, reserve and radio.
        let big = 100 * 1024;
        assert_eq!(
            Board::builder(&ESP32S3, "x").max_image(big).build(),
            Err(BoardError::MaxImage(big))
        );
        // Without the runtime only the slot bounds it.
        assert_eq!(
            ok(Board::builder(&ESP8266, "x").max_image(124 * 1024))
                .profile()
                .max_image,
            124 * 1024
        );
        let over = 124 * 1024 + 1;
        assert_eq!(
            Board::builder(&ESP8266, "x").max_image(over).build(),
            Err(BoardError::MaxImage(over))
        );
    }

    #[test]
    fn e27_fuel_per_call_is_a_board_field() {
        assert_eq!(
            ok(Board::builder(&ESP32S3, "x").fuel_per_call(50_000)).fuel_per_call(),
            50_000
        );
        assert_eq!(
            Board::builder(&ESP32S3, "x").fuel_per_call(0).build(),
            Err(BoardError::Fuel)
        );
    }

    #[test]
    fn e25_first_error_wins_and_errors_describe_themselves() {
        let b = Board::builder(&ESP32S3, "x")
            .reserve(&[60])
            .release(&[26])
            .name("");
        assert_eq!(b.build(), Err(BoardError::NoSuchPin(60)));
        extern crate std;
        use std::string::ToString;
        assert_eq!(
            BoardError::NotReleasable(26).to_string(),
            "board: pin is reserved for flash and cannot be released (GPIO26)"
        );
        assert_eq!(BoardError::Fuel.to_string(), BoardError::Fuel.as_str());
    }

    #[test]
    fn e24_board_must_match_the_compiled_chip() {
        let s3 = ok(Board::builder(&ESP32S3, "x"));
        assert_eq!(for_chip(&s3, Chip::Esp32s3), Ok(&s3));
        assert_eq!(for_chip(&s3, Chip::Esp32), Err(Chip::Esp32s3));
    }

    #[test]
    fn e26_safe_mode_held_at_boot_skips_autostart_only() {
        assert_eq!(autostart(true, SafeMode::NoPin), Autostart::Start);
        assert_eq!(autostart(true, SafeMode::Released), Autostart::Start);
        assert_eq!(autostart(true, SafeMode::Held), Autostart::SafeMode);
        assert_eq!(
            autostart(true, SafeMode::Unreadable),
            Autostart::Start,
            "a failed read never blocks"
        );
        for s in [
            SafeMode::NoPin,
            SafeMode::Released,
            SafeMode::Held,
            SafeMode::Unreadable,
        ] {
            assert_eq!(autostart(false, s), Autostart::Nothing);
        }
        assert_eq!(
            SafeMode::from_sample(Some(true)),
            SafeMode::Held,
            "active low"
        );
        assert_eq!(SafeMode::from_sample(Some(false)), SafeMode::Released);
        assert_eq!(SafeMode::from_sample(None), SafeMode::Unreadable);
    }

    #[test]
    fn e26_safe_mode_keeps_the_payload_installed() {
        use crate::dispatch::State;
        use middleware::loader::PayloadStatus;
        let state = State::boot(true);
        assert_eq!(
            autostart(state.startable(), SafeMode::Held),
            Autostart::SafeMode
        );
        // Nothing about the stored payload changes: still Stopped, so
        // sys/payload/start can start it.
        assert_eq!(state.status(), PayloadStatus::Stopped);
        assert_eq!(
            autostart(State::boot(false).startable(), SafeMode::Held),
            Autostart::Nothing
        );
    }
}
