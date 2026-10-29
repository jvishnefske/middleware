//! Argument validation for the ABI 4 DMA route imports.
//!
//! A *route* moves a stream of frames between a host data source or sink
//! and a pub/sub key without copying the bytes through guest memory. The
//! payload only configures it; the host runs it. Frames travel as chunks in
//! the [`crate::stream`] wire format.
//!
//! * A **source route** (`route_source`) binds a host data source
//!   ([`SourceKind`]) to a publish key. Every frame period the host captures
//!   (or, for [`SourceKind::TestPattern`], generates or reads from flash) a
//!   frame and publishes it on the key as [`crate::stream::Chunker`] chunks.
//! * A **sink route** (`route_sink`) binds a key to an SPI bus opened with
//!   `spi_open`. Chunks received on the key go through a
//!   [`crate::stream::Sink`]; before a frame's first chunk the host sends
//!   the [`Preamble`] (each segment with the D/C pin at its level), then
//!   DMAs every accepted chunk's data with D/C high.
//!
//! # `route_source(source, key_ptr, key_len, cfg_ptr, cfg_len)`
//!
//! `source` is a [`SourceKind`] code; `cfg` is exactly [`SOURCE_CFG_LEN`]
//! bytes (little-endian):
//!
//! | bytes | field                                                         |
//! |-------|---------------------------------------------------------------|
//! | 2     | width in pixels, `1..=MAX_DIM`                                 |
//! | 2     | height in pixels, `1..=MAX_DIM`                                |
//! | 1     | [`PixelFormat`] (0 = RGB565 big-endian, 1 = baseline JPEG)     |
//! | 1     | flags, must be 0                                              |
//! | 2     | frame period in ms, `MIN_PERIOD_MS..`                          |
//! | 2     | chunk data bytes, `1..=MAX_CHUNK_DATA`, 0 = [`ESPNOW_CHUNK_DATA`] |
//!
//! For [`SourceKind::Camera`] the config is [`CAMERA_CFG_LEN`] bytes: the
//! same ten bytes (the format must be JPEG: width and height are the frame
//! size the payload configured on the sensor, the period is the minimum
//! interval between frames) followed by the camera's parallel-port pins
//! ([`CameraPins`], one byte each):
//!
//! | bytes | field                                                         |
//! |-------|---------------------------------------------------------------|
//! | 10    | the source config above, format 1 (JPEG)                       |
//! | 8     | data pins D0..D7                                               |
//! | 1     | VSYNC pin                                                      |
//! | 1     | HREF pin                                                       |
//! | 1     | PCLK pin                                                       |
//!
//! The eleven pins must be distinct; they are inputs, so a board may accept
//! input-only pins for them. The payload sets the sensor up itself (power,
//! clock, register writes over `i2c_transfer`) before it starts the route;
//! the host only receives the frames ([`parse_source`] checks the layout,
//! the board checks the pins). [`TestPattern`](SourceKind::TestPattern)
//! takes exactly [`SOURCE_CFG_LEN`] bytes ([`source_cfg_len`]).
//!
//! For RGB565 every frame is exactly `width * height * 2` bytes, at most
//! [`MAX_FRAME_BYTES`]. For JPEG, width and height describe the image and
//! the frame length varies from frame to frame: each frame's length is the
//! `total` of its chunk headers, `1..=MAX_FRAME_BYTES`
//! ([`SourceConfig::fixed_frame_len`] is `None`). A JPEG frame is one
//! complete baseline JPEG stream (`SOI` … `EOI`), e.g. YCbCr 4:2:0 as an
//! OV2640 emits it.
//!
//! # `route_sink(key_ptr, key_len, bus, cfg_ptr, cfg_len)`
//!
//! `bus` is an SPI bus already opened with `spi_open`. `cfg` is
//! `SINK_HEADER_LEN + segments` bytes, at most [`MAX_SINK_CFG_LEN`]:
//!
//! | bytes | field                                                         |
//! |-------|---------------------------------------------------------------|
//! | 1     | D/C pin (the host drives it as an output while the route runs) |
//! | 1     | flags, must be 0                                              |
//! | 4     | frame length in bytes, `u32` LE, `1..=MAX_FRAME_BYTES`; chunks of frames with another length are dropped |
//! | 1     | segment count, `0..=MAX_SEGMENTS`                              |
//! | …     | each segment: D/C level (0 = command, 1 = data), length `1..`, bytes |
//!
//! All segments together hold at most [`MAX_PREAMBLE`] bytes and the config
//! must end exactly after the last one. [`Preamble::mipi_window`] builds the
//! usual `CASET`/`RASET`/`RAMWR` window for MIPI-DCS panels (GC9A01,
//! ST7789, ILI9341).
//!
//! # Handles, events and stats
//!
//! Both return a route handle `0..MAX_ROUTES`. `route_stop(handle)` stops a
//! route, releases its key, bus and D/C pin, and purges its pending events;
//! a handle that is not running is `Busy`. `route_stat(handle, stat)`
//! returns one [`RouteStat`] counter (saturating). While a sink route runs,
//! the host owns its bus and D/C pin: `spi_transfer` on that bus is `Busy`,
//! and its chunks are consumed by the route, not delivered to
//! `on_message`. Routes report through `on_irq` with
//! [`EventKind::RouteFrames`](super::events::EventKind::RouteFrames),
//! [`EventKind::RouteDropped`](super::events::EventKind::RouteDropped) and
//! [`EventKind::RouteStopped`](super::events::EventKind::RouteStopped).

use super::AbiError;
use crate::stream::{ESPNOW_CHUNK_DATA, MAX_CHUNK_DATA, MAX_FRAME_BYTES};

/// Routes a payload may run at once (handles `0..MAX_ROUTES`).
pub const MAX_ROUTES: usize = 4;

/// Largest frame width or height, in pixels.
pub const MAX_DIM: u16 = 320;

/// Shortest frame period, in milliseconds.
pub const MIN_PERIOD_MS: u16 = 10;

/// Exact length of a `route_source` config.
pub const SOURCE_CFG_LEN: usize = 10;

/// Bytes of a [`CameraPins`] block.
pub const CAMERA_PINS_LEN: usize = 11;

/// Exact length of a `route_source` config for [`SourceKind::Camera`]: the
/// [`SourceConfig`] followed by the [`CameraPins`].
pub const CAMERA_CFG_LEN: usize = SOURCE_CFG_LEN + CAMERA_PINS_LEN;

/// Fixed part of a `route_sink` config, before the segments.
pub const SINK_HEADER_LEN: usize = 7;

/// Most preamble segments.
pub const MAX_SEGMENTS: usize = 8;

/// Most preamble bytes, all segments together.
pub const MAX_PREAMBLE: usize = 64;

/// Longest `route_sink` config: header, two bytes per segment, and the
/// segment bytes.
pub const MAX_SINK_CFG_LEN: usize = SINK_HEADER_LEN + 2 * MAX_SEGMENTS + MAX_PREAMBLE;

/// A host data source for `route_source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// The synthetic test pattern of [`crate::stream::pattern`] (the fake
    /// camera); hosts may offer it on any board.
    TestPattern = 0,
    /// The board's camera.
    Camera = 1,
}

impl SourceKind {
    /// Parses the `source` argument.
    pub const fn from_i32(v: i32) -> Result<Self, AbiError> {
        match v {
            0 => Ok(SourceKind::TestPattern),
            1 => Ok(SourceKind::Camera),
            _ => Err(AbiError::InvalidArgument),
        }
    }
}

/// The `cfg_len` `route_source` requires for `kind`: [`SOURCE_CFG_LEN`]
/// for the test pattern, [`CAMERA_CFG_LEN`] for the camera.
pub const fn source_cfg_len(kind: SourceKind) -> usize {
    match kind {
        SourceKind::TestPattern => SOURCE_CFG_LEN,
        SourceKind::Camera => CAMERA_CFG_LEN,
    }
}

/// The parallel-port pins of a camera source (8-bit DVP: data, frame and
/// line strobes, pixel clock), all inputs to the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CameraPins {
    /// D0..D7, least significant bit first.
    pub data: [u8; 8],
    /// Frame strobe (VSYNC).
    pub vsync: u8,
    /// Line strobe: data is valid while it is high (HREF).
    pub href: u8,
    /// Pixel clock: data is sampled on its rising edge (PCLK).
    pub pclk: u8,
}

impl CameraPins {
    /// The AI-Thinker ESP32-CAM's OV2640 port: D0..D7 = 5, 18, 19, 21, 36,
    /// 39, 34, 35; VSYNC 25, HREF 23, PCLK 22.
    pub const AI_THINKER: CameraPins = CameraPins {
        data: [5, 18, 19, 21, 36, 39, 34, 35],
        vsync: 25,
        href: 23,
        pclk: 22,
    };

    /// Every pin, in wire order (D0..D7, VSYNC, HREF, PCLK).
    pub const fn all(&self) -> [u8; CAMERA_PINS_LEN] {
        let d = self.data;
        [d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7], self.vsync, self.href, self.pclk]
    }

    /// The pin block bytes ([`CameraPins::all`]).
    pub const fn encode(&self) -> [u8; CAMERA_PINS_LEN] {
        self.all()
    }

    /// The pins, if they are distinct; `InvalidArgument` otherwise.
    pub const fn validated(self) -> Result<Self, AbiError> {
        let all = self.all();
        let mut i = 0;
        while i < CAMERA_PINS_LEN {
            let mut j = i + 1;
            while j < CAMERA_PINS_LEN {
                if all[i] == all[j] {
                    return Err(AbiError::InvalidArgument);
                }
                j += 1;
            }
            i += 1;
        }
        Ok(self)
    }

    /// Parses a pin block of exactly [`CAMERA_PINS_LEN`] distinct pins.
    pub fn parse(bytes: &[u8]) -> Result<Self, AbiError> {
        let b: &[u8; CAMERA_PINS_LEN] = bytes.try_into().map_err(|_| AbiError::InvalidArgument)?;
        let [d0, d1, d2, d3, d4, d5, d6, d7, vsync, href, pclk] = *b;
        CameraPins {
            data: [d0, d1, d2, d3, d4, d5, d6, d7],
            vsync,
            href,
            pclk,
        }
        .validated()
    }
}

/// A data source with what it needs to run: the parsed `source` argument of
/// `route_source` together with its source-specific config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// [`SourceKind::TestPattern`].
    TestPattern,
    /// [`SourceKind::Camera`] on these pins.
    Camera(CameraPins),
}

impl Source {
    /// The source's kind.
    pub const fn kind(&self) -> SourceKind {
        match self {
            Source::TestPattern => SourceKind::TestPattern,
            Source::Camera(_) => SourceKind::Camera,
        }
    }

    /// The `cfg` bytes of `route_source` for this source and `cfg`; returns
    /// the buffer and the length to pass ([`source_cfg_len`]).
    pub const fn encode(&self, cfg: &SourceConfig) -> ([u8; CAMERA_CFG_LEN], usize) {
        let head = cfg.encode();
        let mut out = [0u8; CAMERA_CFG_LEN];
        let mut i = 0;
        while i < SOURCE_CFG_LEN {
            out[i] = head[i];
            i += 1;
        }
        match self {
            Source::TestPattern => (out, SOURCE_CFG_LEN),
            Source::Camera(pins) => {
                let p = pins.encode();
                let mut j = 0;
                while j < CAMERA_PINS_LEN {
                    out[SOURCE_CFG_LEN + j] = p[j];
                    j += 1;
                }
                (out, CAMERA_CFG_LEN)
            }
        }
    }
}

/// Parses the `cfg` bytes of `route_source` for a source of `kind`: the
/// [`SourceConfig`], plus for the camera its [`CameraPins`]. A camera config
/// must be JPEG (the sensor's compressed output); anything else, or a
/// length other than [`source_cfg_len`], is `InvalidArgument`.
pub fn parse_source(kind: SourceKind, cfg: &[u8]) -> Result<(Source, SourceConfig), AbiError> {
    let bad = AbiError::InvalidArgument;
    if cfg.len() != source_cfg_len(kind) {
        return Err(bad);
    }
    let (head, pins) = cfg.split_at_checked(SOURCE_CFG_LEN).ok_or(bad)?;
    let config = SourceConfig::parse(head)?;
    match kind {
        SourceKind::TestPattern => Ok((Source::TestPattern, config)),
        SourceKind::Camera if config.format == PixelFormat::Jpeg => {
            Ok((Source::Camera(CameraPins::parse(pins)?), config))
        }
        SourceKind::Camera => Err(bad),
    }
}

/// Pixel encoding of a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 16-bit RGB565, high byte first (what SPI panels take); every frame
    /// is `width * height * 2` bytes.
    Rgb565Be = 0,
    /// Baseline JPEG, one complete image per frame; the length varies per
    /// frame (each chunk header's `total`).
    Jpeg = 1,
}

impl PixelFormat {
    /// Parses a format code.
    pub const fn from_u8(v: u8) -> Result<Self, AbiError> {
        match v {
            0 => Ok(PixelFormat::Rgb565Be),
            1 => Ok(PixelFormat::Jpeg),
            _ => Err(AbiError::InvalidArgument),
        }
    }

    /// Bytes per pixel of a raw format; `None` for a compressed one (JPEG).
    pub const fn bytes_per_pixel(self) -> Option<u32> {
        match self {
            PixelFormat::Rgb565Be => Some(2),
            PixelFormat::Jpeg => None,
        }
    }
}

/// A validated `route_source` config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceConfig {
    /// Width in pixels.
    pub width: u16,
    /// Height in pixels.
    pub height: u16,
    /// Pixel encoding.
    pub format: PixelFormat,
    /// Frame period in milliseconds.
    pub period_ms: u16,
    /// Data bytes per chunk (0 already resolved to [`ESPNOW_CHUNK_DATA`]).
    pub chunk_data: u16,
}

impl SourceConfig {
    /// An RGB565 config with the default chunk size, validated.
    pub const fn new(width: u16, height: u16, period_ms: u16) -> Result<Self, AbiError> {
        SourceConfig {
            width,
            height,
            format: PixelFormat::Rgb565Be,
            period_ms,
            chunk_data: ESPNOW_CHUNK_DATA as u16,
        }
        .validated()
    }

    /// A baseline-JPEG config of a `width`×`height` image with the default
    /// chunk size, validated.
    pub const fn jpeg(width: u16, height: u16, period_ms: u16) -> Result<Self, AbiError> {
        SourceConfig {
            width,
            height,
            format: PixelFormat::Jpeg,
            period_ms,
            chunk_data: ESPNOW_CHUNK_DATA as u16,
        }
        .validated()
    }

    /// The same config with `bytes` of data per chunk (0 = default),
    /// validated.
    pub const fn with_chunk_data(self, bytes: u16) -> Result<Self, AbiError> {
        let chunk_data = if bytes == 0 {
            ESPNOW_CHUNK_DATA as u16
        } else {
            bytes
        };
        SourceConfig { chunk_data, ..self }.validated()
    }

    /// Checks every rule of the table in the module docs.
    pub const fn validated(self) -> Result<Self, AbiError> {
        let bad = Err(AbiError::InvalidArgument);
        if self.width == 0 || self.width > MAX_DIM || self.height == 0 || self.height > MAX_DIM {
            return bad;
        }
        if self.frame_len() > MAX_FRAME_BYTES || self.period_ms < MIN_PERIOD_MS {
            return bad;
        }
        if self.chunk_data == 0 || self.chunk_data as usize > MAX_CHUNK_DATA {
            return bad;
        }
        Ok(self)
    }

    /// Length of every frame in bytes for a raw format (`width * height *
    /// bytes_per_pixel`); `None` for JPEG, whose frames vary in length.
    pub const fn fixed_frame_len(&self) -> Option<u32> {
        match self.format.bytes_per_pixel() {
            Some(bpp) => Some(self.width as u32 * self.height as u32 * bpp),
            None => None,
        }
    }

    /// Largest frame length in bytes: the exact length for a raw format,
    /// [`MAX_FRAME_BYTES`] for JPEG.
    pub const fn frame_len(&self) -> u32 {
        match self.fixed_frame_len() {
            Some(len) => len,
            None => MAX_FRAME_BYTES,
        }
    }

    /// Parses the `cfg` bytes of `route_source`.
    pub fn parse(cfg: &[u8]) -> Result<Self, AbiError> {
        let bad = AbiError::InvalidArgument;
        let b: &[u8; SOURCE_CFG_LEN] = cfg.try_into().map_err(|_| bad)?;
        let [w0, w1, h0, h1, format, flags, p0, p1, c0, c1] = *b;
        if flags != 0 {
            return Err(bad);
        }
        let chunk = u16::from_le_bytes([c0, c1]);
        SourceConfig {
            width: u16::from_le_bytes([w0, w1]),
            height: u16::from_le_bytes([h0, h1]),
            format: PixelFormat::from_u8(format)?,
            period_ms: u16::from_le_bytes([p0, p1]),
            chunk_data: chunk,
        }
        .with_chunk_data(chunk)
    }

    /// The `cfg` bytes for `route_source`.
    pub const fn encode(&self) -> [u8; SOURCE_CFG_LEN] {
        let [w0, w1] = self.width.to_le_bytes();
        let [h0, h1] = self.height.to_le_bytes();
        let [p0, p1] = self.period_ms.to_le_bytes();
        let [c0, c1] = self.chunk_data.to_le_bytes();
        [w0, w1, h0, h1, self.format as u8, 0, p0, p1, c0, c1]
    }
}

/// One preamble segment: bytes sent with the D/C pin at one level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment<'a> {
    /// D/C level: `false` = command (low), `true` = data (high).
    pub dc: bool,
    /// The bytes, at least one.
    pub bytes: &'a [u8],
}

/// Where one segment's bytes sit in [`Preamble`]'s buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Span {
    dc: bool,
    start: u8,
    len: u8,
}

/// The bytes a sink route sends before each frame, as segments; at most
/// [`MAX_SEGMENTS`] segments and [`MAX_PREAMBLE`] bytes. Build with
/// [`Preamble::push`] (or [`Preamble::command`] / [`Preamble::data`]); each
/// returns a new value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preamble {
    bytes: [u8; MAX_PREAMBLE],
    used: u8,
    spans: [Span; MAX_SEGMENTS],
    count: u8,
}

impl Default for Preamble {
    fn default() -> Self {
        Self::new()
    }
}

impl Preamble {
    /// No segments.
    pub const fn new() -> Self {
        Preamble {
            bytes: [0; MAX_PREAMBLE],
            used: 0,
            spans: [Span {
                dc: false,
                start: 0,
                len: 0,
            }; MAX_SEGMENTS],
            count: 0,
        }
    }

    /// The preamble with one more segment; `InvalidArgument` if `bytes` is
    /// empty or a limit would be exceeded.
    pub fn push(self, dc: bool, bytes: &[u8]) -> Result<Self, AbiError> {
        let bad = AbiError::InvalidArgument;
        let start = usize::from(self.used);
        let end = start.checked_add(bytes.len()).ok_or(bad)?;
        if bytes.is_empty() || end > MAX_PREAMBLE {
            return Err(bad);
        }
        let mut next = self;
        next.bytes
            .get_mut(start..end)
            .ok_or(bad)?
            .copy_from_slice(bytes);
        *next.spans.get_mut(usize::from(self.count)).ok_or(bad)? = Span {
            dc,
            start: self.used,
            len: u8::try_from(bytes.len()).map_err(|_| bad)?,
        };
        next.used = u8::try_from(end).map_err(|_| bad)?;
        next.count += 1;
        Ok(next)
    }

    /// [`Preamble::push`] of one command byte (D/C low).
    pub fn command(self, cmd: u8) -> Result<Self, AbiError> {
        self.push(false, &[cmd])
    }

    /// [`Preamble::push`] of parameter bytes (D/C high).
    pub fn data(self, bytes: &[u8]) -> Result<Self, AbiError> {
        self.push(true, bytes)
    }

    /// The MIPI-DCS window `x0..=x1` × `y0..=y1` followed by memory write:
    /// `CASET` (0x2A) `x0 x1`, `RASET` (0x2B) `y0 y1` (16-bit big-endian
    /// each) and `RAMWR` (0x2C) — five segments, 11 bytes.
    pub fn mipi_window(x0: u16, y0: u16, x1: u16, y1: u16) -> Result<Self, AbiError> {
        let [a, b] = x0.to_be_bytes();
        let [c, d] = x1.to_be_bytes();
        let [e, f] = y0.to_be_bytes();
        let [g, h] = y1.to_be_bytes();
        Preamble::new()
            .command(0x2A)?
            .data(&[a, b, c, d])?
            .command(0x2B)?
            .data(&[e, f, g, h])?
            .command(0x2C)
    }

    /// Number of segments.
    pub const fn len(&self) -> usize {
        self.count as usize
    }

    /// Whether there are no segments.
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Total preamble bytes.
    pub const fn byte_len(&self) -> usize {
        self.used as usize
    }

    /// The segments in order.
    pub fn segments(&self) -> impl Iterator<Item = Segment<'_>> {
        self.spans.iter().take(self.len()).filter_map(move |s| {
            let start = usize::from(s.start);
            let bytes = self.bytes.get(start..start + usize::from(s.len))?;
            Some(Segment { dc: s.dc, bytes })
        })
    }
}

/// A validated `route_sink` config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SinkConfig {
    /// The D/C pin.
    pub dc_pin: u8,
    /// Expected frame length in bytes.
    pub frame_len: u32,
    /// Sent before each frame's first chunk.
    pub preamble: Preamble,
}

impl SinkConfig {
    /// A config with no preamble, validated.
    pub const fn new(dc_pin: u8, frame_len: u32) -> Result<Self, AbiError> {
        if frame_len == 0 || frame_len > MAX_FRAME_BYTES {
            return Err(AbiError::InvalidArgument);
        }
        Ok(SinkConfig {
            dc_pin,
            frame_len,
            preamble: Preamble::new(),
        })
    }

    /// The same config with `preamble`.
    pub const fn with_preamble(self, preamble: Preamble) -> Self {
        SinkConfig { preamble, ..self }
    }

    /// Parses the `cfg` bytes of `route_sink`.
    pub fn parse(cfg: &[u8]) -> Result<Self, AbiError> {
        let bad = AbiError::InvalidArgument;
        if cfg.len() > MAX_SINK_CFG_LEN {
            return Err(bad);
        }
        let (head, mut rest) = cfg.split_first_chunk::<SINK_HEADER_LEN>().ok_or(bad)?;
        let [dc_pin, flags, f0, f1, f2, f3, count] = *head;
        if flags != 0 || usize::from(count) > MAX_SEGMENTS {
            return Err(bad);
        }
        let mut sink = SinkConfig::new(dc_pin, u32::from_le_bytes([f0, f1, f2, f3]))?;
        for _ in 0..count {
            let (&[level, len], tail) = rest.split_first_chunk::<2>().ok_or(bad)?;
            let dc = match level {
                0 => false,
                1 => true,
                _ => return Err(bad),
            };
            let (bytes, tail) = tail.split_at_checked(usize::from(len)).ok_or(bad)?;
            sink.preamble = sink.preamble.push(dc, bytes)?;
            rest = tail;
        }
        if rest.is_empty() { Ok(sink) } else { Err(bad) }
    }

    /// Writes the `cfg` bytes for `route_sink` into `out`; returns the
    /// length.
    pub fn encode(&self, out: &mut [u8; MAX_SINK_CFG_LEN]) -> usize {
        let [f0, f1, f2, f3] = self.frame_len.to_le_bytes();
        let head = [self.dc_pin, 0, f0, f1, f2, f3, self.preamble.count];
        let mut at = 0;
        let mut put = |bytes: &[u8]| {
            if let Some(dst) = out.get_mut(at..at + bytes.len()) {
                dst.copy_from_slice(bytes);
                at += bytes.len();
            }
        };
        put(&head);
        for s in self.preamble.segments() {
            // A segment holds at most MAX_PREAMBLE bytes, so it fits u8.
            put(&[u8::from(s.dc), s.bytes.len() as u8]);
            put(s.bytes);
        }
        at
    }
}

/// Counters readable with `route_stat(handle, stat)`; see
/// [`crate::stream::RouteStats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteStat {
    /// Frames completed.
    Frames = 0,
    /// Frames dropped or abandoned.
    FramesDropped = 1,
    /// Chunks forwarded or published.
    Chunks = 2,
    /// Chunks dropped.
    ChunksDropped = 3,
}

impl RouteStat {
    /// Parses the `stat` argument.
    pub const fn from_i32(v: i32) -> Result<Self, AbiError> {
        match v {
            0 => Ok(RouteStat::Frames),
            1 => Ok(RouteStat::FramesDropped),
            2 => Ok(RouteStat::Chunks),
            3 => Ok(RouteStat::ChunksDropped),
            _ => Err(AbiError::InvalidArgument),
        }
    }

    /// Selects this counter from `stats`.
    pub const fn of(self, stats: &crate::stream::RouteStats) -> u32 {
        match self {
            RouteStat::Frames => stats.frames,
            RouteStat::FramesDropped => stats.frames_dropped,
            RouteStat::Chunks => stats.chunks,
            RouteStat::ChunksDropped => stats.chunks_dropped,
        }
    }
}

/// Parses a route handle argument (`0..MAX_ROUTES`).
pub fn route_handle(v: i32) -> Result<u8, AbiError> {
    u8::try_from(v)
        .ok()
        .filter(|&h| usize::from(h) < MAX_ROUTES)
        .ok_or(AbiError::InvalidArgument)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::RouteStats;

    #[test]
    fn source_kinds_and_formats() {
        assert_eq!(SourceKind::from_i32(0), Ok(SourceKind::TestPattern));
        assert_eq!(SourceKind::from_i32(1), Ok(SourceKind::Camera));
        assert_eq!(SourceKind::from_i32(2), Err(AbiError::InvalidArgument));
        assert_eq!(SourceKind::from_i32(-1), Err(AbiError::InvalidArgument));
        assert_eq!(PixelFormat::from_u8(0), Ok(PixelFormat::Rgb565Be));
        assert_eq!(PixelFormat::from_u8(1), Ok(PixelFormat::Jpeg));
        assert_eq!(PixelFormat::from_u8(2), Err(AbiError::InvalidArgument));
        assert_eq!(PixelFormat::Rgb565Be.bytes_per_pixel(), Some(2));
        assert_eq!(PixelFormat::Jpeg.bytes_per_pixel(), None);
    }

    #[test]
    fn source_config_layout_and_round_trip() {
        let c = SourceConfig::new(120, 120, 100).expect("valid");
        assert_eq!(c.frame_len(), 28_800);
        assert_eq!(usize::from(c.chunk_data), ESPNOW_CHUNK_DATA);
        let bytes = c.encode();
        assert_eq!(bytes, [120, 0, 120, 0, 0, 0, 100, 0, 166, 0]);
        assert_eq!(SourceConfig::parse(&bytes), Ok(c));
        // Chunk size 0 means the default.
        let zero = [120, 0, 120, 0, 0, 0, 100, 0, 0, 0];
        assert_eq!(SourceConfig::parse(&zero), Ok(c));
        let big = c.with_chunk_data(500).expect("valid");
        assert_eq!(SourceConfig::parse(&big.encode()), Ok(big));
    }

    #[test]
    fn source_config_rules() {
        let ok = [120, 0, 120, 0, 0, 0, 100, 0, 0, 0];
        let with = |i: usize, v: u8| {
            let mut b = ok;
            b[i] = v;
            SourceConfig::parse(&b)
        };
        let bad = Err(AbiError::InvalidArgument);
        assert_eq!(SourceConfig::parse(&ok[..9]), bad, "short");
        assert_eq!(SourceConfig::parse(&[0; 11]), bad, "long");
        assert_eq!(with(0, 0), bad, "width 0");
        assert_eq!(with(2, 0), bad, "height 0");
        assert!(with(4, 1).is_ok(), "JPEG");
        assert_eq!(with(4, 2), bad, "format");
        assert_eq!(with(5, 1), bad, "flags");
        assert_eq!(with(6, 9), bad, "period below 10 ms");
        assert!(with(6, 10).is_ok());
        assert_eq!(SourceConfig::new(321, 1, 100), bad, "width");
        assert_eq!(SourceConfig::new(1, 321, 100), bad, "height");
        assert!(SourceConfig::new(320, 240, 100).is_ok(), "QVGA fits");
        assert_eq!(
            SourceConfig::new(320, 241, 100),
            bad,
            "over MAX_FRAME_BYTES"
        );
        let c = SourceConfig::new(1, 1, 10).expect("valid");
        assert_eq!(c.with_chunk_data(501), bad, "chunk over MAX_CHUNK_DATA");
        assert!(c.with_chunk_data(1).is_ok());
    }

    #[test]
    fn jpeg_source_config() {
        let c = SourceConfig::jpeg(240, 240, 200).expect("valid");
        let bytes = c.encode();
        assert_eq!(bytes, [240, 0, 240, 0, 1, 0, 200, 0, 166, 0]);
        assert_eq!(SourceConfig::parse(&bytes), Ok(c));
        assert_eq!(c.format, PixelFormat::Jpeg);
        // Frame length varies per frame; only the ceiling is known.
        assert_eq!(c.fixed_frame_len(), None);
        assert_eq!(c.frame_len(), MAX_FRAME_BYTES);
        assert_eq!(
            SourceConfig::new(120, 120, 100).map(|c| c.fixed_frame_len()),
            Ok(Some(28_800))
        );
        // Geometry, period and chunk rules apply as for RGB565; a 320×320
        // JPEG is allowed (no raw frame to fit), 321 is not.
        let bad = Err(AbiError::InvalidArgument);
        assert!(SourceConfig::jpeg(320, 320, 10).is_ok());
        assert_eq!(SourceConfig::jpeg(321, 240, 100), bad);
        assert_eq!(SourceConfig::jpeg(0, 240, 100), bad);
        assert_eq!(SourceConfig::jpeg(240, 240, 9), bad);
        assert_eq!(c.with_chunk_data(501), bad);
        assert_eq!(
            SourceConfig::parse(&[240, 0, 240, 0, 1, 1, 200, 0, 166, 0]),
            bad,
            "flags"
        );
    }

    #[test]
    fn camera_config_layout_and_rules() {
        let cfg = SourceConfig::jpeg(240, 240, 500).expect("valid");
        let cam = Source::Camera(CameraPins::AI_THINKER);
        let (bytes, len) = cam.encode(&cfg);
        assert_eq!(len, CAMERA_CFG_LEN);
        assert_eq!(CAMERA_CFG_LEN, 21);
        assert_eq!(
            bytes,
            [
                240, 0, 240, 0, 1, 0, 0xF4, 1, 166, 0, // 240x240 JPEG, 500 ms, 166
                5, 18, 19, 21, 36, 39, 34, 35, // D0..D7
                25, 23, 22, // VSYNC, HREF, PCLK
            ]
        );
        assert_eq!(parse_source(SourceKind::Camera, &bytes), Ok((cam, cfg)));
        assert_eq!(cam.kind(), SourceKind::Camera);
        assert_eq!(source_cfg_len(SourceKind::Camera), CAMERA_CFG_LEN);
        assert_eq!(source_cfg_len(SourceKind::TestPattern), SOURCE_CFG_LEN);
        // The test pattern takes the ten bytes alone.
        let (tp, n) = Source::TestPattern.encode(&cfg);
        assert_eq!(n, SOURCE_CFG_LEN);
        assert_eq!(
            parse_source(SourceKind::TestPattern, &tp[..n]),
            Ok((Source::TestPattern, cfg))
        );
        let bad = Err(AbiError::InvalidArgument);
        assert_eq!(parse_source(SourceKind::TestPattern, &bytes), bad, "pins on a pattern");
        assert_eq!(parse_source(SourceKind::Camera, &bytes[..20]), bad, "short");
        assert_eq!(parse_source(SourceKind::Camera, &tp[..n]), bad, "no pins");
        let mut rgb = bytes;
        rgb[4] = 0;
        rgb[0] = 120;
        rgb[2] = 120;
        assert_eq!(parse_source(SourceKind::Camera, &rgb), bad, "camera must be JPEG");
        let mut dup = bytes;
        dup[20] = 5; // PCLK on D0's pin
        assert_eq!(parse_source(SourceKind::Camera, &dup), bad, "pins must differ");
        let mut flags = bytes;
        flags[5] = 1;
        assert_eq!(parse_source(SourceKind::Camera, &flags), bad, "head rules apply");
        assert_eq!(CameraPins::parse(&[0; 10]), Err(AbiError::InvalidArgument));
        assert_eq!(
            CameraPins::parse(&CameraPins::AI_THINKER.encode()),
            Ok(CameraPins::AI_THINKER)
        );
    }

    #[test]
    fn preamble_limits() {
        let bad = Err(AbiError::InvalidArgument);
        assert_eq!(Preamble::new().push(true, &[]), bad, "empty segment");
        let full = Preamble::new().data(&[0; MAX_PREAMBLE]).expect("fits");
        assert_eq!(full.byte_len(), MAX_PREAMBLE);
        assert_eq!(full.command(0), bad, "byte budget");
        let mut p = Preamble::new();
        for i in 0..MAX_SEGMENTS {
            p = p.command(i as u8).expect("fits");
        }
        assert_eq!(p.len(), MAX_SEGMENTS);
        assert_eq!(p.command(9), bad, "segment budget");
        assert!(Preamble::new().is_empty());
    }

    #[test]
    fn mipi_window_for_a_centred_120_square() {
        let p = Preamble::mipi_window(60, 60, 179, 179).expect("fits");
        let segs: [(bool, &[u8]); 5] = [
            (false, &[0x2A]),
            (true, &[0, 60, 0, 179]),
            (false, &[0x2B]),
            (true, &[0, 60, 0, 179]),
            (false, &[0x2C]),
        ];
        assert_eq!(p.len(), 5);
        assert_eq!(p.byte_len(), 11);
        assert!(p.segments().map(|s| (s.dc, s.bytes)).eq(segs));
    }

    #[test]
    fn sink_config_layout_and_round_trip() {
        let p = Preamble::mipi_window(60, 60, 179, 179).expect("fits");
        let c = SinkConfig::new(13, 28_800).expect("valid").with_preamble(p);
        let mut out = [0u8; MAX_SINK_CFG_LEN];
        let n = c.encode(&mut out);
        assert_eq!(n, SINK_HEADER_LEN + 2 * 5 + 11);
        assert_eq!(
            &out[..n],
            &[
                13, 0, 0x80, 0x70, 0, 0, 5, // dc 13, 28800 LE, 5 segments
                0, 1, 0x2A, //
                1, 4, 0, 60, 0, 179, //
                0, 1, 0x2B, //
                1, 4, 0, 60, 0, 179, //
                0, 1, 0x2C,
            ]
        );
        assert_eq!(SinkConfig::parse(&out[..n]), Ok(c));
        let bare = SinkConfig::new(2, 8).expect("valid");
        let n = bare.encode(&mut out);
        assert_eq!(SinkConfig::parse(&out[..n]), Ok(bare));
        let full = SinkConfig::new(2, 8).expect("valid").with_preamble(
            (0..MAX_SEGMENTS).fold(Preamble::new(), |p, _| p.data(&[7; 8]).expect("fits")),
        );
        let n = full.encode(&mut out);
        assert_eq!(n, MAX_SINK_CFG_LEN);
        assert_eq!(SinkConfig::parse(&out[..n]), Ok(full));
    }

    #[test]
    fn sink_config_rules() {
        let bad = Err(AbiError::InvalidArgument);
        let ok = [13, 0, 8, 0, 0, 0, 1, 1, 2, 0xAA, 0xBB];
        assert!(SinkConfig::parse(&ok).is_ok());
        let with = |i: usize, v: u8| {
            let mut b = ok;
            b[i] = v;
            SinkConfig::parse(&b)
        };
        assert_eq!(SinkConfig::parse(&ok[..6]), bad, "short header");
        assert_eq!(with(1, 1), bad, "flags");
        assert_eq!(SinkConfig::parse(&[13, 0, 0, 0, 0, 0, 0]), bad, "frame 0");
        let over = (MAX_FRAME_BYTES + 1).to_le_bytes();
        assert_eq!(
            SinkConfig::parse(&[13, 0, over[0], over[1], over[2], over[3], 0]),
            bad,
            "frame too large"
        );
        assert_eq!(with(6, 9), bad, "too many segments");
        assert_eq!(with(6, 2), bad, "missing segment");
        assert_eq!(with(6, 0), bad, "trailing bytes");
        assert_eq!(with(7, 2), bad, "dc level");
        assert_eq!(with(8, 0), bad, "empty segment");
        assert_eq!(with(8, 3), bad, "segment past the end");
        assert_eq!(SinkConfig::parse(&ok[..10]), bad, "truncated segment");
        assert_eq!(
            SinkConfig::parse(&[0; MAX_SINK_CFG_LEN + 1]),
            bad,
            "too long"
        );
        let mut many = [0u8; 7 + 2 * 3 + 66];
        many[..7].copy_from_slice(&[13, 0, 8, 0, 0, 0, 3]);
        many[7..9].copy_from_slice(&[1, 64]);
        many[73..75].copy_from_slice(&[1, 1]);
        many[76..78].copy_from_slice(&[0, 1]);
        assert_eq!(SinkConfig::parse(&many[..79]), bad, "over MAX_PREAMBLE");
    }

    #[test]
    fn stats_and_handles() {
        let s = RouteStats {
            frames: 1,
            frames_dropped: 2,
            chunks: 3,
            chunks_dropped: 4,
        };
        for (code, v) in [(0, 1), (1, 2), (2, 3), (3, 4)] {
            assert_eq!(RouteStat::from_i32(code).map(|st| st.of(&s)), Ok(v));
        }
        assert_eq!(RouteStat::from_i32(4), Err(AbiError::InvalidArgument));
        assert_eq!(route_handle(0), Ok(0));
        assert_eq!(route_handle(MAX_ROUTES as i32 - 1), Ok(3));
        assert_eq!(
            route_handle(MAX_ROUTES as i32),
            Err(AbiError::InvalidArgument)
        );
        assert_eq!(route_handle(-1), Err(AbiError::InvalidArgument));
    }
}
