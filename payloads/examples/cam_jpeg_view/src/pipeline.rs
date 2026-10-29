//! The decode-and-draw pipeline: a pure state machine that turns decoder
//! results and SPI completions into the next thing the shell must do.
//!
//! ```text
//!         ┌─begin, image not covering the lit area─▶ Sending(Black band) ─┐
//!         │                                     last band's last DmaDone   │
//! Idle ──begin──▶ Decoding ◀───────────────────────────────────────────────┘
//!   ▲              │  ▲ ──Row(strip)──▶ Sending(Strip, k = 0..)
//!   │              │  └─────────────── last DmaDone ◀┘
//!   └─Done / error─┘    (Partial re-arms the decode timer; any error ──▶ Idle)
//! ```
//!
//! * **Decoding**: each decode-timer event runs one bounded decoder call
//!   (`decode_mcus(.., MCUS_PER_STEP)` in the shell) and feeds its result
//!   to [`Pipe::on_decode`]. `Partial` re-arms the timer; a completed strip
//!   starts sending it; `Done` ends the frame.
//! * **Sending**: a window goes out as [`window_xfer`] transfers, one in
//!   flight at a time, each started from the previous one's `DmaDone`
//!   ([`Pipe::on_dma`]): `CASET` and `RASET` (command with D/C low, four
//!   parameter bytes with D/C high) for the window, `RAMWR`, then the
//!   window's RGB565 bytes with D/C high in parts of at most
//!   [`PIXEL_PART`] bytes (the ABI's `MAX_DMA` is 4 092): a strip's pixels
//!   from the strip buffer ([`Fill::Strip`]), or zeros ([`Fill::Black`],
//!   [`Xfer::Zeros`]). The decoder resumes only after the last part of a
//!   strip completed, because it writes the next strip into the same
//!   buffer.
//!
//! # Image size
//!
//! Every frame carries its own [`Size`] (the decoder's output size, read
//! after `Decoder::start` on that frame) in its [`Job`]; nothing about the
//! geometry outlives the frame. Each strip's window is computed from the
//! frame's size and the strip's own row, width and height
//! ([`Window::for_strip`]), so a frame may differ in size from the one
//! before (240×240, then 240×176, then 160×120 …), and a last strip shorter
//! than the MCU height (120 rows of 16-row MCUs end with an 8-row strip)
//! gets a window of its own height.
//!
//! The image is drawn centred on the [`PANEL`]×[`PANEL`] panel (column
//! `(PANEL - width) / 2`, row `(PANEL - height) / 2`); an image larger than
//! the panel in either direction is refused before decoding
//! ([`Failure::TooLarge`]). The panel outside the image must be black, and
//! the panel keeps whatever a larger earlier frame drew there, so
//! [`Pipe::begin`] takes the area that may be lit (the shell's record:
//! [`Size::NONE`] after the bring-up's black fill, a frame's size once it
//! began, [`Size::PANEL`] after a failure) and, when the new image does
//! not cover it, first fills the up to four bands around the image with
//! black ([`Size::band`]). A frame the size of the last one sends no black.

use gc9a01::{CASET, PANEL, RAMWR, RASET, window};
use mw_jpeg::{Error, Step, Strip};
use mw_payload::AbiError;

use crate::reasm::Frame;

/// Largest pixel transfer: 8 rows of a 240-wide strip, an even number of
/// bytes within the ABI's `MAX_DMA`.
pub const PIXEL_PART: usize = 8 * PANEL as usize * 2;
const _: () = assert!(PIXEL_PART <= mw_payload::spi::MAX_DMA && PIXEL_PART.is_multiple_of(2));

/// An image size in pixels; images are drawn centred on the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    /// Columns.
    pub width: u16,
    /// Rows.
    pub height: u16,
}

impl Size {
    /// No image: the whole panel is black.
    pub const NONE: Size = Size {
        width: 0,
        height: 0,
    };
    /// The whole panel: any pixel may be lit.
    pub const PANEL: Size = Size {
        width: PANEL,
        height: PANEL,
    };

    /// Whether the image fits the panel.
    pub const fn fits(self) -> bool {
        self.width <= PANEL && self.height <= PANEL
    }

    /// Whether this image, centred, covers `other` centred: centred
    /// rectangles nest, so it does when it is at least as wide and high.
    pub const fn covers(self, other: Size) -> bool {
        self.width >= other.width && self.height >= other.height
    }

    /// Band `i` of the panel outside this image centred: 0 the rows above
    /// it, 1 the rows below it, 2 and 3 the columns left and right of it
    /// beside its rows. `None` for an empty band, `i > 3` or an image that
    /// does not fit. The four bands and the image tile the panel.
    pub fn band(self, i: u8) -> Option<Window> {
        let left = PANEL.checked_sub(self.width)? / 2;
        let top = PANEL.checked_sub(self.height)? / 2;
        // One past the image; at most PANEL as the image fits.
        let (right, bottom) = (left + self.width, top + self.height);
        // Half open: columns `c0..c1`, rows `r0..r1`.
        let (c0, r0, c1, r1) = match i {
            0 => (0, 0, PANEL, top),
            1 => (0, bottom, PANEL, PANEL),
            2 => (0, top, left, bottom),
            3 => (right, top, PANEL, bottom),
            _ => return None,
        };
        // `c1 > c0 >= 0` and `r1 > r0 >= 0`, so the last ones exist.
        (c0 < c1 && r0 < r1).then(|| Window {
            x0: c0,
            y0: r0,
            x1: c1 - 1,
            y1: r1 - 1,
        })
    }
}

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
    /// The window of `strip` of an image of `size` drawn centred on the
    /// panel; `None` if the image does not fit, the strip is empty, or the
    /// strip lies outside the panel.
    pub fn for_strip(size: Size, strip: &Strip) -> Option<Window> {
        let Size { width, height } = size;
        if !size.fits() || strip.width == 0 || strip.height == 0 {
            return None;
        }
        let x0 = (PANEL - width) / 2;
        let y0 = ((PANEL - height) / 2).checked_add(strip.y)?;
        let x1 = x0.checked_add(strip.width - 1)?;
        let y1 = y0.checked_add(strip.height - 1)?;
        (x1 < PANEL && y1 < PANEL).then_some(Window { x0, y0, x1, y1 })
    }

    /// RGB565 bytes that fill the window (`x1 >= x0`, `y1 >= y0`).
    pub fn bytes(&self) -> usize {
        let w = usize::from(self.x1.saturating_sub(self.x0)) + 1;
        let h = usize::from(self.y1.saturating_sub(self.y0)) + 1;
        w * h * 2
    }
}

/// One SPI transfer of a window.
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
    /// `len` zero bytes (black pixels), D/C high.
    Zeros {
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
            Xfer::Pixels { len, .. } | Xfer::Zeros { len } => *len,
        }
    }
}

/// Transfer `k` of window `win` filled with `fill`; `None` after the last
/// one. The data parts cover the window's [`Window::bytes`]: from the
/// strip buffer ([`Xfer::Pixels`]) for [`Fill::Strip`], zeros
/// ([`Xfer::Zeros`]) for [`Fill::Black`].
pub fn window_xfer(win: &Window, k: u8, fill: Fill) -> Option<Xfer> {
    match k {
        0 => Some(Xfer::Command(CASET)),
        1 => Some(Xfer::Params(window(win.x0, win.x1))),
        2 => Some(Xfer::Command(RASET)),
        3 => Some(Xfer::Params(window(win.y0, win.y1))),
        4 => Some(Xfer::Command(RAMWR)),
        _ => {
            let start = usize::from(k - 5).checked_mul(PIXEL_PART)?;
            let len = win.bytes().checked_sub(start)?.min(PIXEL_PART);
            match fill {
                _ if len == 0 => None,
                Fill::Strip => Some(Xfer::Pixels { start, len }),
                Fill::Black { .. } => Some(Xfer::Zeros { len }),
            }
        }
    }
}

/// What a window being sent is filled with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fill {
    /// A decoded strip, from the strip buffer.
    Strip,
    /// Black: band `band` ([`Size::band`]) around the image.
    Black {
        /// The band, `0..4`.
        band: u8,
    },
}

/// The frame being decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Job {
    /// The frame's slot, length and sequence number.
    pub frame: Frame,
    /// `now_ms` when decoding started.
    pub started_ms: u32,
    /// Output size in pixels, of this frame.
    pub size: Size,
}

/// Why a frame was abandoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The decoder refused the frame.
    Decode(Error),
    /// The image is larger than the panel.
    TooLarge,
    /// A transfer failed or came up short.
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
    /// Sending a window: transfer `k` is in flight.
    Sending {
        /// The frame.
        job: Job,
        /// The panel window.
        win: Window,
        /// Index of the transfer in flight ([`window_xfer`]).
        k: u8,
        /// What the window is filled with.
        fill: Fill,
    },
}

impl Pipe {
    /// A decoder was started on `frame` with an output of `size`, and
    /// `lit` is the centred area of the panel that may be lit (see the
    /// module docs, "Image size"): refuse an image larger than the panel,
    /// black out the bands around one that does not cover `lit`, then
    /// decode.
    pub fn begin(frame: Frame, started_ms: u32, size: Size, lit: Size) -> (Pipe, Cmd) {
        if !size.fits() {
            let why = Failure::TooLarge;
            return (Pipe::Idle, Cmd::Failed { frame, why });
        }
        let job = Job {
            frame,
            started_ms,
            size,
        };
        // An image that covers the lit area has no band to black out.
        let from = if size.covers(lit) { 4 } else { 0 };
        Pipe::black(job, from)
    }

    /// Starts blacking out the first non-empty band from `from` on, or
    /// decoding when none is left. Out of line: called from two places.
    #[inline(never)]
    fn black(job: Job, from: u8) -> (Pipe, Cmd) {
        match (from..4).find_map(|i| Some((i, job.size.band(i)?))) {
            Some((band, win)) => {
                let fill = Fill::Black { band };
                let state = Pipe::Sending {
                    job,
                    win,
                    k: 0,
                    fill,
                };
                (state, Cmd::Send(Xfer::Command(CASET)))
            }
            None => (Pipe::Decoding(job), Cmd::Continue),
        }
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
            Ok(Step::Row(strip)) => match Window::for_strip(job.size, &strip) {
                Some(win) => {
                    // The window is the strip's size: `win.bytes()` is
                    // `strip.bytes(2)`.
                    let state = Pipe::Sending {
                        job,
                        win,
                        k: 0,
                        fill: Fill::Strip,
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
    /// After a strip's last transfer decoding resumes; after a black
    /// band's, the next band starts, or decoding when none is left.
    pub fn on_dma(self, result: Result<usize, AbiError>) -> (Pipe, Cmd) {
        let Pipe::Sending { job, win, k, fill } = self else {
            return (self, Cmd::Nothing);
        };
        let expected = window_xfer(&win, k, fill).map(|x| x.bytes());
        let failed = |e| {
            let why = Failure::Spi(e);
            let frame = job.frame;
            (Pipe::Idle, Cmd::Failed { frame, why })
        };
        match result {
            Ok(n) if Some(n) == expected => {
                let k = k.saturating_add(1);
                let next = Pipe::Sending { job, win, k, fill };
                match (window_xfer(&win, k, fill), fill) {
                    (Some(x), _) => (next, Cmd::Send(x)),
                    (None, Fill::Strip) => (Pipe::Decoding(job), Cmd::Continue),
                    (None, Fill::Black { band }) => Pipe::black(job, band.saturating_add(1)),
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

    const fn size(width: u16, height: u16) -> Size {
        Size { width, height }
    }

    fn strip(y: u16, height: u16) -> Strip {
        Strip {
            y,
            height,
            width: 240,
        }
    }

    /// Runs a window's transfers to completion; returns them.
    fn send_all(mut pipe: Pipe, first: Cmd) -> (Pipe, Vec<Xfer>, Cmd) {
        let mut out = Vec::new();
        let mut cmd = first;
        for _ in 0..1000 {
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

    /// The windows (`CASET` and `RASET` parameters) in `xfers`, with the
    /// pixel and zero bytes that followed each.
    fn windows(xfers: &[Xfer]) -> Vec<(Window, usize, usize)> {
        let mut out: Vec<(Window, usize, usize)> = Vec::new();
        let mut cols = (0, 0);
        for pair in xfers.windows(2) {
            match pair {
                [Xfer::Command(CASET), Xfer::Params(p)] => {
                    cols = (
                        u16::from_be_bytes([p[0], p[1]]),
                        u16::from_be_bytes([p[2], p[3]]),
                    )
                }
                [Xfer::Command(RASET), Xfer::Params(p)] => out.push((
                    Window {
                        x0: cols.0,
                        x1: cols.1,
                        y0: u16::from_be_bytes([p[0], p[1]]),
                        y1: u16::from_be_bytes([p[2], p[3]]),
                    },
                    0,
                    0,
                )),
                _ => {}
            }
        }
        // Attribute the data transfers to the window before them.
        let mut w = 0;
        let mut seen = 0;
        for x in xfers {
            match x {
                Xfer::Command(RAMWR) => {
                    seen += 1;
                    w = seen - 1;
                }
                Xfer::Pixels { len, .. } => out[w].1 += len,
                Xfer::Zeros { len } => out[w].2 += len,
                _ => {}
            }
        }
        out
    }

    /// Drives one frame of `size` through the pipeline with synthetic
    /// decoder results (a strip per `mcu` rows, `Partial` before each, as
    /// `decode_mcus` reports them), the panel's `lit` area as the shell
    /// tracks it; returns every transfer and the end command.
    fn frame(size: Size, mcu: u16, lit: Size) -> (Vec<Xfer>, Cmd) {
        let (mut pipe, mut cmd) = Pipe::begin(FRAME, 0, size, lit);
        let mut xfers = Vec::new();
        let mut y = 0;
        for _ in 0..1000 {
            match cmd {
                Cmd::Send(_) => {
                    let (p, x, c) = send_all(pipe, cmd);
                    (pipe, cmd) = (p, c);
                    xfers.extend(x);
                }
                Cmd::Continue if y >= size.height => {
                    (pipe, cmd) = pipe.on_decode(Ok(Step::Done), 9)
                }
                Cmd::Continue => {
                    let (p, c) = pipe.on_decode(Ok(Step::Partial), 1);
                    assert_eq!(c, Cmd::Continue);
                    let height = mcu.min(size.height - y);
                    let s = Strip {
                        y,
                        height,
                        width: size.width,
                    };
                    y += height;
                    (pipe, cmd) = p.on_decode(Ok(Step::Row(s)), 1);
                }
                other => {
                    assert!(pipe.is_idle());
                    return (xfers, other);
                }
            }
        }
        panic!("did not finish");
    }

    #[test]
    fn a_full_width_strip_is_window_ramwr_and_two_parts() {
        let (pipe, cmd) = Pipe::begin(FRAME, 100, size(240, 240), Size::NONE);
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
        let w = Window::for_strip(size(120, 120), &s).expect("fits");
        assert_eq!(
            w,
            Window {
                x0: 60,
                y0: 60,
                x1: 179,
                y1: 67
            }
        );
        assert_eq!(Window::for_strip(size(241, 10), &s), None);
        assert_eq!(
            Window::for_strip(size(240, 240), &strip(232, 16)),
            None,
            "past the bottom"
        );
        // Any window splits into bounded parts that cover it exactly, as
        // pixels or as zeros; the whole panel too.
        for (x1, y1) in [
            (0, 0),
            (1, 0),
            (239, 7),
            (239, 15),
            (119, 31),
            (239, 119),
            (239, 239),
        ] {
            let w = Window {
                x0: 0,
                y0: 0,
                x1,
                y1,
            };
            for fill in [Fill::Strip, Fill::Black { band: 0 }] {
                let parts: Vec<Xfer> = (5..=u8::MAX)
                    .map_while(|k| window_xfer(&w, k, fill))
                    .collect();
                let mut next = 0;
                for p in &parts {
                    let len = match (*p, fill) {
                        (Xfer::Pixels { start, len }, Fill::Strip) => {
                            assert_eq!(start, next);
                            len
                        }
                        (Xfer::Zeros { len }, Fill::Black { .. }) => len,
                        _ => panic!("{p:?} for {fill:?}"),
                    };
                    assert!(len > 0 && len <= PIXEL_PART && len <= mw_payload::spi::MAX_DMA);
                    next += len;
                }
                assert_eq!(next, w.bytes());
                assert_eq!(next, (usize::from(x1) + 1) * (usize::from(y1) + 1) * 2);
            }
        }
    }

    #[test]
    fn bands_and_image_tile_the_panel_exactly() {
        // For every size that fits, each panel pixel is in exactly one of
        // the image and its four bands; nothing for a size that does not.
        for (w, h) in [
            (240, 240),
            (240, 176),
            (160, 120),
            (200, 150),
            (120, 120),
            (239, 1),
            (1, 239),
            (1, 1),
            (0, 0),
        ] {
            let sz = size(w, h);
            let mut hits = vec![0u8; 240 * 240];
            let img = Window::for_strip(
                sz,
                &Strip {
                    y: 0,
                    height: h,
                    width: w,
                },
            );
            let bands = (0..=4).filter_map(|i| sz.band(i));
            for win in img.into_iter().chain(bands) {
                assert!(win.x1 < 240 && win.y1 < 240);
                for y in win.y0..=win.y1 {
                    for x in win.x0..=win.x1 {
                        hits[usize::from(y) * 240 + usize::from(x)] += 1;
                    }
                }
            }
            assert!(hits.iter().all(|&n| n == 1), "{w}x{h}");
            assert_eq!(sz.band(4), None);
        }
        assert_eq!((0..4).filter_map(|i| size(241, 10).band(i)).count(), 0);
        assert_eq!((0..4).filter_map(|i| Size::PANEL.band(i)).count(), 0);
        let band = |w, h, i| size(w, h).band(i);
        let win = |x0, y0, x1, y1| Some(Window { x0, y0, x1, y1 });
        // HQVGA: 32 rows above and below, nothing beside.
        assert_eq!(band(240, 176, 0), win(0, 0, 239, 31));
        assert_eq!(band(240, 176, 1), win(0, 208, 239, 239));
        assert_eq!((band(240, 176, 2), band(240, 176, 3)), (None, None));
        // QQVGA: 60 rows above and below, 40 columns beside.
        assert_eq!(band(160, 120, 0), win(0, 0, 239, 59));
        assert_eq!(band(160, 120, 1), win(0, 180, 239, 239));
        assert_eq!(band(160, 120, 2), win(0, 60, 39, 179));
        assert_eq!(band(160, 120, 3), win(200, 60, 239, 179));
        // Odd margins: the extra row or column goes below and right.
        assert_eq!(band(239, 175, 0), win(0, 0, 239, 31));
        assert_eq!(band(239, 175, 1), win(0, 207, 239, 239));
        assert_eq!(band(239, 175, 2), None);
        assert_eq!(band(239, 175, 3), win(239, 32, 239, 206));
    }

    #[test]
    fn a_smaller_frame_after_a_larger_one_blacks_out_its_border_first() {
        // 240×176 after 240×240: the 32 rows above and below still show the
        // old frame, so they are zeroed (four 3 840-byte parts each) before
        // the first decoder call.
        let (pipe, cmd) = Pipe::begin(FRAME, 0, size(240, 176), size(240, 240));
        let (pipe, xfers, end) = send_all(pipe, cmd);
        let zeros = Xfer::Zeros { len: 3840 };
        let mut want = vec![
            Xfer::Command(CASET),
            Xfer::Params([0, 0, 0, 239]),
            Xfer::Command(RASET),
            Xfer::Params([0, 0, 0, 31]),
            Xfer::Command(RAMWR),
        ];
        want.extend([zeros; 4]);
        want.extend([
            Xfer::Command(CASET),
            Xfer::Params([0, 0, 0, 239]),
            Xfer::Command(RASET),
            Xfer::Params([0, 208, 0, 239]),
            Xfer::Command(RAMWR),
        ]);
        want.extend([zeros; 4]);
        assert_eq!(xfers, want);
        assert!(xfers.iter().all(|x| x.bytes() <= mw_payload::spi::MAX_DMA));
        assert_eq!(end, Cmd::Continue);
        assert!(matches!(pipe, Pipe::Decoding(j) if j.size == size(240, 176)));
        // A frame that covers what may be lit sends no black at all.
        for lit in [Size::NONE, size(240, 176), size(200, 150)] {
            let (p, c) = Pipe::begin(FRAME, 0, size(240, 176), lit);
            assert_eq!(c, Cmd::Continue, "{lit:?}");
            assert!(matches!(p, Pipe::Decoding(_)));
        }
        // After a failure the shell passes the whole panel.
        let (_, xfers, _) = {
            let (p, c) = Pipe::begin(FRAME, 0, size(160, 120), Size::PANEL);
            send_all(p, c)
        };
        assert_eq!(
            windows(&xfers).iter().map(|w| w.2).sum::<usize>(),
            (240 * 240 - 160 * 120) * 2
        );
    }

    #[test]
    fn size_changes_between_frames_draw_each_frame_in_its_own_window() {
        // The camera's frames change size from one to the next. Each frame
        // gets its own windows (nothing kept from the frame before), its
        // own strip count (a short last strip for 120 rows of 16-row
        // MCUs), black bands only where an earlier, larger frame may have
        // drawn, and completes.
        let mut lit = Size::NONE;
        let full = |y0, y1| Window {
            x0: 0,
            y0,
            x1: 239,
            y1,
        };
        let seq = [
            (
                size(240, 240),
                16,
                (0..15)
                    .map(|i| full(16 * i, 16 * i + 15))
                    .collect::<Vec<_>>(),
                vec![],
            ),
            (
                size(240, 176),
                16,
                (0..11)
                    .map(|i| full(32 + 16 * i, 32 + 16 * i + 15))
                    .collect(),
                vec![full(0, 31), full(208, 239)],
            ),
            (
                size(240, 240),
                16,
                (0..15).map(|i| full(16 * i, 16 * i + 15)).collect(),
                vec![],
            ),
            (
                size(160, 120),
                16,
                (0..8)
                    .map(|i| Window {
                        x0: 40,
                        y0: 60 + 16 * i,
                        x1: 199,
                        y1: (60 + 16 * i + 15).min(179),
                    })
                    .collect(),
                vec![
                    full(0, 59),
                    full(180, 239),
                    Window {
                        x0: 0,
                        y0: 60,
                        x1: 39,
                        y1: 179,
                    },
                    Window {
                        x0: 200,
                        y0: 60,
                        x1: 239,
                        y1: 179,
                    },
                ],
            ),
            // The OV2640's 4:2:2 (8-row MCUs), odd margins and a 6-row
            // last strip: inside the 160×120 one's border plus more.
            (
                size(200, 150),
                8,
                (0..19)
                    .map(|i| Window {
                        x0: 20,
                        y0: 45 + 8 * i,
                        x1: 219,
                        y1: (45 + 8 * i + 7).min(194),
                    })
                    .collect(),
                vec![],
            ),
            (
                size(120, 120),
                16,
                (0..8)
                    .map(|i| Window {
                        x0: 60,
                        y0: 60 + 16 * i,
                        x1: 179,
                        y1: (60 + 16 * i + 15).min(179),
                    })
                    .collect(),
                vec![
                    full(0, 59),
                    full(180, 239),
                    Window {
                        x0: 0,
                        y0: 60,
                        x1: 59,
                        y1: 179,
                    },
                    Window {
                        x0: 180,
                        y0: 60,
                        x1: 239,
                        y1: 179,
                    },
                ],
            ),
        ];
        for (sz, mcu, strips, black) in seq {
            let (xfers, end) = frame(sz, mcu, lit);
            // What the shell does when a frame begins.
            lit = sz;
            assert!(
                matches!(end, Cmd::Done { frame: FRAME, .. }),
                "{sz:?}: {end:?}"
            );
            let wins = windows(&xfers);
            let (zeroed, drawn): (Vec<&(Window, usize, usize)>, Vec<_>) =
                wins.iter().partition(|w| w.2 > 0);
            assert_eq!(
                zeroed.iter().map(|w| w.0).collect::<Vec<_>>(),
                black,
                "{sz:?}"
            );
            assert_eq!(
                drawn.iter().map(|w| w.0).collect::<Vec<_>>(),
                strips,
                "{sz:?}"
            );
            for &(w, px, z) in &wins {
                assert_eq!(
                    px + z,
                    w.bytes(),
                    "{sz:?} {w:?}: every window filled exactly"
                );
            }
            let pixels: usize = drawn.iter().map(|w| w.1).sum();
            assert_eq!(pixels, usize::from(sz.width) * usize::from(sz.height) * 2);
        }
        assert_eq!(lit, size(120, 120));
    }

    #[test]
    fn a_frame_larger_than_the_panel_is_refused_and_the_next_one_drawn() {
        for big in [size(320, 240), size(240, 241), size(1600, 1200)] {
            assert_eq!(
                Pipe::begin(FRAME, 0, big, size(240, 240)),
                (
                    Pipe::Idle,
                    Cmd::Failed {
                        frame: FRAME,
                        why: Failure::TooLarge
                    }
                )
            );
        }
        // After a failure the shell records the whole panel as lit.
        let (xfers, end) = frame(size(240, 176), 16, Size::PANEL);
        assert!(matches!(end, Cmd::Done { .. }));
        assert_eq!(windows(&xfers).len(), 2 + 11);
    }

    #[test]
    fn errors_abandon_the_frame() {
        let (pipe, _) = Pipe::begin(FRAME, 0, size(240, 240), Size::NONE);
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
        // So does a failed or short transfer of a black band.
        let (black, _) = Pipe::begin(FRAME, 0, size(240, 176), Size::PANEL);
        assert_eq!(black.on_dma(Err(AbiError::Busy)), failed(AbiError::Busy));
        let (black, _) = black.on_dma(Ok(1));
        assert_eq!(black.on_dma(Ok(3)), failed(AbiError::Transport));
    }

    #[test]
    fn events_out_of_place_are_ignored() {
        assert_eq!(Pipe::Idle.on_dma(Ok(1)), (Pipe::Idle, Cmd::Nothing));
        assert_eq!(
            Pipe::Idle.on_decode(Ok(Step::Done), 0),
            (Pipe::Idle, Cmd::Nothing)
        );
        let (pipe, _) = Pipe::begin(FRAME, 0, size(240, 240), Size::NONE);
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
        let (black, _) = Pipe::begin(FRAME, 0, size(160, 120), Size::PANEL);
        assert_eq!(
            black.on_decode(Ok(Step::Partial), 0),
            (black, Cmd::Nothing),
            "no decoding while the border is blacked out"
        );
    }
}
