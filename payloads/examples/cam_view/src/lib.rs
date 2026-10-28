//! Camera viewer (conagotchi badge, ESP32-S3, ABI 4): shows the 120×120
//! RGB565 stream on [`FRAME_KEY`] in the centre of the badge's 240×240
//! round GC9A01 panel through a host *sink route*, so the pixels never pass
//! through the payload.
//!
//! **Wiring** (the badge's, `badge_core::board::display`): SPI2 with SCK
//! GPIO [`SCK_PIN`] (12), MOSI [`MOSI_PIN`] (11), CS [`CS_PIN`] (10), D/C
//! [`DC_PIN`] (13), reset [`RST_PIN`] (14, active low); no MISO, and no
//! backlight pin (the badge's backlight is always on). 40 MHz, SPI mode 0
//! ([`Hw::open`]).
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
//! The wiring, [`PROGRAM`] (the `mipidsi` 0.10 GC9A01 sequence with the
//! badge's inversion-on and BGR options, ending in a black fill of
//! [`CLEAR_CHUNK`]-byte zero transfers), the [`Sequencer`] and the
//! [`Controller`] live in the shared `gc9a01` crate (`payloads/gc9a01`,
//! also used by `cam_jpeg_view`) and are re-exported here; see its docs.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub use gc9a01::{
    Action, CLEAR_CHUNK, CS_PIN, Cmd, Controller, DC_PIN, Hw, Input, MAX_FAILURES, MOSI_PIN, Op,
    PANEL, PANEL_BYTES, PROGRAM, Phase, RETRY_US, RST_PIN, SCK_PIN, SPI_KHZ, Sequencer, Status,
};
use mw_payload::irq::Event;
use mw_payload::route::{Preamble, Route, RouteEvent, SinkConfig};
use mw_payload::timer::Timer;
use mw_payload::{AbiError, LogLine, Payload};

/// Key the frames arrive on (from `cam_stream`).
pub const FRAME_KEY: &str = "app/cam/frame";
/// Stream width and height in pixels.
pub const STREAM: u16 = 120;
/// Bytes per stream frame (RGB565).
pub const FRAME_LEN: u32 = STREAM as u32 * STREAM as u32 * 2;
/// The one hardware timer used, as a one-shot, for every delay.
pub const TIMER_ID: u8 = 0;
/// Period of the stats log.
pub const STATS_TICK_MS: u32 = 5_000;

/// The sink route config: D/C on [`DC_PIN`], [`FRAME_LEN`]-byte frames,
/// and a window centring [`STREAM`]×[`STREAM`] on the panel.
pub fn sink_config() -> Result<SinkConfig, AbiError> {
    let x0 = (PANEL - STREAM) / 2;
    let x1 = x0 + STREAM - 1;
    let window = Preamble::mipi_window(x0, x0, x1, x1)?;
    Ok(SinkConfig::new(DC_PIN, FRAME_LEN)?.with_preamble(window))
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
            Cmd::Reopen(a) => match Hw::open() {
                Ok(hw) => {
                    self.hw = Some(hw);
                    self.perform(a)
                }
                Err(e) => Err(e),
            },
            Cmd::Start => match self.start_sink() {
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
            (
                0x70,
                &[0x07, 0x07, 0x04, 0x0E, 0x0F, 0x09, 0x07, 0x08, 0x03],
            ),
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
        assert!(matches!(
            a[2],
            Action::Write {
                dc: false,
                bytes: [0xEF]
            }
        ));
    }

    #[test]
    fn sleep_out_waits_120_ms_before_display_on() {
        let a = run_all();
        let slpout = a
            .iter()
            .position(|x| {
                *x == Action::Write {
                    dc: false,
                    bytes: &[0x11],
                }
            })
            .expect("SLPOUT");
        assert_eq!(a[slpout + 1], Action::Delay { us: 120_000 });
        assert_eq!(
            a[slpout + 2],
            Action::Write {
                dc: false,
                bytes: &[0x29]
            }
        );
    }

    #[test]
    fn clear_fills_the_whole_panel_in_bounded_transfers_after_ramwr() {
        let a = run_all();
        let first = a
            .iter()
            .position(|x| matches!(x, Action::Zeros { .. }))
            .expect("clear");
        assert_eq!(
            a[first - 1],
            Action::Write {
                dc: false,
                bytes: &[0x2C]
            }
        );
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
        assert_eq!(
            seq.on(Input::Dma(Ok(0))).1,
            Status::Failed(AbiError::Transport)
        );
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
    /// `Starting` (after `Start`).
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
                Cmd::Start => return ctl,
                other => panic!("unexpected {other:?}"),
            };
            (ctl, cmd) = ctl.on(input);
        }
        panic!("no Start");
    }

    #[test]
    fn controller_boots_reopens_and_starts_the_sink() {
        let (ctl, cmd) = Controller::boot();
        assert!(matches!(
            cmd,
            Cmd::Reopen(Action::Reset { high: false, .. })
        ));
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
        assert!(matches!(
            cmd,
            Cmd::Reopen(Action::Reset { high: false, .. })
        ));
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
