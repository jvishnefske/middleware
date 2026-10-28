//! Camera viewer (conagotchi badge, ESP32-S3, ABI 4): shows the 120×120
//! RGB565 stream on [`FRAME_KEY`] in the centre of the badge's 240×240
//! round GC9A01 panel through a host *sink route*, so the pixels never pass
//! through the payload.
//!
//! **Wiring** (the badge's, `badge_core::board::display`): SPI2 with SCK
//! GPIO [`SCK_PIN`] (12), MOSI [`MOSI_PIN`] (11), CS [`CS_PIN`] (10), D/C
//! [`DC_PIN`] (13), reset [`RST_PIN`] (14, active low); no MISO, and no
//! backlight pin (the badge's backlight is always on). 40 MHz, SPI mode 0.
//!
//! The payload owns the panel bring-up; the host only streams frames:
//!
//! 1. `init` opens the bus and the D/C and reset outputs, then runs
//!    [`PROGRAM`] with the [`Sequencer`]: a hardware reset, the GC9A01 init
//!    sequence, a full-screen window and a black fill. Transfers are
//!    asynchronous DMA, so each step starts one transfer (or a one-shot
//!    timer for a delay) and the next step runs from the matching `DmaDone`
//!    (or `Timer`) event in `on_irq`. Every call does at most one GPIO write
//!    and one transfer, which keeps it far below the badge's 50 000-fuel
//!    budget.
//! 2. When the program finishes, `on_irq` starts a sink route on
//!    [`FRAME_KEY`] with [`sink_config`]: D/C on GPIO 13, 28 800-byte
//!    frames, and the [`Preamble::mipi_window`] centring 120×120 (columns
//!    and rows 60..=179) re-sent before every frame.
//! 3. Route events are counted in [`Stats`] and logged every
//!    [`STATS_TICK_MS`]. If the route stops by itself, or a bring-up step
//!    fails, the [`Controller`] waits [`RETRY_US`] and repeats the whole
//!    bring-up (re-opening the bus and pins the stopped route released),
//!    then re-creates the route. After [`MAX_FAILURES`] failures in a row
//!    without the route starting, it gives up and logs that.
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
//! The black fill sends 240 × 240 × 2 zero bytes in [`CLEAR_CHUNK`]-byte
//! transfers (30 of them). Each is `spi_transfer` with no transmit bytes and
//! a receive length of [`CLEAR_CHUNK`]: the ABI clocks `max(tx, rx)` bytes
//! and pads the transmit side with 0x00, so the zeros cost no guest memory
//! (a static zero buffer of that size would not fit the badge's 16 KiB
//! image limit). With inversion on, 0x0000 shows as black on this panel (as
//! in the firmware).
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use mw_payload::gpio::Output;
use mw_payload::irq::Event;
use mw_payload::route::{Preamble, Route, RouteEvent, SinkConfig};
use mw_payload::spi::{Mode, Spi, SpiConfig};
use mw_payload::timer::Timer;
use mw_payload::{AbiError, LogLine, Payload};

/// Key the frames arrive on (from `cam_stream`).
pub const FRAME_KEY: &str = "app/cam/frame";
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
/// Stream width and height in pixels.
pub const STREAM: u16 = 120;
/// Bytes per stream frame (RGB565).
pub const FRAME_LEN: u32 = STREAM as u32 * STREAM as u32 * 2;
/// Bytes of a full panel of RGB565 pixels.
pub const PANEL_BYTES: u32 = PANEL as u32 * PANEL as u32 * 2;
/// Bytes per black-fill transfer: 8 panel rows.
pub const CLEAR_CHUNK: usize = 8 * PANEL as usize * 2;
const _: () = assert!(CLEAR_CHUNK <= mw_payload::spi::MAX_DMA, "one DMA transfer");
/// The one hardware timer used, as a one-shot, for every delay.
pub const TIMER_ID: u8 = 0;
/// Wait before repeating a failed bring-up or a stopped route.
pub const RETRY_US: u32 = 1_000_000;
/// Bring-up failures in a row after which the payload gives up.
pub const MAX_FAILURES: u8 = 5;
/// Period of the stats log.
pub const STATS_TICK_MS: u32 = 5_000;

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
            Some(&Op::Reset { high, wait_us }) => {
                (at(Await::Timer), Status::Do(Action::Reset { high, wait_us }))
            }
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

/// The sink route config: D/C on [`DC_PIN`], [`FRAME_LEN`]-byte frames,
/// and a window centring [`STREAM`]×[`STREAM`] on the panel.
pub fn sink_config() -> Result<SinkConfig, AbiError> {
    let x0 = (PANEL - STREAM) / 2;
    let x1 = x0 + STREAM - 1;
    let window = Preamble::mipi_window(x0, x0, x1, x1)?;
    Ok(SinkConfig::new(DC_PIN, FRAME_LEN)?.with_preamble(window))
}

/// The top-level phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Running [`PROGRAM`].
    BringUp(Sequencer),
    /// The program finished; the shell is starting the route.
    Starting,
    /// The route runs.
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
    /// Start the sink route; report back with [`Controller::started`] or
    /// [`Controller::failed`].
    StartSink,
    /// Arm the timer for this many microseconds.
    Arm(u32),
    /// Log that the payload gave up.
    GiveUp,
    /// Nothing.
    Nothing,
}

/// The payload's decisions: bring-up, route start, retry and give-up, as a
/// pure state machine over [`Phase`].
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
                        Cmd::StartSink,
                    ),
                    Status::Failed(_) => next.failed(),
                    Status::Ignored => (self, Cmd::Nothing),
                }
            }
            Phase::Backoff if input == Input::Timer => Self::bring_up(self.failures),
            _ => (self, Cmd::Nothing),
        }
    }

    /// Something failed: an action could not start, a transfer failed, the
    /// route could not start or it stopped by itself. Backs off, or gives
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

    /// The route started: streaming, and the failure count resets.
    pub fn started(self) -> Self {
        Controller {
            phase: Phase::Streaming,
            failures: 0,
        }
    }
}

/// Route counters, all saturating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    /// Frames drawn.
    pub frames: u32,
    /// Frames abandoned after a lost chunk.
    pub dropped: u32,
    /// Times the route stopped by itself.
    pub stops: u32,
}

impl Stats {
    /// Folds one route event in.
    pub fn apply(self, event: RouteEvent) -> Self {
        match event {
            RouteEvent::Frames(n) => Stats {
                frames: self.frames.saturating_add(n),
                ..self
            },
            RouteEvent::Dropped(n) => Stats {
                dropped: self.dropped.saturating_add(n),
                ..self
            },
            RouteEvent::Stopped(_) => Stats {
                stops: self.stops.saturating_add(1),
                ..self
            },
        }
    }
}

/// Logs `"cam_view: <what> error <code>"`.
fn log_err(what: &str, err: AbiError) {
    let mut line = LogLine::new();
    line.push_str("cam_view: ")
        .push_str(what)
        .push_str(" error ")
        .push_u32(err.code().unsigned_abs());
    line.log();
}

/// The bus and pins.
struct Hw {
    spi: Spi,
    dc: Output,
    rst: Output,
}

/// Opens (or re-opens) the bus and pins.
fn open_hw() -> Result<Hw, AbiError> {
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

/// Application state: the imperative shell around [`Controller`].
pub struct CamView {
    ctl: Controller,
    hw: Option<Hw>,
    timer: Option<Timer>,
    route: Option<Route>,
    stats: Stats,
}

impl CamView {
    /// Arms the one-shot timer, stopping the previous one first.
    fn arm(&mut self, us: u32) -> Result<(), AbiError> {
        if let Some(t) = self.timer.take() {
            let _ = t.stop();
        }
        self.timer = Some(Timer::start_once(TIMER_ID, us)?);
        Ok(())
    }

    /// Performs a bring-up action.
    fn perform(&mut self, action: Action) -> Result<(), AbiError> {
        match action {
            Action::Reset { high, wait_us } => {
                let hw = self.hw.as_mut().ok_or(AbiError::Busy)?;
                hw.rst.set(high)?;
                self.arm(wait_us)
            }
            Action::Delay { us } => self.arm(us),
            Action::Write { dc, bytes } => {
                let hw = self.hw.as_mut().ok_or(AbiError::Busy)?;
                hw.dc.set(dc)?;
                hw.spi.transfer(bytes, 0)
            }
            Action::Zeros { len } => {
                let hw = self.hw.as_mut().ok_or(AbiError::Busy)?;
                hw.dc.set(true)?;
                hw.spi.transfer(&[], len)
            }
        }
    }

    /// Starts the sink route.
    fn start_sink(&mut self) -> Result<Route, AbiError> {
        let hw = self.hw.as_ref().ok_or(AbiError::Busy)?;
        Route::sink(FRAME_KEY, &hw.spi, &sink_config()?)
    }

    /// Carries out `cmd`; a failure feeds back into the controller once
    /// (its own command, an `Arm` or `GiveUp`, is carried out without
    /// further feedback, so this never loops).
    fn run(&mut self, ctl: Controller, cmd: Cmd) {
        self.ctl = ctl;
        let result = match cmd {
            Cmd::Perform(a) => self.perform(a),
            Cmd::Reopen(a) => match open_hw() {
                Ok(hw) => {
                    self.hw = Some(hw);
                    self.perform(a)
                }
                Err(e) => Err(e),
            },
            Cmd::StartSink => match self.start_sink() {
                Ok(route) => {
                    self.route = Some(route);
                    self.ctl = self.ctl.started();
                    mw_payload::log("cam_view: panel ready, showing app/cam/frame");
                    Ok(())
                }
                Err(e) => Err(e),
            },
            Cmd::Arm(us) => self.arm(us),
            Cmd::GiveUp => {
                mw_payload::log("cam_view: giving up after repeated failures");
                Ok(())
            }
            Cmd::Nothing => Ok(()),
        };
        if let Err(e) = result {
            log_err("bring-up", e);
            let (ctl, cmd) = self.ctl.failed();
            self.ctl = ctl;
            let retry = match cmd {
                Cmd::Arm(us) => self.arm(us),
                _ => {
                    mw_payload::log("cam_view: giving up after repeated failures");
                    Ok(())
                }
            };
            if let Err(e) = retry {
                log_err("timer_start", e);
            }
        }
    }
}

impl Payload for CamView {
    fn init() -> Self {
        if let Err(e) = mw_payload::set_tick(STATS_TICK_MS) {
            log_err("set_tick", e);
        }
        let (ctl, cmd) = Controller::boot();
        let mut app = CamView {
            ctl,
            hw: None,
            timer: None,
            route: None,
            stats: Stats::default(),
        };
        app.run(ctl, cmd);
        app
    }

    fn tick(&mut self, _now_ms: u32) {
        if self.ctl.phase() != Phase::Streaming {
            return;
        }
        let mut line = LogLine::new();
        line.push_str("cam_view: frames ")
            .push_u32(self.stats.frames)
            .push_str(" dropped ")
            .push_u32(self.stats.dropped)
            .push_str(" stops ")
            .push_u32(self.stats.stops);
        line.log();
    }

    fn on_irq(&mut self, event: Event) {
        if let Some(ev) = self.route.as_ref().and_then(|r| r.on_event(&event)) {
            self.stats = self.stats.apply(ev);
            if let RouteEvent::Stopped(e) = ev {
                // The route's handle is free and its bus and D/C pin are
                // released; bring the panel up again and re-create it.
                log_err("route stopped", e);
                self.route = None;
                let (ctl, cmd) = self.ctl.failed();
                self.run(ctl, cmd);
            }
            return;
        }
        let input = match event {
            Event::Timer { id: TIMER_ID, .. } => Input::Timer,
            Event::DmaDone { .. } => {
                match self.hw.as_mut().and_then(|hw| hw.spi.on_event(&event)) {
                    Some(result) => Input::Dma(result),
                    None => return,
                }
            }
            _ => return,
        };
        let (ctl, cmd) = self.ctl.on(input);
        self.run(ctl, cmd);
    }
}

// The exports need a single-threaded wasm32 module; host builds only run
// the unit tests below.
#[cfg(target_arch = "wasm32")]
mw_payload::payload!(CamView);

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs the sequencer to completion with every event succeeding;
    /// returns the actions in order.
    fn run_all() -> Vec<Action> {
        let mut out = Vec::new();
        let (mut seq, mut status) = Sequencer::start();
        for _ in 0..1000 {
            match status {
                Status::Do(a) => {
                    out.push(a);
                    let input = match a {
                        Action::Reset { .. } | Action::Delay { .. } => Input::Timer,
                        Action::Write { bytes, .. } => Input::Dma(Ok(bytes.len())),
                        Action::Zeros { len } => Input::Dma(Ok(len)),
                    };
                    (seq, status) = seq.on(input);
                }
                Status::Finished => return out,
                other => panic!("unexpected {other:?}"),
            }
        }
        panic!("did not finish");
    }

    /// The writes as (command, parameters) pairs, checking D/C levels:
    /// every command is one byte with D/C low, every parameter run D/C
    /// high.
    fn commands(actions: &[Action]) -> Vec<(u8, Vec<u8>)> {
        let mut out: Vec<(u8, Vec<u8>)> = Vec::new();
        for a in actions {
            match *a {
                Action::Write { dc: false, bytes } => {
                    assert_eq!(bytes.len(), 1, "a command is one byte");
                    out.push((bytes[0], Vec::new()));
                }
                Action::Write { dc: true, bytes } => {
                    let last = out.last_mut().expect("parameters follow a command");
                    assert!(last.1.is_empty(), "one parameter run per command");
                    last.1.extend_from_slice(bytes);
                }
                _ => {}
            }
        }
        out
    }

    #[test]
    fn init_sequence_matches_mipidsi_gc9a01_with_the_badge_options() {
        let cmds = commands(&run_all());
        let expect: &[(u8, &[u8])] = &[
            (0xEF, &[]),
            (0xEB, &[0x14]),
            (0xFE, &[]),
            (0xEF, &[]),
            (0xEB, &[0x14]),
            (0x84, &[0x40]),
            (0x85, &[0xFF]),
            (0x86, &[0xFF]),
            (0x87, &[0xFF]),
            (0x88, &[0x0A]),
            (0x89, &[0x21]),
            (0x8A, &[0x00]),
            (0x8B, &[0x80]),
            (0x8C, &[0x01]),
            (0x8D, &[0x01]),
            (0x8E, &[0xFF]),
            (0x8F, &[0xFF]),
            (0xB6, &[0x00, 0x20]),
            (0x36, &[0x08]),
            (0x3A, &[0x55]),
            (0x90, &[0x08, 0x08, 0x08, 0x08]),
            (0xBD, &[0x06]),
            (0xBC, &[0x00]),
            (0xFF, &[0x60, 0x01, 0x04]),
            (0xC3, &[0x13]),
            (0xC4, &[0x13]),
            (0xC9, &[0x22]),
            (0xBE, &[0x11]),
            (0xE1, &[0x10, 0x0E]),
            (0xDF, &[0x20, 0x0C, 0x02]),
            (0xF0, &[0x45, 0x09, 0x08, 0x08, 0x26, 0x2A]),
            (0xF1, &[0x43, 0x70, 0x72, 0x36, 0x37, 0x6F]),
            (0xF2, &[0x45, 0x09, 0x08, 0x08, 0x26, 0x2A]),
            (0xF3, &[0x43, 0x70, 0x72, 0x36, 0x37, 0x6F]),
            (0xED, &[0x18, 0x0B]),
            (0xAE, &[0x77]),
            (0xCD, &[0x63]),
            (0x70, &[0x07, 0x07, 0x04, 0x0E, 0x0F, 0x09, 0x07, 0x08, 0x03]),
            (0xE8, &[0x34]),
            (
                0x62,
                &[
                    0x18, 0x0D, 0x71, 0xED, 0x70, 0x70, 0x18, 0x0F, 0x71, 0xEF, 0x70, 0x70,
                ],
            ),
            (
                0x63,
                &[
                    0x18, 0x11, 0x71, 0xF1, 0x70, 0x70, 0x18, 0x13, 0x71, 0xF3, 0x70, 0x70,
                ],
            ),
            (0x64, &[0x28, 0x29, 0xF1, 0x01, 0xF1, 0x00, 0x07]),
            (
                0x66,
                &[0x3C, 0x00, 0xCD, 0x67, 0x45, 0x45, 0x10, 0x00, 0x00, 0x00],
            ),
            (
                0x67,
                &[0x00, 0x3C, 0x00, 0x00, 0x00, 0x01, 0x54, 0x10, 0x32, 0x98],
            ),
            (0x74, &[0x10, 0x85, 0x80, 0x00, 0x00, 0x4E, 0x00]),
            (0x98, &[0x3E, 0x07]),
            (0x21, &[]),
            (0x11, &[]),
            (0x29, &[]),
            (0x2A, &[0, 0, 0, 239]),
            (0x2B, &[0, 0, 0, 239]),
            (0x2C, &[]),
        ];
        let expect: Vec<(u8, Vec<u8>)> = expect.iter().map(|(c, p)| (*c, p.to_vec())).collect();
        assert_eq!(cmds, expect);
    }

    #[test]
    fn reset_then_power_up_delay_come_first() {
        let a = run_all();
        assert_eq!(
            a[0],
            Action::Reset {
                high: false,
                wait_us: 10_000
            }
        );
        assert_eq!(
            a[1],
            Action::Reset {
                high: true,
                wait_us: 200_000
            }
        );
        assert!(matches!(a[2], Action::Write { dc: false, bytes: [0xEF] }));
    }

    #[test]
    fn sleep_out_waits_120_ms_before_display_on() {
        let a = run_all();
        let slpout = a
            .iter()
            .position(|x| *x == Action::Write { dc: false, bytes: &[0x11] })
            .expect("SLPOUT");
        assert_eq!(a[slpout + 1], Action::Delay { us: 120_000 });
        assert_eq!(a[slpout + 2], Action::Write { dc: false, bytes: &[0x29] });
    }

    #[test]
    fn clear_fills_the_whole_panel_in_bounded_transfers_after_ramwr() {
        let a = run_all();
        let first = a
            .iter()
            .position(|x| matches!(x, Action::Zeros { .. }))
            .expect("clear");
        assert_eq!(a[first - 1], Action::Write { dc: false, bytes: &[0x2C] });
        let lens: Vec<usize> = a[first..]
            .iter()
            .map(|x| match *x {
                Action::Zeros { len } => len,
                other => panic!("only zeros after RAMWR, got {other:?}"),
            })
            .collect();
        assert_eq!(lens.iter().sum::<usize>(), 240 * 240 * 2);
        assert!(lens.iter().all(|&n| n > 0 && n <= CLEAR_CHUNK));
        assert_eq!(lens.len(), 30, "8 rows per transfer");
        // Every action waits for exactly one event: about 140 events in all.
        assert!(a.len() < 200, "{}", a.len());
    }

    #[test]
    fn sequencer_ignores_unawaited_events_and_fails_on_dma_errors() {
        let (seq, status) = Sequencer::start();
        assert!(matches!(status, Status::Do(Action::Reset { .. })));
        // Waiting for the timer: a DMA completion is not ours.
        assert_eq!(seq.on(Input::Dma(Ok(1))), (seq, Status::Ignored));
        let (seq, _) = seq.on(Input::Timer);
        let (seq, status) = seq.on(Input::Timer);
        assert_eq!(
            status,
            Status::Do(Action::Write {
                dc: false,
                bytes: &[0xEF]
            })
        );
        // Waiting for DMA: a stray timer is ignored.
        assert_eq!(seq.on(Input::Timer), (seq, Status::Ignored));
        let (failed, status) = seq.on(Input::Dma(Err(AbiError::Transport)));
        assert_eq!(status, Status::Failed(AbiError::Transport));
        assert_eq!(failed.on(Input::Dma(Ok(1))).1, Status::Ignored, "inert");
        // A short transfer is a failure too.
        assert_eq!(seq.on(Input::Dma(Ok(0))).1, Status::Failed(AbiError::Transport));
    }

    #[test]
    fn sink_config_bytes_match_the_abi_example() {
        let cfg = sink_config().expect("valid");
        let mut out = [0u8; mw_payload::route::MAX_SINK_CFG_LEN];
        let n = cfg.encode(&mut out);
        assert_eq!(
            &out[..n],
            &[
                13, 0, 0x80, 0x70, 0, 0, 5, // D/C 13, 28 800 LE, 5 segments
                0, 1, 0x2A, // CASET
                1, 4, 0, 60, 0, 179, // x 60..=179
                0, 1, 0x2B, // RASET
                1, 4, 0, 60, 0, 179, // y 60..=179
                0, 1, 0x2C, // RAMWR
            ]
        );
        assert_eq!(SinkConfig::parse(&out[..n]), Ok(cfg));
    }

    /// Drives a controller through a full bring-up; returns it in
    /// `Starting` (after `StartSink`).
    fn bring_up(ctl: Controller, first: Cmd) -> Controller {
        let mut cmd = first;
        let mut ctl = ctl;
        for _ in 0..1000 {
            let input = match cmd {
                Cmd::Perform(a) | Cmd::Reopen(a) => match a {
                    Action::Reset { .. } | Action::Delay { .. } => Input::Timer,
                    Action::Write { bytes, .. } => Input::Dma(Ok(bytes.len())),
                    Action::Zeros { len } => Input::Dma(Ok(len)),
                },
                Cmd::StartSink => return ctl,
                other => panic!("unexpected {other:?}"),
            };
            (ctl, cmd) = ctl.on(input);
        }
        panic!("no StartSink");
    }

    #[test]
    fn controller_boots_reopens_and_starts_the_sink() {
        let (ctl, cmd) = Controller::boot();
        assert!(matches!(cmd, Cmd::Reopen(Action::Reset { high: false, .. })));
        let ctl = bring_up(ctl, cmd);
        assert_eq!(ctl.phase(), Phase::Starting);
        let ctl = ctl.started();
        assert_eq!(ctl.phase(), Phase::Streaming);
        // Streaming ignores timer and DMA events.
        assert_eq!(ctl.on(Input::Timer), (ctl, Cmd::Nothing));
    }

    #[test]
    fn route_stop_backs_off_then_redoes_the_bring_up() {
        let (ctl, cmd) = Controller::boot();
        let ctl = bring_up(ctl, cmd).started();
        let (ctl, cmd) = ctl.failed();
        assert_eq!((ctl.phase(), cmd), (Phase::Backoff, Cmd::Arm(RETRY_US)));
        assert_eq!(ctl.on(Input::Dma(Ok(1))), (ctl, Cmd::Nothing));
        let (ctl, cmd) = ctl.on(Input::Timer);
        assert!(matches!(cmd, Cmd::Reopen(Action::Reset { high: false, .. })));
        assert_eq!(bring_up(ctl, cmd).phase(), Phase::Starting);
    }

    #[test]
    fn repeated_failures_give_up() {
        let (mut ctl, _) = Controller::boot();
        for _ in 1..MAX_FAILURES {
            // The first transfer fails.
            let (c, _) = ctl.on(Input::Timer);
            let (c, _) = c.on(Input::Timer);
            let (c, cmd) = c.on(Input::Dma(Err(AbiError::Transport)));
            assert_eq!((c.phase(), cmd), (Phase::Backoff, Cmd::Arm(RETRY_US)));
            let (c, cmd) = c.on(Input::Timer);
            assert!(matches!(cmd, Cmd::Reopen(_)));
            ctl = c;
        }
        let (ctl, cmd) = ctl.failed();
        assert_eq!((ctl.phase(), cmd), (Phase::GaveUp, Cmd::GiveUp));
        assert_eq!(ctl.on(Input::Timer), (ctl, Cmd::Nothing));
    }

    #[test]
    fn a_started_route_resets_the_failure_count() {
        let (ctl, _) = Controller::boot();
        let (ctl, _) = ctl.failed();
        let (ctl, _) = ctl.failed();
        let ctl = ctl.started();
        for _ in 1..MAX_FAILURES {
            let (c, cmd) = ctl.failed();
            assert_eq!(cmd, Cmd::Arm(RETRY_US));
            let _ = c;
        }
    }

    #[test]
    fn stats_accumulate_and_saturate() {
        let s = Stats::default()
            .apply(RouteEvent::Frames(2))
            .apply(RouteEvent::Dropped(1))
            .apply(RouteEvent::Frames(u32::MAX))
            .apply(RouteEvent::Stopped(AbiError::Busy));
        assert_eq!(
            s,
            Stats {
                frames: u32::MAX,
                dropped: 1,
                stops: 1
            }
        );
    }

    #[test]
    fn host_stub_bring_up_backs_off_instead_of_panicking() {
        // Off wasm32 every import is Unsupported: open fails, the
        // controller backs off (and arming the timer fails quietly).
        let app = CamView::init();
        assert_eq!(app.ctl.phase(), Phase::Backoff);
        assert!(app.route.is_none());
    }
}
