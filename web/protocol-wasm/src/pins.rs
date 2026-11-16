//! The web pin table's checks: a payload's pin rows and bus settings,
//! checked against a board's [`PinMap`] (`board-pins`, the rules both
//! firmwares apply) and the ABI's own argument rules
//! (`middleware::wasm_abi::periph`: [`I2cConfig::parse`],
//! [`SpiConfig::parse`]).
//!
//! The web model (`web/src/model/`) owns the table's shape; this module
//! owns what is allowed in it, so the browser never restates a pin rule.
//! [`check`] returns every problem as an [`Issue`] with a plain-language
//! message and, where one exists, a [`Fix`] the page offers as a button.
//!
//! Per row:
//!
//! * the label: present, `[A-Za-z][A-Za-z0-9_]*` of at most
//!   [`MAX_LABEL`] bytes, and unique (ignoring case) in the table;
//! * the pin: chosen, and on the chip; then by function — a GPIO output
//!   must pass [`PinMap::check`] as an output and an input as an input; an
//!   ADC row must be one of the board's ADC pins; a PWM row one of the
//!   board's PWM outputs; an I2C row must be its bus's board-wired SDA or
//!   SCL ([`PinMap::i2c_pins`]); an SPI row must pass
//!   [`board_pins::spi::check_role`] (direction, and on the RP chips the F1
//!   signal).
//!
//! Per table:
//!
//! * no two rows on one pin;
//! * each I2C bus has exactly one SDA and one SCL row, and its clock passes
//!   [`I2cConfig::parse`];
//! * the SPI bus (bus 0, [`periph::SPI_BUSES`]) has exactly one SCK row,
//!   at least one of MOSI and MISO and at most one of each role; its pins
//!   resolve onto one controller ([`board_pins::spi::route`]); and the
//!   whole request passes [`SpiConfig::parse`] (clock and mode).
//!
//! Everything here is pure; `crate::bindings` converts JS rows to [`Row`]
//! and [`Issue`]s back to JS objects.

use board_pins::rp::{SpiBlock, SpiSignal};
use board_pins::spi::{self, SpiPins, SpiProblem, SpiRole};
use board_pins::{AdcError, Direction, PinError, PinMap};
use middleware::wasm_abi::periph::{self, I2cConfig, SpiConfig};

/// Longest label, in bytes.
pub const MAX_LABEL: usize = 24;

/// What a row's pin does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Func {
    /// `gpio_output`.
    GpioOut,
    /// `gpio_input`.
    GpioIn,
    /// `adc_read`.
    Adc,
    /// A board PWM output.
    Pwm,
    /// I2C data.
    I2cSda,
    /// I2C clock.
    I2cScl,
    /// SPI clock.
    SpiSck,
    /// SPI controller out.
    SpiMosi,
    /// SPI controller in.
    SpiMiso,
    /// SPI chip select.
    SpiCs,
}

impl Func {
    /// Every function, in the order the page lists them.
    pub const ALL: [Func; 10] = [
        Func::GpioOut,
        Func::GpioIn,
        Func::Adc,
        Func::Pwm,
        Func::I2cSda,
        Func::I2cScl,
        Func::SpiSck,
        Func::SpiMosi,
        Func::SpiMiso,
        Func::SpiCs,
    ];

    /// The model's name for the function (`"gpio-out"`, `"i2c-sda"`, ...).
    pub const fn name(self) -> &'static str {
        match self {
            Func::GpioOut => "gpio-out",
            Func::GpioIn => "gpio-in",
            Func::Adc => "adc",
            Func::Pwm => "pwm",
            Func::I2cSda => "i2c-sda",
            Func::I2cScl => "i2c-scl",
            Func::SpiSck => "spi-sck",
            Func::SpiMosi => "spi-mosi",
            Func::SpiMiso => "spi-miso",
            Func::SpiCs => "spi-cs",
        }
    }

    /// The function a model name stands for.
    pub fn parse(name: &str) -> Option<Func> {
        Func::ALL.into_iter().find(|f| f.name() == name)
    }

    /// The SPI role, for an SPI row.
    pub const fn spi_role(self) -> Option<SpiRole> {
        match self {
            Func::SpiSck => Some(SpiRole::Sck),
            Func::SpiMosi => Some(SpiRole::Mosi),
            Func::SpiMiso => Some(SpiRole::Miso),
            Func::SpiCs => Some(SpiRole::Cs),
            _ => None,
        }
    }

    /// Whether the row belongs to a bus (I2C or SPI), so its `bus` counts.
    pub const fn on_bus(self) -> bool {
        matches!(self, Func::I2cSda | Func::I2cScl) || self.spi_role().is_some()
    }
}

/// One row of the pin table, as the page sends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The name blocks and flows use for the pin.
    pub label: String,
    /// The GPIO, or `None` when not chosen yet.
    pub pin: Option<i64>,
    /// The function, or `Err(name)` for a name this module does not know.
    pub func: Result<Func, String>,
    /// The bus number for I2C and SPI rows.
    pub bus: i64,
}

/// A bus's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusSetting {
    /// `i2c_open(bus, hz)`.
    I2c {
        /// Bus number.
        bus: i64,
        /// Clock in Hz.
        hz: i64,
    },
    /// `spi_open(bus, .., khz, mode)`.
    Spi {
        /// Bus number.
        bus: i64,
        /// Clock in kHz.
        khz: i64,
        /// SPI mode 0–3.
        mode: i64,
    },
}

/// The part of a row an issue is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// The label.
    Label,
    /// The pin.
    Pin,
    /// The function.
    Func,
    /// The bus number.
    Bus,
    /// A bus setting (clock, mode).
    Setting,
    /// The table as a whole.
    Table,
}

impl Field {
    /// The name the page uses.
    pub const fn name(self) -> &'static str {
        match self {
            Field::Label => "label",
            Field::Pin => "pin",
            Field::Func => "func",
            Field::Bus => "bus",
            Field::Setting => "setting",
            Field::Table => "table",
        }
    }
}

/// A change that resolves an issue, offered as a button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fix {
    /// Set the row's pin.
    Pin(u8),
    /// Add a row with this function on this bus (and pin, when the board
    /// fixes it).
    AddRow {
        /// The function of the new row.
        func: Func,
        /// Its bus.
        bus: u8,
        /// Its pin, when the board decides it.
        pin: Option<u8>,
    },
}

/// One problem with the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// The row (index into the rows given), or `None` for the table or a
    /// bus.
    pub row: Option<usize>,
    /// What part is wrong.
    pub field: Field,
    /// What is wrong and what to do, in plain language.
    pub message: String,
    /// A one-click fix, when there is an obvious one.
    pub fix: Option<Fix>,
}

fn issue(row: Option<usize>, field: Field, message: String, fix: Option<Fix>) -> Issue {
    Issue {
        row,
        field,
        message,
        fix,
    }
}

/// Whether `label` is a valid label (see the module docs).
pub fn label_ok(label: &str) -> bool {
    let mut chars = label.chars();
    label.len() <= MAX_LABEL
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// "GPIO 2, 6 and 18" style list.
fn gpio_list(pins: &[u8]) -> String {
    let text: Vec<String> = pins.iter().map(u8::to_string).collect();
    match text.as_slice() {
        [] => String::new(),
        [one] => format!("GPIO {one}"),
        [rest @ .., last] => format!("GPIO {} and {last}", rest.join(", ")),
    }
}

fn line_name(f: Func) -> &'static str {
    if f == Func::I2cSda { "SDA" } else { "SCL" }
}

fn role_name(r: SpiRole) -> &'static str {
    match r {
        SpiRole::Sck => "SCK",
        SpiRole::Mosi => "MOSI",
        SpiRole::Miso => "MISO",
        SpiRole::Cs => "CS",
    }
}

fn block_name(b: SpiBlock) -> &'static str {
    match b {
        SpiBlock::Spi0 => "SPI0",
        SpiBlock::Spi1 => "SPI1",
    }
}

/// The message for a pin the board policy refuses.
fn pin_error_message(map: &PinMap, pin: u8, e: PinError, wanted: &str) -> String {
    let why = map
        .reason(pin)
        .map(|r| format!(" ({r})"))
        .unwrap_or_default();
    match e {
        PinError::NoSuchPin => format!("This board has no GPIO {pin}."),
        PinError::Reserved => {
            format!("GPIO {pin} is reserved on this board{why}; choose another pin.")
        }
        PinError::InputOnly => format!(
            "GPIO {pin} can only be read{why}, so it cannot be {wanted}; choose another pin."
        ),
    }
}

/// Pins that can take `role` on this board (any SPI block on the RP chips).
fn spi_candidates(map: &PinMap, role: SpiRole) -> Vec<u8> {
    map.payload_pins()
        .filter(|&p| spi::check_role(map, role, p, None).is_ok())
        .collect()
}

/// The pin as a GPIO number the chip could have (0..=63), else an issue.
fn pin_number(row: usize, pin: Option<i64>) -> Result<Option<u8>, Issue> {
    match pin {
        None => Ok(None),
        Some(p) => match u8::try_from(p) {
            Ok(p) if p < 64 => Ok(Some(p)),
            _ => Err(issue(
                Some(row),
                Field::Pin,
                format!("This board has no GPIO {p}."),
                None,
            )),
        },
    }
}

/// Checks one row on its own (label syntax aside); returns its issues.
fn check_row(map: &PinMap, i: usize, row: &Row, func: Func, taken: &[Option<u8>]) -> Vec<Issue> {
    let mut out = Vec::new();
    let pin = match pin_number(i, row.pin) {
        Ok(p) => p,
        Err(e) => return vec![e],
    };
    let unused = |p: u8| !taken.contains(&Some(p));
    let first_unused = |cands: &[u8]| cands.iter().copied().find(|&p| unused(p));
    match func {
        Func::GpioOut | Func::GpioIn => {
            let Some(p) = pin else {
                out.push(issue(Some(i), Field::Pin, "Choose a pin.".into(), None));
                return out;
            };
            let (dir, wanted) = if func == Func::GpioOut {
                (Direction::Output, "an output")
            } else {
                (Direction::Input, "an input")
            };
            if let Err(e) = map.check(p, dir) {
                out.push(issue(
                    Some(i),
                    Field::Pin,
                    pin_error_message(map, p, e, wanted),
                    None,
                ));
            }
        }
        Func::Adc => {
            let adc = map.adc_pins();
            if adc.is_empty() {
                out.push(issue(
                    Some(i),
                    Field::Func,
                    "This board's firmware gives payloads no analog inputs.".into(),
                    None,
                ));
                return out;
            }
            let Some(p) = pin else {
                let msg = format!("Choose a pin: the analog inputs are {}.", gpio_list(adc));
                out.push(issue(
                    Some(i),
                    Field::Pin,
                    msg,
                    first_unused(adc).map(Fix::Pin),
                ));
                return out;
            };
            match map.adc_channel(p) {
                Ok(_) => {}
                Err(AdcError::NoSuchPin) => out.push(issue(
                    Some(i),
                    Field::Pin,
                    format!("This board has no GPIO {p}."),
                    None,
                )),
                Err(AdcError::NotAnalog) => {
                    let msg = format!(
                        "GPIO {p} has no analog input; the analog inputs are {}.",
                        gpio_list(adc)
                    );
                    out.push(issue(
                        Some(i),
                        Field::Pin,
                        msg,
                        first_unused(adc).map(Fix::Pin),
                    ));
                }
            }
        }
        Func::Pwm => {
            let pwm: Vec<u8> = map.pwm_pins().map(|(_, p)| p).collect();
            if pwm.is_empty() {
                out.push(issue(
                    Some(i),
                    Field::Func,
                    "This board's firmware wires no PWM outputs for payloads.".into(),
                    None,
                ));
                return out;
            }
            match pin {
                Some(p) if pwm.contains(&p) => {}
                Some(p) => {
                    let msg = format!(
                        "GPIO {p} is not a PWM output on this board; the PWM outputs are {}.",
                        gpio_list(&pwm)
                    );
                    out.push(issue(
                        Some(i),
                        Field::Pin,
                        msg,
                        first_unused(&pwm).map(Fix::Pin),
                    ));
                }
                None => {
                    let msg = format!("Choose a pin: the PWM outputs are {}.", gpio_list(&pwm));
                    out.push(issue(
                        Some(i),
                        Field::Pin,
                        msg,
                        first_unused(&pwm).map(Fix::Pin),
                    ));
                }
            }
        }
        Func::I2cSda | Func::I2cScl => {
            let line = line_name(func);
            let Some(bus) = u8::try_from(row.bus)
                .ok()
                .filter(|b| i32::from(*b) < periph::I2C_BUSES)
            else {
                out.push(issue(
                    Some(i),
                    Field::Bus,
                    "The I2C bus must be 0 or 1.".into(),
                    None,
                ));
                return out;
            };
            let Some((sda, scl)) = map.i2c_pins(bus) else {
                let other = (0..2u8).find(|&b| map.i2c_pins(b).is_some());
                let msg = match other {
                    Some(b) => format!("This board does not wire I2C bus {bus}; use bus {b}."),
                    None => "This board's firmware wires no I2C bus for payloads.".into(),
                };
                out.push(issue(Some(i), Field::Bus, msg, None));
                return out;
            };
            let want = if func == Func::I2cSda { sda } else { scl };
            match pin {
                Some(p) if p == want => {}
                Some(_) | None => {
                    let msg =
                        format!("I2C bus {bus} {line} is fixed at GPIO {want} on this board.");
                    out.push(issue(Some(i), Field::Pin, msg, Some(Fix::Pin(want))));
                }
            }
        }
        Func::SpiSck | Func::SpiMosi | Func::SpiMiso | Func::SpiCs => {
            let Some(role) = func.spi_role() else {
                return out;
            };
            if !(0..i64::from(periph::SPI_BUSES)).contains(&row.bus) {
                out.push(issue(
                    Some(i),
                    Field::Bus,
                    "Payloads have one SPI bus: bus 0.".into(),
                    None,
                ));
                return out;
            }
            let name = role_name(role);
            let Some(p) = pin else {
                out.push(issue(
                    Some(i),
                    Field::Pin,
                    format!("Choose a pin for SPI {name}."),
                    None,
                ));
                return out;
            };
            match spi::check_role(map, role, p, None) {
                Ok(_) => {}
                Err(e) => match e.problem {
                    SpiProblem::Pin(pe) => {
                        let wanted = format!("SPI {name}");
                        out.push(issue(
                            Some(i),
                            Field::Pin,
                            pin_error_message(map, p, pe, &wanted),
                            None,
                        ));
                    }
                    SpiProblem::NoSignal | SpiProblem::OtherBlock { .. } => {
                        let msg = format!(
                            "GPIO {p} cannot carry SPI {name}; on this board {name} can be {}.",
                            gpio_list(&spi_candidates(map, role))
                        );
                        out.push(issue(Some(i), Field::Pin, msg, None));
                    }
                },
            }
        }
    }
    out
}

/// The pins of the SPI bus and the row of each, when every row of it is
/// otherwise fine.
struct SpiBus {
    sck: Option<(usize, u8)>,
    mosi: Option<(usize, u8)>,
    miso: Option<(usize, u8)>,
    cs: Option<(usize, u8)>,
}

/// Checks `rows` and `settings` on `map`; see the module docs.
pub fn check(map: &PinMap, rows: &[Row], settings: &[BusSetting]) -> Vec<Issue> {
    let mut out = Vec::new();
    // Pins as the rows give them (valid numbers only), for duplicates and
    // fix suggestions.
    let pins: Vec<Option<u8>> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| pin_number(i, r.pin).ok().flatten())
        .collect();

    for (i, row) in rows.iter().enumerate() {
        // Label.
        let label = row.label.as_str();
        if label.is_empty() {
            out.push(issue(
                Some(i),
                Field::Label,
                "Give this pin a label, for example LED.".into(),
                None,
            ));
        } else if !label_ok(label) {
            let msg = format!(
                "A label starts with a letter and has only letters, digits and _ (at most {MAX_LABEL})."
            );
            out.push(issue(Some(i), Field::Label, msg, None));
        } else if let Some(j) = rows
            .iter()
            .enumerate()
            .position(|(j, r)| j != i && r.label.eq_ignore_ascii_case(label))
        {
            let msg = format!(
                "The label {label} is also used on row {}; labels must be unique.",
                j + 1
            );
            out.push(issue(Some(i), Field::Label, msg, None));
        }
        // Function and pin.
        let func = match &row.func {
            Ok(f) => *f,
            Err(name) => {
                out.push(issue(
                    Some(i),
                    Field::Func,
                    format!("Unknown function {name:?}."),
                    None,
                ));
                continue;
            }
        };
        let row_issues = check_row(map, i, row, func, &pins);
        let pin_ok = row_issues.iter().all(|x| x.field != Field::Pin);
        out.extend(row_issues);
        // Duplicate pins (only once the pin itself is valid).
        if let (true, Some(p)) = (pin_ok, pins.get(i).copied().flatten())
            && let Some(j) = pins
                .iter()
                .enumerate()
                .position(|(j, q)| j != i && *q == Some(p))
        {
            let other = rows
                .get(j)
                .map(|r| r.label.as_str())
                .filter(|l| !l.is_empty())
                .unwrap_or("another row");
            let msg = format!(
                "GPIO {p} is also used by {other} (row {}); a pin can do one thing.",
                j + 1
            );
            out.push(issue(Some(i), Field::Pin, msg, None));
        }
    }

    check_i2c(map, rows, settings, &mut out);
    check_spi(map, rows, &pins, settings, &mut out);
    out
}

/// Rows of `func` on `bus` (valid functions only).
fn rows_with(rows: &[Row], func: Func, bus: i64) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, r)| r.func == Ok(func) && r.bus == bus)
        .map(|(i, _)| i)
        .collect()
}

fn check_i2c(map: &PinMap, rows: &[Row], settings: &[BusSetting], out: &mut Vec<Issue>) {
    for bus in 0..i64::from(periph::I2C_BUSES) {
        let sda = rows_with(rows, Func::I2cSda, bus);
        let scl = rows_with(rows, Func::I2cScl, bus);
        if sda.is_empty() && scl.is_empty() {
            continue;
        }
        let wired = u8::try_from(bus).ok().and_then(|b| map.i2c_pins(b));
        for (list, func) in [(&sda, Func::I2cSda), (&scl, Func::I2cScl)] {
            let line = line_name(func);
            if list.len() > 1 {
                let msg = format!("I2C bus {bus} has {} {line} rows; keep one.", list.len());
                out.push(issue(None, Field::Table, msg, None));
            }
            if list.is_empty() {
                let pin = wired.map(|(d, c)| if func == Func::I2cSda { d } else { c });
                let at = pin
                    .map(|p| format!(" (GPIO {p} on this board)"))
                    .unwrap_or_default();
                let msg = format!("I2C bus {bus} needs an {line} row too{at}.");
                let fix = u8::try_from(bus)
                    .ok()
                    .map(|b| Fix::AddRow { func, bus: b, pin });
                out.push(issue(None, Field::Table, msg, fix));
            }
        }
        let hz = settings.iter().find_map(|s| match s {
            BusSetting::I2c { bus: b, hz } if *b == bus => Some(*hz),
            _ => None,
        });
        match hz {
            None => out.push(issue(
                None,
                Field::Setting,
                format!("Set the clock of I2C bus {bus}."),
                None,
            )),
            Some(hz) => {
                let valid = i32::try_from(hz)
                    .ok()
                    .is_some_and(|hz| I2cConfig::parse(bus as i32, hz).is_ok());
                if !valid {
                    let (lo, hi) = (periph::I2C_FREQ_HZ.start(), periph::I2C_FREQ_HZ.end());
                    let msg = format!("The I2C bus {bus} clock must be {lo} to {hi} Hz.");
                    out.push(issue(None, Field::Setting, msg, None));
                }
            }
        }
    }
}

fn check_spi(
    map: &PinMap,
    rows: &[Row],
    pins: &[Option<u8>],
    settings: &[BusSetting],
    out: &mut Vec<Issue>,
) {
    let bus: i64 = 0;
    let roles = [Func::SpiSck, Func::SpiMosi, Func::SpiMiso, Func::SpiCs];
    let lists: Vec<Vec<usize>> = roles.iter().map(|&f| rows_with(rows, f, bus)).collect();
    if lists.iter().all(Vec::is_empty) {
        return;
    }
    let mut complete = true;
    for (f, list) in roles.iter().zip(&lists) {
        if list.len() > 1 {
            let name = f.spi_role().map(role_name).unwrap_or("");
            out.push(issue(
                None,
                Field::Table,
                format!("SPI bus 0 has {} {name} rows; keep one.", list.len()),
                None,
            ));
            complete = false;
        }
    }
    let [sck, mosi, miso, cs] = [0, 1, 2, 3].map(|k| lists.get(k).cloned().unwrap_or_default());
    if sck.is_empty() {
        out.push(issue(
            None,
            Field::Table,
            "SPI bus 0 needs a clock (SCK) row.".into(),
            Some(Fix::AddRow {
                func: Func::SpiSck,
                bus: 0,
                pin: None,
            }),
        ));
        complete = false;
    }
    if mosi.is_empty() && miso.is_empty() {
        let msg = "SPI bus 0 needs a MOSI or MISO row (data out or in).".to_string();
        out.push(issue(
            None,
            Field::Table,
            msg,
            Some(Fix::AddRow {
                func: Func::SpiMosi,
                bus: 0,
                pin: None,
            }),
        ));
        complete = false;
    }
    let setting = settings.iter().find_map(|s| match s {
        BusSetting::Spi { bus: 0, khz, mode } => Some((*khz, *mode)),
        _ => None,
    });
    let Some((khz, mode)) = setting else {
        out.push(issue(
            None,
            Field::Setting,
            "Set the clock and mode of SPI bus 0.".into(),
            None,
        ));
        return;
    };
    if !i32::try_from(khz).is_ok_and(|k| (1..=periph::MAX_SPI_KHZ).contains(&k)) {
        let msg = format!("The SPI clock must be 1 to {} kHz.", periph::MAX_SPI_KHZ);
        out.push(issue(None, Field::Setting, msg, None));
        complete = false;
    }
    if !(0..=3).contains(&mode) {
        out.push(issue(
            None,
            Field::Setting,
            "The SPI mode must be 0, 1, 2 or 3.".into(),
            None,
        ));
        complete = false;
    }
    // Controller consistency and the ABI's own check need every SPI row's
    // pin to be valid on its own.
    let pin_bad = |i: usize| {
        out.iter()
            .any(|x| x.row == Some(i) && x.field == Field::Pin)
    };
    let one = |l: &[usize]| -> Option<(usize, u8)> {
        l.first()
            .and_then(|&i| pins.get(i).copied().flatten().map(|p| (i, p)))
    };
    let all_rows: Vec<usize> = lists.iter().flatten().copied().collect();
    if !complete || all_rows.iter().any(|&i| pin_bad(i)) {
        return;
    }
    let b = SpiBus {
        sck: one(&sck),
        mosi: one(&mosi),
        miso: one(&miso),
        cs: one(&cs),
    };
    let Some((sck_row, sck_pin)) = b.sck else {
        return;
    };
    let req = SpiPins {
        sck: sck_pin,
        mosi: b.mosi.map(|x| x.1),
        miso: b.miso.map(|x| x.1),
        cs: b.cs.map(|x| x.1),
    };
    if let Err(e) = spi::route(map, &req) {
        let row = [b.sck, b.mosi, b.miso, b.cs]
            .into_iter()
            .flatten()
            .find(|(_, p)| *p == e.pin)
            .map(|(r, _)| r);
        let name = role_name(e.role);
        let (msg, fix) = match e.problem {
            SpiProblem::OtherBlock {
                sck: want,
                pin: got,
            } => {
                let signal = e.role.signal().unwrap_or(SpiSignal::Sck);
                let fix = board_pins::rp::spi_pins(want, signal)
                    .find(|&p| map.check(p, e.role.direction()).is_ok() && !pins.contains(&Some(p)))
                    .map(Fix::Pin);
                let msg = format!(
                    "SCK on GPIO {sck_pin} is {} but {name} on GPIO {} is {}; all SPI pins must be on one controller.",
                    block_name(want),
                    e.pin,
                    block_name(got)
                );
                (msg, fix)
            }
            // Per-row checks already reported these; kept for completeness.
            SpiProblem::NoSignal | SpiProblem::Pin(_) => (
                format!("GPIO {} cannot be SPI {name} on this board.", e.pin),
                None,
            ),
        };
        out.push(issue(row.or(Some(sck_row)), Field::Pin, msg, fix));
        return;
    }
    let arg = |x: Option<(usize, u8)>| x.map_or(-1, |(_, p)| i32::from(p));
    // khz and mode were range checked above, so the casts are exact.
    if SpiConfig::parse(
        0,
        i32::from(sck_pin),
        arg(b.mosi),
        arg(b.miso),
        arg(b.cs),
        khz as i32,
        mode as i32,
    )
    .is_err()
    {
        out.push(issue(
            None,
            Field::Table,
            "The SPI pins must all be different.".into(),
            None,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use board_pins::boards::{ESP32, ESP32C3, ESP32S3, PICO};

    fn row(label: &str, pin: Option<i64>, func: Func, bus: i64) -> Row {
        Row {
            label: label.into(),
            pin,
            func: Ok(func),
            bus,
        }
    }
    fn i2c(bus: i64) -> BusSetting {
        BusSetting::I2c { bus, hz: 100_000 }
    }
    fn spi0() -> BusSetting {
        BusSetting::Spi {
            bus: 0,
            khz: 1000,
            mode: 0,
        }
    }
    fn messages(issues: &[Issue]) -> Vec<String> {
        issues
            .iter()
            .map(|i| format!("{:?} {}: {}", i.row, i.field.name(), i.message))
            .collect()
    }

    #[test]
    fn function_names_round_trip() {
        for f in Func::ALL {
            assert_eq!(Func::parse(f.name()), Some(f));
        }
        assert_eq!(Func::parse("uart"), None);
    }

    #[test]
    fn a_valid_pico_table_has_no_issues() {
        let rows = [
            row("LED", Some(25), Func::GpioOut, 0),
            row("BUTTON", Some(15), Func::GpioIn, 0),
            row("POT", Some(26), Func::Adc, 0),
            row("SDA", Some(4), Func::I2cSda, 0),
            row("SCL", Some(5), Func::I2cScl, 0),
            row("SCK", Some(18), Func::SpiSck, 0),
            row("MOSI", Some(19), Func::SpiMosi, 0),
            row("MISO", Some(16), Func::SpiMiso, 0),
            row("CS", Some(17), Func::SpiCs, 0),
        ];
        assert_eq!(
            messages(&check(&PICO, &rows, &[i2c(0), spi0()])),
            Vec::<String>::new()
        );
    }

    #[test]
    fn labels_must_be_present_valid_and_unique() {
        let rows = [
            row("", Some(2), Func::GpioOut, 0),
            row("9lives", Some(3), Func::GpioOut, 0),
            row("led", Some(4), Func::GpioOut, 0),
            row("LED", Some(5), Func::GpioOut, 0),
        ];
        let issues = check(&PICO, &rows, &[]);
        assert_eq!(issues.len(), 4, "{:?}", messages(&issues));
        assert!(issues.iter().all(|i| i.field == Field::Label));
        assert!(issues[2].message.contains("also used on row 4"));
        assert!(issues[3].message.contains("also used on row 3"));
        assert!(label_ok("A_1") && !label_ok("a-b") && !label_ok(&"x".repeat(25)));
    }

    #[test]
    fn duplicate_pins_flag_both_rows() {
        let rows = [
            row("LED", Some(25), Func::GpioOut, 0),
            row("LAMP", Some(25), Func::GpioOut, 0),
        ];
        let issues = check(&PICO, &rows, &[]);
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].row, Some(0));
        assert!(issues[0].message.contains("also used by LAMP (row 2)"));
        assert!(issues[1].message.contains("also used by LED (row 1)"));
    }

    #[test]
    fn reserved_input_only_and_missing_pins_say_why() {
        let rows = [
            row("A", Some(23), Func::GpioOut, 0),
            row("B", Some(24), Func::GpioOut, 0),
            row("C", Some(24), Func::GpioIn, 0),
            row("D", Some(40), Func::GpioIn, 0),
            row("E", None, Func::GpioOut, 0),
            row("F", Some(-3), Func::GpioOut, 0),
        ];
        let m = messages(&check(&PICO, &rows, &[]));
        let has = |s: &str| m.iter().any(|x| x.contains(s));
        assert!(
            has(
                "Some(0) pin: GPIO 23 is reserved on this board (the regulator's power-save control)"
            ),
            "{m:?}"
        );
        assert!(
            has(
                "Some(1) pin: GPIO 24 can only be read (VBUS sense (input only)), so it cannot be an output"
            ),
            "{m:?}"
        );
        // C is a fine input, but shares GPIO 24 with B.
        assert!(
            has("Some(2) pin: GPIO 24 is also used by B (row 2)"),
            "{m:?}"
        );
        assert!(has("Some(3) pin: This board has no GPIO 40"), "{m:?}");
        assert!(has("Some(4) pin: Choose a pin"), "{m:?}");
        assert!(has("Some(5) pin: This board has no GPIO -3"), "{m:?}");
        assert_eq!(m.len(), 6, "{m:?}");
        let esp = check(&ESP32, &[row("LED", Some(8), Func::GpioOut, 0)], &[]);
        assert_eq!(
            esp[0].message,
            "GPIO 8 is reserved on this board (the SPI flash); choose another pin."
        );
    }

    #[test]
    fn adc_and_pwm_follow_the_board_map() {
        let pico = check(
            &PICO,
            &[
                row("POT", Some(25), Func::Adc, 0),
                row("FAN", Some(2), Func::Pwm, 0),
            ],
            &[],
        );
        assert_eq!(
            pico[0].message,
            "GPIO 25 has no analog input; the analog inputs are GPIO 26, 27 and 28."
        );
        assert_eq!(pico[0].fix, Some(Fix::Pin(26)));
        assert_eq!(
            pico[1].message,
            "This board's firmware wires no PWM outputs for payloads."
        );
        // The ESP32's analog inputs are ADC1's (GPIO 32-39).
        let esp = check(
            &ESP32,
            &[
                row("POT", Some(32), Func::Adc, 0),
                row("FAN", Some(2), Func::Pwm, 0),
            ],
            &[],
        );
        assert_eq!(esp.len(), 1, "{esp:?}");
        assert_eq!(
            esp[0].message,
            "GPIO 2 is not a PWM output on this board; the PWM outputs are GPIO 0, 4, 13 and 14."
        );
        assert_eq!(esp[0].fix, Some(Fix::Pin(0)));
        let esp = check(&ESP32, &[row("POT", Some(25), Func::Adc, 0)], &[]);
        assert_eq!(esp[0].fix, Some(Fix::Pin(36)), "ADC1 channel 0 first");
        assert!(check(&ESP32, &[row("FLASH", Some(4), Func::Pwm, 0)], &[]).is_empty());
    }

    #[test]
    fn i2c_pins_are_fixed_by_the_board_and_come_in_pairs() {
        let rows = [row("SDA", Some(8), Func::I2cSda, 0)];
        let issues = check(&PICO, &rows, &[i2c(0)]);
        assert_eq!(
            issues[0].message,
            "I2C bus 0 SDA is fixed at GPIO 4 on this board."
        );
        assert_eq!(issues[0].fix, Some(Fix::Pin(4)));
        assert_eq!(
            issues[1].message,
            "I2C bus 0 needs an SCL row too (GPIO 5 on this board)."
        );
        assert_eq!(
            issues[1].fix,
            Some(Fix::AddRow {
                func: Func::I2cScl,
                bus: 0,
                pin: Some(5)
            })
        );
        // The ESP32-S3 wires bus 0 only; the C3 none.
        let s3 = check(
            &ESP32S3,
            &[
                row("SDA", Some(5), Func::I2cSda, 1),
                row("SCL", Some(6), Func::I2cScl, 1),
            ],
            &[i2c(1)],
        );
        assert_eq!(
            s3[0].message,
            "This board does not wire I2C bus 1; use bus 0."
        );
        let c3 = check(
            &ESP32C3,
            &[
                row("SDA", Some(5), Func::I2cSda, 0),
                row("SCL", Some(6), Func::I2cScl, 0),
            ],
            &[i2c(0)],
        );
        assert_eq!(
            c3[0].message,
            "This board's firmware wires no I2C bus for payloads."
        );
        // The clock is the ABI's range.
        let slow = check(
            &PICO,
            &[
                row("SDA", Some(4), Func::I2cSda, 0),
                row("SCL", Some(5), Func::I2cScl, 0),
            ],
            &[BusSetting::I2c { bus: 0, hz: 50 }],
        );
        assert_eq!(
            messages(&slow),
            ["None setting: The I2C bus 0 clock must be 10000 to 1000000 Hz."]
        );
        let two = check(
            &PICO,
            &[
                row("A", Some(4), Func::I2cSda, 0),
                row("B", Some(5), Func::I2cScl, 0),
                row("C", Some(4), Func::I2cSda, 0),
            ],
            &[i2c(0)],
        );
        assert!(
            messages(&two)
                .iter()
                .any(|m| m.contains("I2C bus 0 has 2 SDA rows")),
            "{:?}",
            messages(&two)
        );
    }

    #[test]
    fn spi_pins_must_have_the_signal_and_share_a_controller() {
        let rows = [
            row("SCK", Some(19), Func::SpiSck, 0),
            row("MOSI", Some(18), Func::SpiMosi, 0),
        ];
        let m = messages(&check(&PICO, &rows, &[spi0()]));
        assert_eq!(
            m[0],
            "Some(0) pin: GPIO 19 cannot carry SPI SCK; on this board SCK can be GPIO 2, 6, 10, 14, 18 and 22."
        );
        assert!(m[1].contains("GPIO 18 cannot carry SPI MOSI"), "{m:?}");
        let mixed = [
            row("SCK", Some(18), Func::SpiSck, 0),
            row("MOSI", Some(11), Func::SpiMosi, 0),
        ];
        let issues = check(&PICO, &mixed, &[spi0()]);
        assert_eq!(issues.len(), 1, "{:?}", messages(&issues));
        assert_eq!(issues[0].row, Some(1));
        assert_eq!(
            issues[0].message,
            "SCK on GPIO 18 is SPI0 but MOSI on GPIO 11 is SPI1; all SPI pins must be on one controller."
        );
        assert_eq!(issues[0].fix, Some(Fix::Pin(3)));
        // Through the ESP matrix any capable pin works; MISO may be input only.
        let esp = [
            row("SCK", Some(14), Func::SpiSck, 0),
            row("MOSI", Some(13), Func::SpiMosi, 0),
            row("MISO", Some(35), Func::SpiMiso, 0),
        ];
        assert!(check(&ESP32, &esp, &[spi0()]).is_empty());
        let cs_in = [
            row("SCK", Some(14), Func::SpiSck, 0),
            row("MOSI", Some(13), Func::SpiMosi, 0),
            row("CS", Some(35), Func::SpiCs, 0),
        ];
        assert!(
            check(&ESP32, &cs_in, &[spi0()])[0]
                .message
                .contains("cannot be SPI CS")
        );
    }

    #[test]
    fn spi_bus_must_be_complete_and_configured() {
        let only_cs = [row("CS", Some(17), Func::SpiCs, 0)];
        let m = messages(&check(&PICO, &only_cs, &[spi0()]));
        assert_eq!(
            m,
            [
                "None table: SPI bus 0 needs a clock (SCK) row.",
                "None table: SPI bus 0 needs a MOSI or MISO row (data out or in)."
            ]
        );
        let ok = [
            row("SCK", Some(18), Func::SpiSck, 0),
            row("MOSI", Some(19), Func::SpiMosi, 0),
        ];
        let bad = check(
            &PICO,
            &ok,
            &[BusSetting::Spi {
                bus: 0,
                khz: 50_000,
                mode: 4,
            }],
        );
        assert_eq!(
            messages(&bad),
            [
                "None setting: The SPI clock must be 1 to 40000 kHz.",
                "None setting: The SPI mode must be 0, 1, 2 or 3."
            ]
        );
        let bus1 = check(&PICO, &[row("SCK", Some(18), Func::SpiSck, 1)], &[]);
        assert_eq!(bus1[0].message, "Payloads have one SPI bus: bus 0.");
        assert!(
            check(&PICO, &ok, &[])
                .iter()
                .any(|i| i.message == "Set the clock and mode of SPI bus 0.")
        );
    }

    #[test]
    fn unknown_functions_are_reported() {
        let rows = [Row {
            label: "X".into(),
            pin: Some(2),
            func: Err("uart-tx".into()),
            bus: 0,
        }];
        assert_eq!(
            check(&PICO, &rows, &[])[0].message,
            "Unknown function \"uart-tx\"."
        );
    }
}
