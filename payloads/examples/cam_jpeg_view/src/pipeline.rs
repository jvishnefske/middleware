//! The decode-and-draw pipeline: a pure state machine that turns decoder
//! results and SPI completions into the next thing the shell must do.
//!
//! ```text
//! Idle ──begin──▶ Decoding ──Row(strip)──▶ Sending(k = 0..) ──last DmaDone──▶ Decoding
//!   ▲                │  ▲ Partial (re-arm the decode timer)       │
//!   └────Done / error┘  └───────────────────────────────────────┘ DMA error ──▶ Idle
//! ```
//!
//! * **Decoding**: each decode-timer event runs one bounded decoder call
//!   (`decode_mcus(.., MCUS_PER_STEP)` in the shell) and feeds its result
//!   to [`Pipe::on_decode`]. `Partial` re-arms the timer; a completed strip
//!   starts sending it; `Done` ends the frame.
//! * **Sending**: a strip goes out as [`strip_xfer`] transfers, one in
//!   flight at a time, each started from the previous one's `DmaDone`
//!   ([`Pipe::on_dma`]): `CASET` and `RASET` (command with D/C low, four
//!   parameter bytes with D/C high) for the strip's window, `RAMWR`, then
//!   the RGB565 pixels with D/C high in parts of at most [`PIXEL_PART`]
//!   bytes (the ABI's `MAX_DMA` is 4 092). The decoder resumes only after
//!   the last part completed, because it writes the next strip into the
//!   same buffer.
//!
//! The image is drawn centred on the [`PANEL`]×[`PANEL`] panel; an image
//! larger than the panel is refused before decoding.

use gc9a01::{CASET, PANEL, RAMWR, RASET, window};
use mw_jpeg::{Error, Step, Strip};
use mw_payload::AbiError;

use crate::reasm::Frame;

/// Largest pixel transfer: 8 rows of a 240-wide strip, an even number of
/// bytes within the ABI's `MAX_DMA`.
pub const PIXEL_PART: usize = 8 * PANEL as usize * 2;
const _: () = assert!(PIXEL_PART <= mw_payload::spi::MAX_DMA && PIXEL_PART.is_multiple_of(2));

/// A panel window, inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// First column.
    pub x0: u16,
    /// First row.
    pub y0: u16,
    /// Last column.
    pub x1: u16,
    /// Last row.
    pub y1: u16,
}

impl Window {
    /// The window of `strip` of an image of `width`×`height` drawn centred
    /// on the panel; `None` if the image does not fit or the strip is empty.
    pub fn for_strip(width: u16, height: u16, strip: &Strip) -> Option<Window> {
        if width > PANEL || height > PANEL || strip.width == 0 || strip.height == 0 {
            return None;
        }
        let x0 = (PANEL - width) / 2;
        let y0 = (PANEL - height) / 2 + strip.y;
        let x1 = x0.checked_add(strip.width - 1)?;
        let y1 = y0.checked_add(strip.height - 1)?;
        (x1 < PANEL && y1 < PANEL).then_some(Window { x0, y0, x1, y1 })
    }
}

/// One SPI transfer of a strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Xfer {
    /// A command byte, D/C low.
    Command(u8),
    /// Four parameter bytes, D/C high.
    Params([u8; 4]),
    /// `len` bytes of the strip buffer from `start`, D/C high.
    Pixels {
        /// First byte in the strip buffer.
        start: usize,
        /// Bytes, `1..=PIXEL_PART`.
        len: usize,
    },
}

impl Xfer {
    /// The D/C level: `false` for a command.
    pub const fn dc(&self) -> bool {
        !matches!(self, Xfer::Command(_))
    }

    /// Bytes clocked.
    pub const fn bytes(&self) -> usize {
        match self {
            Xfer::Command(_) => 1,
            Xfer::Params(_) => 4,
            Xfer::Pixels { len, .. } => *len,
        }
    }
}

/// Transfer `k` of a strip of `bytes` pixel bytes drawn in `win`; `None`
/// after the last one.
pub fn strip_xfer(win: &Window, bytes: usize, k: u8) -> Option<Xfer> {
    match k {
        0 => Some(Xfer::Command(CASET)),
        1 => Some(Xfer::Params(window(win.x0, win.x1))),
        2 => Some(Xfer::Command(RASET)),
        3 => Some(Xfer::Params(window(win.y0, win.y1))),
        4 => Some(Xfer::Command(RAMWR)),
        _ => {
            let start = usize::from(k - 5).checked_mul(PIXEL_PART)?;
            (start < bytes).then(|| Xfer::Pixels {
                start,
                len: (bytes - start).min(PIXEL_PART),
            })
        }
    }
}

/// The frame being decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Job {
    /// The frame's slot, length and sequence number.
    pub frame: Frame,
    /// `now_ms` when decoding started.
    pub started_ms: u32,
    /// Output width in pixels.
    pub width: u16,
    /// Output height in pixels.
    pub height: u16,
}

/// Why a frame was abandoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The decoder refused the frame.
    Decode(Error),
    /// The image is larger than the panel.
    TooLarge,
    /// A strip transfer failed or came up short.
    Spi(AbiError),
}

/// What the shell must do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    /// Nothing.
    Nothing,
    /// Arm the decode timer for the next decoder call.
    Continue,
    /// Set D/C to [`Xfer::dc`] and start this transfer.
    Send(Xfer),
    /// The frame is drawn, `ms` after decoding started.
    Done {
        /// The frame.
        frame: Frame,
        /// Decode and draw time in milliseconds.
        ms: u32,
    },
    /// The frame was abandoned.
    Failed {
        /// The frame.
        frame: Frame,
        /// Why.
        why: Failure,
    },
}

/// The pipeline state; see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pipe {
    /// No frame in hand.
    Idle,
    /// Decoding `Job`; the decode timer is (to be) armed.
    Decoding(Job),
    /// Sending a strip: transfer `k` is in flight.
    Sending {
        /// The frame.
        job: Job,
        /// The strip's panel window.
        win: Window,
        /// The strip's pixel bytes.
        bytes: usize,
        /// Index of the transfer in flight ([`strip_xfer`]).
        k: u8,
    },
}

impl Pipe {
    /// A decoder was started on `frame` with an output of
    /// `width`×`height`: decode it, unless it does not fit the panel.
    pub fn begin(frame: Frame, started_ms: u32, width: u16, height: u16) -> (Pipe, Cmd) {
        if width > PANEL || height > PANEL {
            let why = Failure::TooLarge;
            return (Pipe::Idle, Cmd::Failed { frame, why });
        }
        let job = Job {
            frame,
            started_ms,
            width,
            height,
        };
        (Pipe::Decoding(job), Cmd::Continue)
    }

    /// Whether no frame is in hand.
    pub const fn is_idle(&self) -> bool {
        matches!(self, Pipe::Idle)
    }

    /// Feeds the result of one decoder call made while
    /// [`Pipe::Decoding`]; ignored in any other state.
    pub fn on_decode(self, result: Result<Step, Error>, now_ms: u32) -> (Pipe, Cmd) {
        let Pipe::Decoding(job) = self else {
            return (self, Cmd::Nothing);
        };
        let frame = job.frame;
        match result {
            Ok(Step::Partial) => (self, Cmd::Continue),
            Ok(Step::Row(strip)) => match Window::for_strip(job.width, job.height, &strip) {
                Some(win) => {
                    let bytes = strip.bytes(2);
                    let state = Pipe::Sending {
                        job,
                        win,
                        bytes,
                        k: 0,
                    };
                    (state, Cmd::Send(Xfer::Command(CASET)))
                }
                None => (
                    Pipe::Idle,
                    Cmd::Failed {
                        frame,
                        why: Failure::TooLarge,
                    },
                ),
            },
            Ok(Step::Done) => (
                Pipe::Idle,
                Cmd::Done {
                    frame,
                    ms: now_ms.wrapping_sub(job.started_ms),
                },
            ),
            Err(e) => (
                Pipe::Idle,
                Cmd::Failed {
                    frame,
                    why: Failure::Decode(e),
                },
            ),
        }
    }

    /// Feeds the completion of the transfer in flight (or the error of one
    /// that could not start) while [`Pipe::Sending`]; ignored otherwise.
    pub fn on_dma(self, result: Result<usize, AbiError>) -> (Pipe, Cmd) {
        let Pipe::Sending { job, win, bytes, k } = self else {
            return (self, Cmd::Nothing);
        };
        let expected = strip_xfer(&win, bytes, k).map(|x| x.bytes());
        let failed = |e| {
            let why = Failure::Spi(e);
            let frame = job.frame;
            (Pipe::Idle, Cmd::Failed { frame, why })
        };
        match result {
            Ok(n) if Some(n) == expected => {
                let k = k.saturating_add(1);
                match strip_xfer(&win, bytes, k) {
                    Some(x) => (Pipe::Sending { job, win, bytes, k }, Cmd::Send(x)),
                    None => (Pipe::Decoding(job), Cmd::Continue),
                }
            }
            Ok(_) => failed(AbiError::Transport),
            Err(e) => failed(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: Frame = Frame {
        slot: 2,
        len: 4000,
        seq: 7,
    };

    fn strip(y: u16, height: u16) -> Strip {
        Strip {
            y,
            height,
            width: 240,
        }
    }

    /// Runs a strip's transfers to completion; returns them.
    fn send_all(mut pipe: Pipe, first: Cmd) -> (Pipe, Vec<Xfer>, Cmd) {
        let mut out = Vec::new();
        let mut cmd = first;
        for _ in 0..100 {
            match cmd {
                Cmd::Send(x) => {
                    out.push(x);
                    (pipe, cmd) = pipe.on_dma(Ok(x.bytes()));
                }
                other => return (pipe, out, other),
            }
        }
        panic!("did not finish");
    }

    #[test]
    fn a_full_width_strip_is_window_ramwr_and_two_parts() {
        let (pipe, cmd) = Pipe::begin(FRAME, 100, 240, 240);
        assert_eq!(cmd, Cmd::Continue);
        let (pipe, cmd) = pipe.on_decode(Ok(Step::Partial), 110);
        assert_eq!(cmd, Cmd::Continue);
        let (pipe, cmd) = pipe.on_decode(Ok(Step::Row(strip(16, 16))), 120);
        let (pipe, xfers, end) = send_all(pipe, cmd);
        assert_eq!(
            xfers,
            vec![
                Xfer::Command(0x2A),
                Xfer::Params([0, 0, 0, 239]),
                Xfer::Command(0x2B),
                Xfer::Params([0, 16, 0, 31]),
                Xfer::Command(0x2C),
                Xfer::Pixels {
                    start: 0,
                    len: 3840
                },
                Xfer::Pixels {
                    start: 3840,
                    len: 3840
                },
            ]
        );
        assert_eq!(
            xfers.iter().map(Xfer::dc).collect::<Vec<_>>(),
            [false, true, false, true, false, true, true]
        );
        assert_eq!(end, Cmd::Continue, "back to decoding");
        assert!(matches!(pipe, Pipe::Decoding(_)));
        let (pipe, cmd) = pipe.on_decode(Ok(Step::Done), 150);
        assert_eq!(
            cmd,
            Cmd::Done {
                frame: FRAME,
                ms: 50
            }
        );
        assert!(pipe.is_idle());
    }

    #[test]
    fn smaller_images_are_centred_and_parts_never_exceed_max_dma() {
        let s = Strip {
            y: 0,
            height: 8,
            width: 120,
        };
        let w = Window::for_strip(120, 120, &s).expect("fits");
        assert_eq!(
            w,
            Window {
                x0: 60,
                y0: 60,
                x1: 179,
                y1: 67
            }
        );
        assert_eq!(Window::for_strip(241, 10, &s), None);
        assert_eq!(
            Window::for_strip(240, 240, &strip(232, 16)),
            None,
            "past the bottom"
        );
        // Any strip size splits into bounded parts that cover it exactly.
        for bytes in [1, 2, 3839, 3840, 3841, 7680, 10240] {
            let parts: Vec<Xfer> = (5..=u8::MAX)
                .map_while(|k| strip_xfer(&w, bytes, k))
                .collect();
            let mut next = 0;
            for p in &parts {
                let Xfer::Pixels { start, len } = *p else {
                    panic!("{p:?}")
                };
                assert_eq!(start, next);
                assert!(len > 0 && len <= PIXEL_PART && len <= mw_payload::spi::MAX_DMA);
                next += len;
            }
            assert_eq!(next, bytes);
        }
    }

    #[test]
    fn errors_abandon_the_frame() {
        let (pipe, _) = Pipe::begin(FRAME, 0, 240, 240);
        let e = Error::Truncated;
        assert_eq!(
            pipe.on_decode(Err(e), 1),
            (
                Pipe::Idle,
                Cmd::Failed {
                    frame: FRAME,
                    why: Failure::Decode(e)
                }
            )
        );
        assert_eq!(
            Pipe::begin(FRAME, 0, 320, 240),
            (
                Pipe::Idle,
                Cmd::Failed {
                    frame: FRAME,
                    why: Failure::TooLarge
                }
            )
        );
        let (sending, cmd) = pipe.on_decode(Ok(Step::Row(strip(0, 16))), 1);
        assert_eq!(cmd, Cmd::Send(Xfer::Command(0x2A)));
        let failed = |why| {
            (
                Pipe::Idle,
                Cmd::Failed {
                    frame: FRAME,
                    why: Failure::Spi(why),
                },
            )
        };
        assert_eq!(sending.on_dma(Err(AbiError::Busy)), failed(AbiError::Busy));
        assert_eq!(sending.on_dma(Ok(2)), failed(AbiError::Transport), "short");
    }

    #[test]
    fn events_out_of_place_are_ignored() {
        assert_eq!(Pipe::Idle.on_dma(Ok(1)), (Pipe::Idle, Cmd::Nothing));
        assert_eq!(
            Pipe::Idle.on_decode(Ok(Step::Done), 0),
            (Pipe::Idle, Cmd::Nothing)
        );
        let (pipe, _) = Pipe::begin(FRAME, 0, 240, 240);
        assert_eq!(
            pipe.on_dma(Ok(1)),
            (pipe, Cmd::Nothing),
            "no transfer in flight"
        );
        let (sending, _) = pipe.on_decode(Ok(Step::Row(strip(0, 16))), 0);
        assert_eq!(
            sending.on_decode(Ok(Step::Partial), 0),
            (sending, Cmd::Nothing),
            "no decoding while a strip is sent"
        );
    }
}
