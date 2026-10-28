//! GC9A01 panel bring-up for payloads on the conagotchi badge (ESP32-S3):
//! the wiring, the `mipidsi`-derived init [`PROGRAM`], the pure
//! [`Sequencer`] that steps through it one DMA transfer or timer delay at a
//! time, and the [`Controller`] that repeats a failed bring-up after
//! [`RETRY_US`] and gives up after [`MAX_FAILURES`]. Shared by the
//! `cam_view` (sink route) and `cam_jpeg_view` (JPEG decode in WASM)
//! examples; [`Hw::open`] is the only part that calls the host.
//!
//! **Wiring** (the badge's, `badge_core::board::display`): SPI2 with SCK
//! GPIO [`SCK_PIN`] (12), MOSI [`MOSI_PIN`] (11), CS [`CS_PIN`] (10), D/C
//! [`DC_PIN`] (13), reset [`RST_PIN`] (14, active low); no MISO, and no
//! backlight pin (the badge's backlight is always on). [`SPI_KHZ`]
//! (40 MHz), SPI mode 0.
//!
//! # The init sequence
//!
//! [`PROGRAM`] transcribes `mipidsi` 0.10's `GC9A01` model (`init` in
//! `src/models/gc9a01.rs`) and its `Builder::init` reset, with the options
//! the badge firmware uses (`firmware/src/display.rs`): colour inversion on
//! (`INVON`, 0x21) and BGR subpixel order (`MADCTL` 0x08: no rotation or
//! mirroring, BGR bit), 16-bit pixels (`COLMOD` 0x55). Each command byte
//! goes out with D/C low and its parameters, as a separate transfer, with
//! D/C high. The reset is held for 10 ms (mipidsi: 10 µs; the payload's
//! shortest timer is 100 µs) followed by mipidsi's 200 ms power-up delay,
//! and `SLPOUT` is followed by 120 ms before `DISPON`.
//!
//! The program ends with a full-screen window, `RAMWR` and a black fill:
//! 240 × 240 × 2 zero bytes in [`CLEAR_CHUNK`]-byte transfers (30 of them).
//! Each is `spi_transfer` with no transmit bytes and a receive length of
//! [`CLEAR_CHUNK`]: the ABI clocks `max(tx, rx)` bytes and pads the
//! transmit side with 0x00, so the zeros cost no guest memory. With
//! inversion on, 0x0000 shows as black on this panel (as in the firmware).
//!
//! # Drawing after the bring-up
//!
//! [`window`] gives the `CASET` / `RASET` parameter bytes of a pixel
//! rectangle; send [`CASET`], its parameters, [`RASET`], its parameters and
//! [`RAMWR`] (commands with D/C low, parameters with D/C high), then the
//! RGB565 big-endian pixels with D/C high, row by row.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use mw_payload::AbiError;
use mw_payload::gpio::Output;
use mw_payload::spi::{Mode, Spi, SpiConfig};

/// SPI clock.
pub const SCK_PIN: u8 = 12;
/// Controller out (silkscreen "SDA").
pub const MOSI_PIN: u8 = 11;
/// Hardware chip select.
pub const CS_PIN: u8 = 10;
/// Data/command select: low for a command byte, high for parameters and
/// pixels.
pub const DC_PIN: u8 = 13;
/// Panel reset, active low.
pub const RST_PIN: u8 = 14;
/// SPI clock in kHz (the badge firmware's 40 MHz, the ABI maximum).
pub const SPI_KHZ: u32 = 40_000;
/// Panel width and height in pixels.
pub const PANEL: u16 = 240;
/// Bytes of a full panel of RGB565 pixels.
pub const PANEL_BYTES: u32 = PANEL as u32 * PANEL as u32 * 2;
/// Bytes per black-fill transfer: 8 panel rows.
pub const CLEAR_CHUNK: usize = 8 * PANEL as usize * 2;
const _: () = assert!(CLEAR_CHUNK <= mw_payload::spi::MAX_DMA, "one DMA transfer");
/// Wait before repeating a failed bring-up.
pub const RETRY_US: u32 = 1_000_000;
/// Bring-up failures in a row after which the payload gives up.
pub const MAX_FAILURES: u8 = 5;
/// MIPI-DCS column address set.
pub const CASET: u8 = 0x2A;
/// MIPI-DCS row address set.
pub const RASET: u8 = 0x2B;
/// MIPI-DCS memory write.
pub const RAMWR: u8 = 0x2C;

/// The `CASET` or `RASET` parameter bytes of the inclusive address range
/// `start..=end`: both as big-endian `u16`.
pub const fn window(start: u16, end: u16) -> [u8; 4] {
    let s = start.to_be_bytes();
    let e = end.to_be_bytes();
    [s[0], s[1], e[0], e[1]]
}

/// One step of the panel bring-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Drive the reset pin to `high`, then wait `wait_us`.
    Reset {
        /// Level to drive.
        high: bool,
        /// Wait afterwards, in microseconds.
        wait_us: u32,
    },
    /// Wait `us` microseconds.
    Delay {
        /// The wait.
        us: u32,
    },
    /// One command byte with D/C low.
    Command(u8),
    /// A command's parameter bytes with D/C high.
    Params(&'static [u8]),
    /// [`PANEL_BYTES`] zero bytes with D/C high, [`CLEAR_CHUNK`] per
    /// transfer (black, after a full-screen window and `RAMWR`).
    Clear,
}

use Op::{Command as C, Params as P};

/// The panel bring-up, in order; see "The init sequence" in the module
/// docs.
pub const PROGRAM: &[Op] = &[
    Op::Reset {
        high: false,
        wait_us: 10_000,
    },
    Op::Reset {
        high: true,
        wait_us: 200_000,
    },
    C(0xEF), // inter register enable 2
    C(0xEB),
    P(&[0x14]),
    C(0xFE), // inter register enable 1
    C(0xEF), // inter register enable 2
    C(0xEB),
    P(&[0x14]),
    C(0x84),
    P(&[0x40]),
    C(0x85),
    P(&[0xFF]),
    C(0x86),
    P(&[0xFF]),
    C(0x87),
    P(&[0xFF]),
    C(0x88),
    P(&[0x0A]),
    C(0x89),
    P(&[0x21]),
    C(0x8A),
    P(&[0x00]),
    C(0x8B),
    P(&[0x80]),
    C(0x8C),
    P(&[0x01]),
    C(0x8D),
    P(&[0x01]),
    C(0x8E),
    P(&[0xFF]),
    C(0x8F),
    P(&[0xFF]),
    C(0xB6), // display function control
    P(&[0x00, 0x20]),
    C(0x36), // MADCTL: BGR, no rotation
    P(&[0x08]),
    C(0x3A), // COLMOD: 16 bits per pixel
    P(&[0x55]),
    C(0x90),
    P(&[0x08, 0x08, 0x08, 0x08]),
    C(0xBD),
    P(&[0x06]),
    C(0xBC),
    P(&[0x00]),
    C(0xFF),
    P(&[0x60, 0x01, 0x04]),
    C(0xC3), // power control 2
    P(&[0x13]),
    C(0xC4), // power control 3
    P(&[0x13]),
    C(0xC9), // power control 4
    P(&[0x22]),
    C(0xBE),
    P(&[0x11]),
    C(0xE1),
    P(&[0x10, 0x0E]),
    C(0xDF),
    P(&[0x20, 0x0C, 0x02]),
    C(0xF0), // gamma 1
    P(&[0x45, 0x09, 0x08, 0x08, 0x26, 0x2A]),
    C(0xF1), // gamma 2
    P(&[0x43, 0x70, 0x72, 0x36, 0x37, 0x6F]),
    C(0xF2), // gamma 3
    P(&[0x45, 0x09, 0x08, 0x08, 0x26, 0x2A]),
    C(0xF3), // gamma 4
    P(&[0x43, 0x70, 0x72, 0x36, 0x37, 0x6F]),
    C(0xED),
    P(&[0x18, 0x0B]),
    C(0xAE),
    P(&[0x77]),
    C(0xCD),
    P(&[0x63]),
    C(0x70),
    P(&[0x07, 0x07, 0x04, 0x0E, 0x0F, 0x09, 0x07, 0x08, 0x03]),
    C(0xE8), // frame rate
    P(&[0x34]),
    C(0x62),
    P(&[
        0x18, 0x0D, 0x71, 0xED, 0x70, 0x70, 0x18, 0x0F, 0x71, 0xEF, 0x70, 0x70,
    ]),
    C(0x63),
    P(&[
        0x18, 0x11, 0x71, 0xF1, 0x70, 0x70, 0x18, 0x13, 0x71, 0xF3, 0x70, 0x70,
    ]),
    C(0x64),
    P(&[0x28, 0x29, 0xF1, 0x01, 0xF1, 0x00, 0x07]),
    C(0x66),
    P(&[0x3C, 0x00, 0xCD, 0x67, 0x45, 0x45, 0x10, 0x00, 0x00, 0x00]),
    C(0x67),
    P(&[0x00, 0x3C, 0x00, 0x00, 0x00, 0x01, 0x54, 0x10, 0x32, 0x98]),
    C(0x74),
    P(&[0x10, 0x85, 0x80, 0x00, 0x00, 0x4E, 0x00]),
    C(0x98),
    P(&[0x3E, 0x07]),
    C(0x21), // INVON: colour inversion on
    C(0x11), // SLPOUT
    Op::Delay { us: 120_000 },
    C(0x29), // DISPON
    C(0x2A), // CASET 0..=239
    P(&[0x00, 0x00, 0x00, 0xEF]),
    C(0x2B), // RASET 0..=239
    P(&[0x00, 0x00, 0x00, 0xEF]),
    C(0x2C), // RAMWR
    Op::Clear,
];

/// What the shell must do for the current step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Drive the reset pin, then arm the timer for `wait_us`.
    Reset {
        /// Level to drive.
        high: bool,
        /// Timer delay in microseconds.
        wait_us: u32,
    },
    /// Arm the timer for `us`.
    Delay {
        /// Timer delay in microseconds.
        us: u32,
    },
    /// Set D/C to `dc`, then start a transfer of `bytes`.
    Write {
        /// D/C level: `false` = command, `true` = data.
        dc: bool,
        /// The bytes.
        bytes: &'static [u8],
    },
    /// Set D/C high, then clock out `len` zero bytes (a transfer with no
    /// transmit bytes and `len` receive bytes; the host pads with 0x00).
    Zeros {
        /// Bytes, at most [`CLEAR_CHUNK`].
        len: usize,
    },
}

/// An event the bring-up may be waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// The one-shot timer fired.
    Timer,
    /// The SPI transfer finished.
    Dma(Result<usize, AbiError>),
}

/// Result of feeding the [`Sequencer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Perform this action, then wait for its event.
    Do(Action),
    /// The program completed.
    Finished,
    /// A transfer failed or came up short.
    Failed(AbiError),
    /// The input was not the awaited one; nothing to do.
    Ignored,
}

/// What the sequencer waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Await {
    Timer,
    Dma(usize),
    Nothing,
}

/// Steps through [`PROGRAM`], one action per awaited event. A pure state
/// machine: [`Sequencer::start`] and [`Sequencer::on`] return the next
/// state and what to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sequencer {
    pc: usize,
    cleared: u32,
    awaiting: Await,
}

impl Sequencer {
    /// The first step of [`PROGRAM`].
    pub fn start() -> (Self, Status) {
        Self::emit(0, 0)
    }

    /// The state at `pc` (with `cleared` bytes of a [`Op::Clear`] sent)
    /// and its action.
    fn emit(pc: usize, cleared: u32) -> (Self, Status) {
        let at = |awaiting| Sequencer {
            pc,
            cleared,
            awaiting,
        };
        match PROGRAM.get(pc) {
            None => (at(Await::Nothing), Status::Finished),
            Some(&Op::Reset { high, wait_us }) => (
                at(Await::Timer),
                Status::Do(Action::Reset { high, wait_us }),
            ),
            Some(&Op::Delay { us }) => (at(Await::Timer), Status::Do(Action::Delay { us })),
            Some(Op::Command(cmd)) => (
                at(Await::Dma(1)),
                Status::Do(Action::Write {
                    dc: false,
                    bytes: core::slice::from_ref(cmd),
                }),
            ),
            Some(&Op::Params(bytes)) => (
                at(Await::Dma(bytes.len())),
                Status::Do(Action::Write { dc: true, bytes }),
            ),
            Some(Op::Clear) => {
                let left = PANEL_BYTES.saturating_sub(cleared) as usize;
                let len = left.min(CLEAR_CHUNK);
                (at(Await::Dma(len)), Status::Do(Action::Zeros { len }))
            }
        }
    }

    /// Feeds an event: the next step when it is the awaited one.
    pub fn on(self, input: Input) -> (Self, Status) {
        let done = Sequencer {
            awaiting: Await::Nothing,
            ..self
        };
        match (self.awaiting, input) {
            (Await::Timer, Input::Timer) => Self::emit(self.pc + 1, 0),
            (Await::Dma(_), Input::Dma(Err(e))) => (done, Status::Failed(e)),
            (Await::Dma(want), Input::Dma(Ok(n))) if n == want => {
                if PROGRAM.get(self.pc) == Some(&Op::Clear) {
                    let cleared = self.cleared.saturating_add(n as u32);
                    if cleared < PANEL_BYTES {
                        return Self::emit(self.pc, cleared);
                    }
                }
                Self::emit(self.pc + 1, 0)
            }
            (Await::Dma(_), Input::Dma(Ok(_))) => (done, Status::Failed(AbiError::Transport)),
            _ => (self, Status::Ignored),
        }
    }
}

/// The top-level phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Running [`PROGRAM`].
    BringUp(Sequencer),
    /// The program finished; the shell is starting to draw (a sink route,
    /// or its own pipeline).
    Starting,
    /// Drawing runs.
    Streaming,
    /// Waiting [`RETRY_US`] before another bring-up.
    Backoff,
    /// [`MAX_FAILURES`] reached; nothing more happens.
    GaveUp,
}

/// What the shell must do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    /// Perform this bring-up action.
    Perform(Action),
    /// (Re)open the bus, D/C and reset pins, then perform this action.
    Reopen(Action),
    /// The panel is ready: start drawing (a sink route, or the payload's
    /// own pipeline) and report back with [`Controller::started`] or
    /// [`Controller::failed`].
    Start,
    /// Arm the timer for this many microseconds.
    Arm(u32),
    /// Log that the payload gave up.
    GiveUp,
    /// Nothing.
    Nothing,
}

/// The payload's decisions: bring-up, start of drawing, retry and give-up,
/// as a pure state machine over [`Phase`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Controller {
    phase: Phase,
    failures: u8,
}

impl Controller {
    /// The first bring-up.
    pub fn boot() -> (Self, Cmd) {
        Self::bring_up(0)
    }

    fn bring_up(failures: u8) -> (Self, Cmd) {
        let (seq, status) = Sequencer::start();
        let ctl = Controller {
            phase: Phase::BringUp(seq),
            failures,
        };
        match status {
            Status::Do(a) => (ctl, Cmd::Reopen(a)),
            // PROGRAM starts with an action; anything else is a bug.
            _ => ctl.failed(),
        }
    }

    /// The current phase.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Feeds a timer or DMA event.
    pub fn on(self, input: Input) -> (Self, Cmd) {
        match self.phase {
            Phase::BringUp(seq) => {
                let (seq, status) = seq.on(input);
                let next = Controller {
                    phase: Phase::BringUp(seq),
                    ..self
                };
                match status {
                    Status::Do(a) => (next, Cmd::Perform(a)),
                    Status::Finished => (
                        Controller {
                            phase: Phase::Starting,
                            ..self
                        },
                        Cmd::Start,
                    ),
                    Status::Failed(_) => next.failed(),
                    Status::Ignored => (self, Cmd::Nothing),
                }
            }
            Phase::Backoff if input == Input::Timer => Self::bring_up(self.failures),
            _ => (self, Cmd::Nothing),
        }
    }

    /// Something failed: an action could not start, a transfer failed, or
    /// drawing could not start or stopped by itself (e.g. a sink route). Backs off, or gives
    /// up after [`MAX_FAILURES`] in a row.
    pub fn failed(self) -> (Self, Cmd) {
        let failures = self.failures.saturating_add(1);
        if failures >= MAX_FAILURES {
            let ctl = Controller {
                phase: Phase::GaveUp,
                failures,
            };
            (ctl, Cmd::GiveUp)
        } else {
            let ctl = Controller {
                phase: Phase::Backoff,
                failures,
            };
            (ctl, Cmd::Arm(RETRY_US))
        }
    }

    /// Drawing started: streaming, and the failure count resets.
    pub fn started(self) -> Self {
        Controller {
            phase: Phase::Streaming,
            failures: 0,
        }
    }
}

/// The badge panel's bus and pins.
#[derive(Debug)]
pub struct Hw {
    /// SPI2 at [`SPI_KHZ`], mode 0, MOSI and CS, no MISO.
    pub spi: Spi,
    /// The D/C output ([`DC_PIN`]).
    pub dc: Output,
    /// The reset output ([`RST_PIN`]).
    pub rst: Output,
}

impl Hw {
    /// Opens (or re-opens) the bus and the D/C and reset pins.
    pub fn open() -> Result<Hw, AbiError> {
        let config = SpiConfig::builder(SCK_PIN)
            .mosi(MOSI_PIN)
            .cs(CS_PIN)
            .freq_khz(SPI_KHZ)
            .mode(Mode::Mode0)
            .build()?;
        let spi = Spi::open(config)?;
        let dc = Output::new(DC_PIN)?;
        let rst = Output::new(RST_PIN)?;
        Ok(Hw { spi, dc, rst })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_is_two_big_endian_u16() {
        assert_eq!(window(0, 239), [0, 0, 0, 0xEF]);
        assert_eq!(window(0x0102, 0x0304), [1, 2, 3, 4]);
    }

    #[test]
    fn program_ends_with_a_full_window_ramwr_and_the_clear() {
        const FULL: [u8; 4] = window(0, PANEL - 1);
        let n = PROGRAM.len();
        assert_eq!(
            &PROGRAM[n - 6..],
            &[
                Op::Command(CASET),
                Op::Params(&FULL),
                Op::Command(RASET),
                Op::Params(&FULL),
                Op::Command(RAMWR),
                Op::Clear,
            ]
        );
    }

    #[test]
    fn a_finished_bring_up_asks_to_start_drawing() {
        let (mut ctl, mut cmd) = Controller::boot();
        for _ in 0..1000 {
            let input = match cmd {
                Cmd::Perform(a) | Cmd::Reopen(a) => match a {
                    Action::Reset { .. } | Action::Delay { .. } => Input::Timer,
                    Action::Write { bytes, .. } => Input::Dma(Ok(bytes.len())),
                    Action::Zeros { len } => Input::Dma(Ok(len)),
                },
                Cmd::Start => break,
                other => panic!("unexpected {other:?}"),
            };
            (ctl, cmd) = ctl.on(input);
        }
        assert_eq!((ctl.phase(), cmd), (Phase::Starting, Cmd::Start));
        assert_eq!(ctl.started().phase(), Phase::Streaming);
    }

    #[test]
    fn host_stub_open_fails_cleanly() {
        assert!(matches!(Hw::open(), Err(AbiError::Unsupported)));
    }
}
