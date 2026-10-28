//! A virtual GC9A01-like SPI panel (requirement R10): the display a sink
//! route draws on in the simulator.
//!
//! The model interprets the byte stream a MIPI-DCS panel sees, each byte
//! with its D/C level (`false` = command, `true` = data):
//!
//! * `CASET` (0x2A) and `RASET` (0x2B) take four data bytes, `start` and
//!   `end` as big-endian `u16`, and set the column or row window. A window
//!   whose start is past its end is ignored (the previous one stays).
//!   Extra data bytes are ignored.
//! * `RAMWR` (0x2C) moves the write cursor to the window's top-left corner;
//!   the data bytes that follow are RGB565 big-endian pixels written left to
//!   right, top to bottom inside the window, wrapping back to the top-left
//!   corner after the last pixel. Pixels outside the
//!   [`WIDTH`]×[`HEIGHT`] panel are discarded.
//! * Every other command, and its data, is ignored; any command ends a
//!   memory write (a dangling high byte of a pixel is discarded).
//!
//! The framebuffer starts black (RGB565 0x0000) and a full-panel window.
//! The model remembers which pixels were ever written, so a test
//! (`Panel::written`, test builds only, like `Panel::pixel`) can tell
//! untouched memory from a black pixel. [`Panel::to_ppm`] renders the
//! framebuffer as a binary PPM (P6) image.

/// Panel width in pixels.
pub const WIDTH: u16 = 240;
/// Panel height in pixels.
pub const HEIGHT: u16 = 240;

/// MIPI-DCS column address set.
pub const CASET: u8 = 0x2A;
/// MIPI-DCS row address set.
pub const RASET: u8 = 0x2B;
/// MIPI-DCS memory write.
pub const RAMWR: u8 = 0x2C;

const PIXELS: usize = WIDTH as usize * HEIGHT as usize;

/// An inclusive `start..=end` address range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    start: u16,
    end: u16,
}

/// The panel's controller state and framebuffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Panel {
    pixels: Vec<u16>,
    written: Vec<bool>,
    cols: Span,
    rows: Span,
    /// The command whose data bytes are arriving.
    command: Option<u8>,
    /// Parameter bytes collected for `CASET` / `RASET`.
    params: [u8; 4],
    params_len: usize,
    /// Write cursor during `RAMWR`.
    x: u16,
    y: u16,
    /// High byte of a pixel waiting for its low byte.
    high: Option<u8>,
    /// Pixels written (including discarded off-panel ones).
    pixel_writes: u64,
}

impl Default for Panel {
    fn default() -> Self {
        Panel::new()
    }
}

impl Panel {
    /// A black panel with a full-screen window.
    pub fn new() -> Self {
        Panel {
            pixels: vec![0; PIXELS],
            written: vec![false; PIXELS],
            cols: Span {
                start: 0,
                end: WIDTH - 1,
            },
            rows: Span {
                start: 0,
                end: HEIGHT - 1,
            },
            command: None,
            params: [0; 4],
            params_len: 0,
            x: 0,
            y: 0,
            high: None,
            pixel_writes: 0,
        }
    }

    /// Feeds `bytes` sent with the D/C pin at `dc`.
    pub fn write(&mut self, dc: bool, bytes: &[u8]) {
        for &b in bytes {
            if dc {
                self.data(b);
            } else {
                self.command(b);
            }
        }
    }

    fn command(&mut self, cmd: u8) {
        self.command = Some(cmd);
        self.params_len = 0;
        self.high = None;
        if cmd == RAMWR {
            self.x = self.cols.start;
            self.y = self.rows.start;
        }
    }

    fn data(&mut self, b: u8) {
        match self.command {
            Some(cmd @ (CASET | RASET)) => {
                if let Some(slot) = self.params.get_mut(self.params_len) {
                    *slot = b;
                    self.params_len += 1;
                    if self.params_len == self.params.len() {
                        self.set_window(cmd);
                    }
                }
            }
            Some(RAMWR) => match self.high.take() {
                None => self.high = Some(b),
                Some(hi) => self.put(u16::from_be_bytes([hi, b])),
            },
            _ => {}
        }
    }

    fn set_window(&mut self, cmd: u8) {
        let [a, b, c, d] = self.params;
        let span = Span {
            start: u16::from_be_bytes([a, b]),
            end: u16::from_be_bytes([c, d]),
        };
        if span.start > span.end {
            return;
        }
        if cmd == CASET {
            self.cols = span;
        } else {
            self.rows = span;
        }
    }

    /// Writes one pixel at the cursor and advances it inside the window.
    fn put(&mut self, colour: u16) {
        if let Some(i) = index(self.x, self.y)
            && let (Some(p), Some(w)) = (self.pixels.get_mut(i), self.written.get_mut(i))
        {
            *p = colour;
            *w = true;
        }
        self.pixel_writes = self.pixel_writes.saturating_add(1);
        if self.x < self.cols.end {
            self.x += 1;
        } else {
            self.x = self.cols.start;
            self.y = if self.y < self.rows.end {
                self.y + 1
            } else {
                self.rows.start
            };
        }
    }

    /// The RGB565 colour at `(x, y)`; `None` off the panel.
    #[cfg(test)]
    pub fn pixel(&self, x: u16, y: u16) -> Option<u16> {
        self.pixels.get(index(x, y)?).copied()
    }

    /// Whether `(x, y)` was ever written.
    #[cfg(test)]
    pub fn written(&self, x: u16, y: u16) -> bool {
        index(x, y)
            .and_then(|i| self.written.get(i).copied())
            .unwrap_or(false)
    }

    /// Pixels written since power-up.
    pub fn pixel_writes(&self) -> u64 {
        self.pixel_writes
    }

    /// The framebuffer as a binary PPM (P6, 8 bits per channel).
    pub fn to_ppm(&self) -> Vec<u8> {
        let mut out = format!("P6\n{WIDTH} {HEIGHT}\n255\n").into_bytes();
        out.reserve(PIXELS * 3);
        for &p in &self.pixels {
            out.extend_from_slice(&rgb888(p));
        }
        out
    }
}

/// Framebuffer index of `(x, y)`, or `None` off the panel.
fn index(x: u16, y: u16) -> Option<usize> {
    (x < WIDTH && y < HEIGHT).then(|| usize::from(y) * usize::from(WIDTH) + usize::from(x))
}

/// Expands RGB565 to 8-bit channels by bit replication (an injective map,
/// so two pixels are equal in the PPM exactly when they are in RGB565).
pub fn rgb888(p: u16) -> [u8; 3] {
    let r = ((p >> 11) & 0x1F) as u8;
    let g = ((p >> 5) & 0x3F) as u8;
    let b = (p & 0x1F) as u8;
    [
        (r << 3) | (r >> 2),
        (g << 2) | (g >> 4),
        (b << 3) | (b >> 2),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::wasm_abi::route::Preamble;

    fn send_preamble(panel: &mut Panel, p: &Preamble) {
        for s in p.segments() {
            panel.write(s.dc, s.bytes);
        }
    }

    #[test]
    fn window_and_memory_write_place_pixels() {
        let mut panel = Panel::new();
        send_preamble(&mut panel, &Preamble::mipi_window(10, 20, 11, 21).unwrap());
        // Five pixels into a 2×2 window: the fifth wraps to the corner.
        panel.write(true, &[0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0]);
        panel.write(true, &[0x11, 0x11]);
        assert_eq!(panel.pixel(10, 20), Some(0x1111));
        assert_eq!(panel.pixel(11, 20), Some(0x5678));
        assert_eq!(panel.pixel(10, 21), Some(0x9ABC));
        assert_eq!(panel.pixel(11, 21), Some(0xDEF0));
        assert!(!panel.written(12, 20) && !panel.written(10, 22));
        assert_eq!(panel.pixel_writes(), 5);
    }

    #[test]
    fn commands_reset_partial_pixels_and_bad_windows_are_ignored() {
        let mut panel = Panel::new();
        panel.write(false, &[RAMWR]);
        panel.write(true, &[0xAB]);
        panel.write(false, &[0x00]); // NOP drops the dangling byte
        panel.write(true, &[0xCD, 0xEF]);
        assert!(!panel.written(0, 0), "data after a NOP is ignored");
        panel.write(false, &[CASET]);
        panel.write(true, &[0, 9, 0, 3]); // start > end: ignored
        panel.write(false, &[RAMWR]);
        panel.write(true, &[0xFF, 0xFF]);
        assert_eq!(panel.pixel(0, 0), Some(0xFFFF));
        assert_eq!(panel.pixel(WIDTH, 0), None);
    }

    #[test]
    fn off_panel_pixels_are_discarded() {
        let mut panel = Panel::new();
        send_preamble(&mut panel, &Preamble::mipi_window(239, 0, 240, 0).unwrap());
        panel.write(true, &[0x00, 0x01, 0x00, 0x02]);
        assert_eq!(panel.pixel(239, 0), Some(1));
        assert_eq!(panel.pixel_writes(), 2);
    }

    #[test]
    fn ppm_header_and_colour_expansion() {
        let ppm = Panel::new().to_ppm();
        assert!(ppm.starts_with(b"P6\n240 240\n255\n"));
        assert_eq!(ppm.len(), 15 + 240 * 240 * 3);
        assert_eq!(rgb888(0xFFFF), [255, 255, 255]);
        assert_eq!(rgb888(0xF800), [255, 0, 0]);
        assert_eq!(rgb888(0x07E0), [0, 255, 0]);
        assert_eq!(rgb888(0x001F), [0, 0, 255]);
    }
}
