//! JPEG camera viewer (conagotchi badge, ESP32-S3, ABI 4, requirement
//! R14): receives the 240×240 baseline JPEG stream on [`FRAME_KEY`] (from
//! `cam_jpeg_stream`), reassembles each frame in linear memory, decodes it
//! in WASM with `mw-jpeg` in bounded steps and draws it on the badge's
//! 240×240 GC9A01 panel with its own SPI transfers.
//!
//! 1. `init` sets a [`STATS_TICK_MS`] tick, subscribes to [`FRAME_KEY`]
//!    and brings the panel up with the shared `gc9a01` crate (reset, init
//!    sequence, black fill; one transfer or delay per event on timer
//!    [`BRINGUP_TIMER`]). A failed bring-up is retried after 1 s and
//!    abandoned after 5 failures in a row ([`gc9a01::Controller`]).
//! 2. `on_message` feeds every chunk to the [`reasm::Reassembler`] and
//!    copies the stored ones into the frame slots (see [`reasm`] for the
//!    rules: in-order continuation only; a gap drops the frame). A
//!    completed frame arms the decode timer when the pipeline is idle.
//! 3. The decode timer ([`DECODE_TIMER`], one-shot, [`DECODE_GAP_US`])
//!    drives the [`pipeline::Pipe`]: when idle, it takes the newest
//!    complete frame and runs `Decoder::start` on it; then each event runs
//!    one `decode_mcus(.., MCUS_PER_STEP)` call. A completed 16-row strip
//!    is sent with `CASET` / `RASET` / `RAMWR` and the RGB565 pixels in at
//!    most [`pipeline::PIXEL_PART`]-byte transfers, each started from the
//!    previous transfer's `DmaDone`; decoding resumes after the last one.
//!    One transfer is in flight at a time.
//! 4. Every tick logs and publishes [`ViewStats`] on [`STATS_KEY`].
//!
//! # Work per call (fuel)
//!
//! Every call into the payload runs on a fixed fuel budget
//! (`wasm_abi::FUEL_PER_CALL`, 1 000 000). The heaviest calls are the
//! decode steps: `mw-jpeg` measures a 4:2:0 MCU at 56 600 fuel on average
//! on the fake camera's quality-60 frames and 114 000 on the most detailed
//! MCU of a noisy quality-80 frame, so [`MCUS_PER_STEP`] = 4 bounds a step
//! at about 460 000 even for that worst case (the runtime translates the
//! whole payload when it loads it, so first calls cost no extra), well under the target of
//! 600 000. `Decoder::start` (about 20 000, 56 000 on its first call) gets
//! a call of its own. A 240-wide row of 15 MCUs takes four steps, and a
//! frame 60 steps, 15 strips of 7 transfers and one start. Measured in
//! `payload-sim` (`tools/payload-sim/tests/r14.rs`, 1 000 000 fuel per
//! call) on the fixture frames: at most 259 985 fuel in one call, about
//! 13.2 M per frame in all (reception included), so the frame rate is the
//! host's wasmi speed divided by that. This crate and `mw-jpeg` are built
//! at `opt-level = "s"` (`payloads/Cargo.toml`): against "z", 12 % less
//! fuel per frame (15.1 M) for no more code.
//!
//! # Code size
//!
//! The heap a host needs for a payload grows by about 2.75 bytes per image
//! byte (wasmi's translated module, `wasm_host::heap`), so the image is
//! kept small: 25 714 bytes (23.3 KB of code), down from 30 878.
//!
//! * `mw-jpeg` without its `half` feature: no reduced 4×4 IDCT (only
//!   [`Scale::Full`] is used).
//! * No panic path at all, so none of core's panic formatting
//!   (`panic_fmt`, `Display for usize`): the SDK's `LogLine` and its
//!   message copy cannot panic, and frame chunks are copied with a zip
//!   rather than `copy_from_slice`, whose length check stays an
//!   out-of-line call.
//!
//! # Memory
//!
//! | what | bytes |
//! |---|---|
//! | shadow stack (`-zstack-size`) | 8 192 (deploy) / 16 384 (standard) |
//! | [`SLOTS`] frame slots of [`FRAME_CAP`] | 24 576 |
//! | strip buffer ([`STRIP_BYTES`], 240 × 16 RGB565) | 7 680 |
//! | `mw_jpeg::Decoder` | 4 352 |
//! | constants and other statics (SDK receive buffer, payload state) | about 2 500 |
//!
//! Data and `.bss` take 39 088 bytes as linked. **Deploy the byte-page
//! build** (`payloads/scripts/build-byte-pages.sh -p cam_jpeg_view`, output
//! in `payloads/target/byte-pages/wasm32-unknown-unknown/release/`): its
//! linear memory is fixed at MEMORY = 48 128 bytes (initial = maximum,
//! 1-byte pages) with STACK = 8 192, from `[package.metadata.byte-pages]`
//! in `Cargo.toml`: stack + data + `.bss` = 47 280 (`__heap_base`),
//! rounded up to 1 KiB. The stack is about 2.7 times what the fixture
//! stream needs (an overflow traps with 2 816 bytes; 3 072 run clean).
//! The host's heap bound charges the module's declared 48 128 bytes rather
//! than a 64 KiB page: on the badge (32-bit) it needs 164 291 bytes free
//! (160 195 bound + 4 096 margin), against 181 685 for the standard build
//! (one 64 KiB page, 16 KiB stack, `.bss` up to 55 472).
//!
//! The big buffers live in one all-zero static ([`Buffers::EMPTY`], in
//! `.bss`), not in the payload state, which the SDK builds on the stack.
//! The fixture frames are at most 4 728 bytes; [`FRAME_CAP`] leaves room
//! for a real camera's larger frames, and a longer frame is dropped.
//!
//! # Falling behind
//!
//! The camera sends 10 frames a second; the badge decodes fewer. Frames
//! keep arriving during a decode (the fill slot), and the newest complete
//! one waits in the ready slot, replacing an older one that was never
//! decoded ([`reasm::RxStats::superseded`]). So the decoder always starts
//! on the newest complete frame.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod pipeline;
pub mod reasm;

use gc9a01::{Action, Cmd as CtlCmd, Controller, Hw, Input, Phase};
use mw_jpeg::{Decoder, Error, Rgb565Be, Scale};
use mw_payload::irq::Event;
use mw_payload::timer::Timer;
use mw_payload::{AbiError, LogLine};

use pipeline::{Cmd, Pipe, Xfer};
use reasm::{Frame, Reassembler, SLOTS, Verdict};

/// Key the JPEG chunks arrive on (from `cam_jpeg_stream`).
pub const FRAME_KEY: &str = "app/cam/jpeg";
/// Key the [`ViewStats`] are published on.
pub const STATS_KEY: &str = "app/cam/view_stats";
/// Bytes per frame slot: the largest frame accepted.
pub const FRAME_CAP: usize = 8 * 1024;
/// Strip buffer: 16 rows (a 4:2:0 MCU row) of 240 RGB565 pixels.
pub const STRIP_BYTES: usize = gc9a01::PANEL as usize * 16 * 2;
/// MCUs per decoder call; see "Work per call" in the module docs.
pub const MCUS_PER_STEP: u16 = 4;
/// Timer for the panel bring-up delays.
pub const BRINGUP_TIMER: u8 = 0;
/// Timer pacing the decoder calls.
pub const DECODE_TIMER: u8 = 1;
/// Gap between decoder calls, in microseconds: lets the host run its own
/// work (radio, other events) between two heavy calls.
pub const DECODE_GAP_US: u32 = 1_000;
/// Period of the stats tick.
pub const STATS_TICK_MS: u32 = 2_000;

/// The large buffers, kept out of the payload state (see "Memory").
pub struct Buffers {
    /// Frame slots, indexed by [`Frame::slot`].
    pub frames: [[u8; FRAME_CAP]; SLOTS],
    /// The decoder's output strip.
    pub strip: [u8; STRIP_BYTES],
    /// The JPEG decoder.
    pub dec: Decoder,
}

impl Buffers {
    /// All zero bytes, so a static of it is `.bss`.
    pub const EMPTY: Buffers = Buffers {
        frames: [[0; FRAME_CAP]; SLOTS],
        strip: [0; STRIP_BYTES],
        dec: Decoder::IDLE,
    };
}

/// The bytes of `frame` in its slot.
fn frame_bytes(frames: &[[u8; FRAME_CAP]; SLOTS], frame: Frame) -> Option<&[u8]> {
    frames
        .get(usize::from(frame.slot))?
        .get(..frame.len as usize)
}

/// Viewer counters, all saturating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ViewStats {
    /// Frames reassembled completely.
    pub received: u32,
    /// Frames decoded and drawn.
    pub decoded: u32,
    /// Frames dropped before decoding: lost chunks, too large, or
    /// superseded by a newer frame.
    pub dropped: u32,
    /// Frames abandoned while decoding or drawing (decoder error, SPI
    /// error, image larger than the panel).
    pub failed: u32,
    /// Milliseconds from decoder start to the last strip drawn, of the last
    /// frame drawn.
    pub decode_ms: u32,
}

impl ViewStats {
    /// The [`STATS_KEY`] payload: the five fields in declaration order as
    /// `u32` little-endian.
    pub fn encode(&self) -> [u8; 20] {
        let mut out = [0u8; 20];
        let fields = [
            self.received,
            self.decoded,
            self.dropped,
            self.failed,
            self.decode_ms,
        ];
        for (dst, n) in out.chunks_exact_mut(4).zip(fields) {
            dst.copy_from_slice(&n.to_le_bytes());
        }
        out
    }
}

/// Logs `"cam_jpeg_view: <what> error <code>"`.
fn log_err(what: &str, err: AbiError) {
    let mut line = LogLine::new();
    line.push_str("cam_jpeg_view: ")
        .push_str(what)
        .push_str(" error ")
        .push_u32(err.code().unsigned_abs());
    line.log();
}

/// Logs `"cam_jpeg_view: frame <seq> <what>"`, then `" in <ms> ms"` when
/// given.
fn log_frame(frame: Frame, what: &str, ms: Option<u32>) {
    let mut line = LogLine::new();
    line.push_str("cam_jpeg_view: frame ")
        .push_u32(u32::from(frame.seq))
        .push_str(" ")
        .push_str(what);
    if let Some(ms) = ms {
        line.push_str(" in ").push_u32(ms).push_str(" ms");
    }
    line.log();
}

/// Stops `timer` (if any) and starts `id` as a one-shot of `us`.
fn rearm(timer: &mut Option<Timer>, id: u8, us: u32) -> Result<(), AbiError> {
    if let Some(t) = timer.take() {
        let _ = t.stop();
    }
    *timer = Some(Timer::start_once(id, us)?);
    Ok(())
}

/// Application state: the imperative shell around [`Controller`],
/// [`Reassembler`] and [`Pipe`]. Its methods take the [`Buffers`]
/// explicitly.
pub struct CamJpegView {
    ctl: Controller,
    hw: Option<Hw>,
    bringup_timer: Option<Timer>,
    decode_timer: Option<Timer>,
    /// The decode timer is armed and has not fired yet.
    decode_armed: bool,
    rx: Reassembler,
    pipe: Pipe,
    stats: ViewStats,
}

impl CamJpegView {
    /// The `init` work: tick, subscription and the first bring-up step.
    pub fn new() -> Self {
        if let Err(e) = mw_payload::set_tick(STATS_TICK_MS) {
            log_err("set_tick", e);
        }
        if let Err(e) = mw_payload::subscribe(FRAME_KEY) {
            log_err("subscribe", e);
        }
        let (ctl, cmd) = Controller::boot();
        let mut app = CamJpegView {
            ctl,
            hw: None,
            bringup_timer: None,
            decode_timer: None,
            decode_armed: false,
            rx: Reassembler::new(FRAME_CAP as u32),
            pipe: Pipe::Idle,
            stats: ViewStats::default(),
        };
        app.run(ctl, cmd);
        app
    }

    /// The counters, with the reassembler's folded in.
    pub fn stats(&self) -> ViewStats {
        let rx = self.rx.stats();
        ViewStats {
            received: rx.received,
            dropped: rx.lost.saturating_add(rx.superseded),
            ..self.stats
        }
    }

    /// The bring-up phase.
    pub fn phase(&self) -> Phase {
        self.ctl.phase()
    }

    /// Performs a bring-up action.
    fn perform(&mut self, action: Action) -> Result<(), AbiError> {
        match action {
            Action::Reset { high, wait_us } => {
                let hw = self.hw.as_mut().ok_or(AbiError::Busy)?;
                hw.rst.set(high)?;
                rearm(&mut self.bringup_timer, BRINGUP_TIMER, wait_us)
            }
            Action::Delay { us } => rearm(&mut self.bringup_timer, BRINGUP_TIMER, us),
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

    /// Carries out a controller command; a failure feeds back into the
    /// controller once (its answer, an `Arm` or `GiveUp`, is carried out
    /// without further feedback, so this never loops).
    fn run(&mut self, ctl: Controller, cmd: CtlCmd) {
        self.ctl = ctl;
        let result = match cmd {
            CtlCmd::Perform(a) => self.perform(a),
            CtlCmd::Reopen(a) => match Hw::open() {
                Ok(hw) => {
                    self.hw = Some(hw);
                    self.perform(a)
                }
                Err(e) => Err(e),
            },
            CtlCmd::Start => {
                self.ctl = self.ctl.started();
                mw_payload::log("cam_jpeg_view: panel ready, showing app/cam/jpeg");
                self.kick();
                Ok(())
            }
            CtlCmd::Arm(us) => rearm(&mut self.bringup_timer, BRINGUP_TIMER, us),
            CtlCmd::GiveUp => {
                mw_payload::log("cam_jpeg_view: giving up after repeated failures");
                Ok(())
            }
            CtlCmd::Nothing => Ok(()),
        };
        if let Err(e) = result {
            log_err("bring-up", e);
            let (ctl, cmd) = self.ctl.failed();
            self.ctl = ctl;
            let retry = match cmd {
                CtlCmd::Arm(us) => rearm(&mut self.bringup_timer, BRINGUP_TIMER, us),
                _ => {
                    mw_payload::log("cam_jpeg_view: giving up after repeated failures");
                    Ok(())
                }
            };
            if let Err(e) = retry {
                log_err("timer_start", e);
            }
        }
    }

    /// Arms the decode timer unless it is armed already.
    fn arm_decode(&mut self) {
        if self.decode_armed {
            return;
        }
        match rearm(&mut self.decode_timer, DECODE_TIMER, DECODE_GAP_US) {
            Ok(()) => self.decode_armed = true,
            Err(e) => log_err("decode timer", e),
        }
    }

    /// Arms the decode timer if the panel is up, the pipeline idle and a
    /// frame ready.
    fn kick(&mut self) {
        if self.ctl.phase() == Phase::Streaming && self.pipe.is_idle() && self.rx.ready().is_some()
        {
            self.arm_decode();
        }
    }

    /// `on_message`: stores a chunk and kicks the decoder when it completed
    /// a frame.
    pub fn receive(&mut self, bufs: &mut Buffers, msg: &[u8]) {
        let (rx, verdict) = self.rx.on_message(msg);
        self.rx = rx;
        if let Verdict::Store {
            slot,
            offset,
            completed,
        } = verdict
        {
            let data = msg.get(middleware::stream::HEADER_LEN..).unwrap_or(&[]);
            let start = offset as usize;
            let dst = bufs
                .frames
                .get_mut(usize::from(slot))
                .and_then(|s| s.get_mut(start..start.saturating_add(data.len())));
            // A zip, not `copy_from_slice` (equal lengths by construction):
            // the payload links no panic path, so none of core's panic
            // formatting.
            if let Some(dst) = dst {
                dst.iter_mut().zip(data).for_each(|(d, s)| *d = *s);
            }
            if completed.is_some() {
                self.kick();
            }
        }
    }

    /// `tick`: logs and publishes the stats.
    pub fn report(&mut self) {
        let s = self.stats();
        if let Err(e) = mw_payload::publish(STATS_KEY, &s.encode()) {
            log_err("publish", e);
        }
        let mut line = LogLine::new();
        line.push_str("cam_jpeg_view: received ")
            .push_u32(s.received)
            .push_str(" decoded ")
            .push_u32(s.decoded)
            .push_str(" dropped ")
            .push_u32(s.dropped)
            .push_str(" failed ")
            .push_u32(s.failed)
            .push_str(" decode_ms ")
            .push_u32(s.decode_ms);
        line.log();
    }

    /// `on_irq`: bring-up events, decode timer and strip transfers.
    pub fn event(&mut self, bufs: &mut Buffers, event: Event) {
        let streaming = self.ctl.phase() == Phase::Streaming;
        match event {
            Event::Timer {
                id: DECODE_TIMER, ..
            } => {
                self.decode_armed = false;
                if streaming {
                    self.decode_step(bufs);
                }
            }
            Event::Timer {
                id: BRINGUP_TIMER, ..
            } => {
                let (ctl, cmd) = self.ctl.on(Input::Timer);
                self.run(ctl, cmd);
            }
            Event::DmaDone { .. } => {
                let Some(result) = self.hw.as_mut().and_then(|hw| hw.spi.on_event(&event)) else {
                    return;
                };
                if streaming {
                    let step = self.pipe.on_dma(result);
                    self.apply(bufs, step);
                } else {
                    let (ctl, cmd) = self.ctl.on(Input::Dma(result));
                    self.run(ctl, cmd);
                }
            }
            _ => {}
        }
    }

    /// One unit of decode work: start the newest frame, or one bounded
    /// decoder call.
    fn decode_step(&mut self, bufs: &mut Buffers) {
        let now = mw_payload::now_ms();
        let step = match self.pipe {
            Pipe::Idle => {
                let (rx, frame) = self.rx.take();
                self.rx = rx;
                let Some(frame) = frame else { return };
                let started = match frame_bytes(&bufs.frames, frame) {
                    Some(jpeg) => bufs.dec.start(jpeg, Scale::Full),
                    None => Err(Error::InputChanged),
                };
                match started {
                    Ok(()) => Pipe::begin(
                        frame,
                        now,
                        bufs.dec.output_width(),
                        bufs.dec.output_height(),
                    ),
                    Err(e) => (
                        Pipe::Idle,
                        Cmd::Failed {
                            frame,
                            why: pipeline::Failure::Decode(e),
                        },
                    ),
                }
            }
            Pipe::Decoding(job) => {
                let result = match frame_bytes(&bufs.frames, job.frame) {
                    Some(jpeg) => {
                        bufs.dec
                            .decode_mcus::<Rgb565Be>(jpeg, &mut bufs.strip, MCUS_PER_STEP)
                    }
                    None => Err(Error::InputChanged),
                };
                self.pipe.on_decode(result, now)
            }
            Pipe::Sending { .. } => return,
        };
        self.apply(bufs, step);
    }

    /// Takes a new pipeline state and carries out its command. A transfer
    /// that cannot start is fed back as a failed completion; that ends the
    /// frame, so at most two rounds run.
    fn apply(&mut self, bufs: &mut Buffers, (pipe, cmd): (Pipe, Cmd)) {
        self.pipe = pipe;
        let mut cmd = cmd;
        for _ in 0..2 {
            match cmd {
                Cmd::Nothing => return,
                Cmd::Continue => {
                    self.arm_decode();
                    return;
                }
                Cmd::Send(x) => match self.send(&bufs.strip, x) {
                    Ok(()) => return,
                    Err(e) => {
                        let (pipe, next) = self.pipe.on_dma(Err(e));
                        self.pipe = pipe;
                        cmd = next;
                    }
                },
                Cmd::Done { frame, ms } => {
                    self.stats.decoded = self.stats.decoded.saturating_add(1);
                    self.stats.decode_ms = ms;
                    log_frame(frame, "drawn", Some(ms));
                    self.rx = self.rx.done();
                    self.kick();
                    return;
                }
                Cmd::Failed { frame, .. } => {
                    self.stats.failed = self.stats.failed.saturating_add(1);
                    log_frame(frame, "failed", None);
                    self.rx = self.rx.done();
                    self.kick();
                    return;
                }
            }
        }
    }

    /// Starts one strip transfer: D/C, then the bytes.
    fn send(&mut self, strip: &[u8], x: Xfer) -> Result<(), AbiError> {
        let hw = self.hw.as_mut().ok_or(AbiError::Busy)?;
        hw.dc.set(x.dc())?;
        match x {
            Xfer::Command(c) => hw.spi.transfer(&[c], 0),
            Xfer::Params(p) => hw.spi.transfer(&p, 0),
            Xfer::Pixels { start, len } => {
                let px = strip
                    .get(start..start.saturating_add(len))
                    .ok_or(AbiError::OutOfBounds)?;
                hw.spi.transfer(px, 0)
            }
        }
    }
}

impl Default for CamJpegView {
    fn default() -> Self {
        CamJpegView::new()
    }
}

// The exports need a single-threaded wasm32 module (where a `GuestCell`
// may be a static); host builds only run the unit tests below.
#[cfg(target_arch = "wasm32")]
mod exports {
    use super::{Buffers, CamJpegView};
    use mw_payload::irq::Event;
    use mw_payload::{GuestCell, Payload, Slot};

    /// The large buffers (`.bss`).
    static BUFS: GuestCell<Buffers> = GuestCell::new(Buffers::EMPTY);

    impl Payload for CamJpegView {
        fn init() -> Self {
            CamJpegView::new()
        }

        fn tick(&mut self, _now_ms: u32) {
            self.report();
        }

        fn on_message(&mut self, _slot: Slot, data: &[u8]) {
            if let Some(mut bufs) = BUFS.borrow_mut() {
                self.receive(&mut bufs, data);
            }
        }

        fn on_irq(&mut self, event: Event) {
            if let Some(mut bufs) = BUFS.borrow_mut() {
                self.event(&mut bufs, event);
            }
        }
    }

    mw_payload::payload!(CamJpegView);
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::stream::{Chunker, ESPNOW_CHUNK_DATA, HEADER_LEN};
    use mw_jpeg::Step;

    fn fixture(i: usize) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../../fixtures/cam/jpeg240/frame_{i:02}.jpg"));
        std::fs::read(path).expect("fixture")
    }

    fn messages(seq: u16, jpeg: &[u8]) -> Vec<Vec<u8>> {
        let c = Chunker::new(seq, jpeg.len() as u32, ESPNOW_CHUNK_DATA).expect("valid");
        (0..c.count())
            .map(|i| {
                let plan = c.chunk(i).expect("in range");
                let data = &jpeg[plan.range];
                let mut out = vec![0u8; HEADER_LEN + data.len()];
                let n = plan.header.write(data, &mut out).expect("fits");
                out.truncate(n);
                out
            })
            .collect()
    }

    #[test]
    fn memory_fits_one_page_with_the_stack() {
        let big = SLOTS * FRAME_CAP + STRIP_BYTES + 4352;
        assert_eq!(big, 36_608);
        assert!(16 * 1024 + big + 4096 <= 64 * 1024);
        assert!(fixture_lens().iter().all(|&n| n <= FRAME_CAP));
    }

    /// `key = N` from the manifest's `[package.metadata.byte-pages]`.
    fn byte_pages(key: &str) -> usize {
        let manifest = include_str!("../Cargo.toml");
        let table = manifest
            .split("[package.metadata.byte-pages]")
            .nth(1)
            .expect("byte-pages table");
        table
            .lines()
            .filter_map(|l| l.split_once('='))
            .find(|(k, _)| k.trim() == key)
            .and_then(|(_, v)| v.trim().parse().ok())
            .expect(key)
    }

    #[test]
    fn deploy_build_memory_is_whole_kib_and_holds_buffers_and_stack() {
        let (memory, stack) = (byte_pages("memory"), byte_pages("stack"));
        assert_eq!((memory, stack), (48_128, 8_192), "update the crate docs");
        assert_eq!(memory % 1024, 0);
        // The linker checks the exact fit; this keeps the numbers honest
        // when the buffers change: stack, the buffers and 2 KiB of other
        // statics, with less than 1 KiB of rounding left over.
        let big = SLOTS * FRAME_CAP + STRIP_BYTES + 4352;
        assert!(stack + big + 2048 <= memory);
        assert!(memory < stack + big + 2048 + 1024 + 2048);
        assert!(memory < 64 * 1024, "smaller than the standard page");
    }

    fn fixture_lens() -> Vec<usize> {
        (0..8).map(|i| fixture(i).len()).collect()
    }

    #[test]
    fn stats_fold_the_reassembler_and_encode_as_le_words() {
        let mut app = CamJpegView::new();
        let mut bufs = Box::new(Buffers::EMPTY);
        for m in messages(1, &fixture(0)) {
            app.receive(&mut bufs, &m);
        }
        let s = app.stats();
        assert_eq!((s.received, s.decoded, s.dropped), (1, 0, 0));
        let s = ViewStats {
            received: 1,
            decoded: 2,
            dropped: 3,
            failed: 4,
            decode_ms: 0x0102_0304,
        };
        assert_eq!(
            s.encode(),
            [1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 4, 0, 0, 0, 4, 3, 2, 1]
        );
    }

    #[test]
    fn received_fixture_frames_decode_in_bounded_steps() {
        // The shell's data path without the host: reassemble a fixture
        // frame into a slot, then decode it with the payload's step size;
        // every strip fits the strip buffer and the panel.
        let mut app = CamJpegView::new();
        let mut bufs = Box::new(Buffers::EMPTY);
        let jpeg = fixture(3);
        for m in messages(9, &jpeg) {
            app.receive(&mut bufs, &m);
        }
        let (rx, frame) = app.rx.take();
        let frame = frame.expect("complete");
        assert_eq!(frame_bytes(&bufs.frames, frame), Some(&jpeg[..]));
        app.rx = rx;
        let Buffers { frames, strip, dec } = &mut *bufs;
        let bytes = frame_bytes(frames, frame).expect("slot");
        dec.start(bytes, Scale::Full).expect("header");
        assert!(dec.strip_bytes::<Rgb565Be>() <= STRIP_BYTES);
        let (mut pipe, _) = Pipe::begin(frame, 0, dec.output_width(), dec.output_height());
        let (mut calls, mut rows) = (0, 0);
        for _ in 0..1000 {
            let r = dec.decode_mcus::<Rgb565Be>(bytes, &mut strip[..], MCUS_PER_STEP);
            calls += 1;
            if matches!(r, Ok(Step::Row(_))) {
                rows += 1;
            }
            let done = matches!(r, Ok(Step::Done));
            let (p, cmd) = pipe.on_decode(r, 5);
            pipe = p;
            let mut cmd = cmd;
            while let Cmd::Send(x) = cmd {
                (pipe, cmd) = pipe.on_dma(Ok(x.bytes()));
            }
            if done {
                assert!(matches!(cmd, Cmd::Done { .. }));
                break;
            }
            assert_eq!(cmd, Cmd::Continue);
        }
        assert_eq!(rows, 15);
        assert_eq!(calls, 15 * 4 + 1, "four calls a row and one for Done");
    }

    #[test]
    fn host_stub_bring_up_backs_off_and_frames_still_reassemble() {
        // Off wasm32 every import is Unsupported: opening fails and the
        // controller backs off; nothing panics, and no decode starts.
        let mut app = CamJpegView::new();
        assert_eq!(app.phase(), Phase::Backoff);
        let mut bufs = Box::new(Buffers::EMPTY);
        for m in messages(1, &fixture(1)) {
            app.receive(&mut bufs, &m);
        }
        app.event(
            &mut bufs,
            Event::Timer {
                id: DECODE_TIMER,
                periods: 1,
                time_us: 0,
            },
        );
        assert!(app.pipe.is_idle(), "not streaming yet");
        assert_eq!(app.stats().received, 1);
        app.report();
    }
}
