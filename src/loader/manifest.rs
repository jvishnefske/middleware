//! `sys/manifest`: what a node offers payloads, in one self-describing
//! record (requirements MF1–MF5).
//!
//! `sys/info` says which ABI a firmware links and a 32-bit [`Caps`] set;
//! `sys/board` describes an ESP32's pins. Neither says *which* sample
//! rates a microphone takes, how many I2C buses there are or which links
//! the node has, so a browser that binds a payload's channels
//! ([`crate::wasm_abi::channels`]) cannot check a binding against the node
//! before installing it. The manifest does.
//!
//! # Layout
//!
//! `[version u8 = 1][abi u16 LE]`, then records `[tag u8][len u8][body]`
//! until the end. Integers are little-endian.
//!
//! | tag | record            | body                                             |
//! |-----|-------------------|--------------------------------------------------|
//! | 1   | [`tag::NODE`]     | node id (8)                                      |
//! | 2   | [`tag::IMPORT_GROUPS`] | [`Groups`] (u16)                            |
//! | 3   | [`tag::I2C`]      | buses u8, min Hz u32, max Hz u32                 |
//! | 4   | [`tag::SPI`]      | buses u8                                         |
//! | 5   | [`tag::SOURCE_IMU`] | rates mask u8 over [`IMU_RATES_HZ`] (accelerometer only), ranges mask u8 over [`IMU_RANGES_G`], rates mask u8 with the magnetometer (0: none) |
//! | 6   | [`tag::SOURCE_MIC`] | rates mask u8 over [`MIC_RATES_HZ`], modes mask u8 over [`AudioMode`] codes, max gain dB u8 |
//! | 7   | [`tag::SOURCE_FRAMES`] | kinds mask u8 over [`SourceKind`] codes, formats mask u8 over [`PixelFormat`](crate::wasm_abi::route::PixelFormat) codes, max width u16, max height u16 |
//! | 8   | [`tag::SINK_SPI`] | largest frame in bytes, u32                      |
//! | 9   | [`tag::LINK`]     | link id u8, [`LinkKind`] u8 (one record per link) |
//! | 10  | [`tag::LIMITS`]   | routes u8, subscriptions u8, channels u8, max message u16 |
//! | 11  | [`tag::BOARD`]    | the board's model, UTF-8, 1 to [`BOARD_NAME_MAX`] bytes ([`BoardName`]) |
//!
//! Decoding is forward compatible, unlike `sys/board`'s exact length:
//! an unknown tag is skipped, and a known record longer than its body
//! above keeps its known prefix, so a later firmware may append fields
//! and records. A known record shorter than its body, a record running
//! past the end, or another version is refused ([`ManifestError`]). At
//! most [`MAX_LINKS`] links are kept; further link records are skipped.
//!
//! [`Manifest`] is plain data, built with `const fn`s next to a board's
//! `wasm_host::Device` implementation; [`Manifest::caps`] derives
//! the `sys/info` capability bits from it, so the two never disagree.
//! An encoded manifest is at most [`MANIFEST_MAX`] bytes.

use super::Caps;
use crate::transport::SenderId;
use crate::wasm_abi::route::{
    AudioMode, IMU_RANGES_G, IMU_RATES_HZ, ImuConfig, MIC_RATES_HZ, MicConfig, SampleSource,
    SourceConfig, SourceKind,
};

/// The manifest format version this module writes and reads.
pub const VERSION: u8 = 1;

/// Largest encoded manifest; fits one `sys/*` reply (512 bytes).
pub const MANIFEST_MAX: usize = 384;

/// Most links a manifest carries.
pub const MAX_LINKS: usize = 4;

/// Longest board name a manifest carries, in bytes.
pub const BOARD_NAME_MAX: usize = 32;

/// Record tags.
pub mod tag {
    /// The node id.
    pub const NODE: u8 = 1;
    /// The import groups the runtime links and the shell serves.
    pub const IMPORT_GROUPS: u8 = 2;
    /// I2C controllers.
    pub const I2C: u8 = 3;
    /// SPI controllers.
    pub const SPI: u8 = 4;
    /// The IMU sample source.
    pub const SOURCE_IMU: u8 = 5;
    /// The microphone sample source.
    pub const SOURCE_MIC: u8 = 6;
    /// Frame sources (test pattern, camera).
    pub const SOURCE_FRAMES: u8 = 7;
    /// SPI sink routes.
    pub const SINK_SPI: u8 = 8;
    /// One link.
    pub const LINK: u8 = 9;
    /// Runtime limits.
    pub const LIMITS: u8 = 10;
    /// The board's model.
    pub const BOARD: u8 = 11;
}

/// Which board a node runs on, as its own description names it: a Linux
/// node's device-tree model (`/proc/device-tree/model`, such as
/// `ZynqMP KD240 revA/B/1` or `Raspberry Pi 4 Model B Rev 1.4`). `sys/info`'s
/// chip says the architecture (`linux-aarch64`); this says the board, so a
/// tool can tell a Kria from a Pi. UTF-8, cut to [`BOARD_NAME_MAX`] bytes at
/// a character boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoardName {
    len: u8,
    bytes: [u8; BOARD_NAME_MAX],
}

impl BoardName {
    /// `name`, cut to [`BOARD_NAME_MAX`] bytes at a character boundary;
    /// `None` when empty.
    pub const fn new(name: &str) -> Option<Self> {
        let b = name.as_bytes();
        let mut len = if b.len() < BOARD_NAME_MAX {
            b.len()
        } else {
            BOARD_NAME_MAX
        };
        // Back off from inside a multi-byte character.
        while len > 0 && len < b.len() && (b[len] & 0xc0) == 0x80 {
            len -= 1;
        }
        if len == 0 {
            return None;
        }
        let mut bytes = [0u8; BOARD_NAME_MAX];
        let mut i = 0;
        while i < len {
            bytes[i] = b[i];
            i += 1;
        }
        Some(BoardName {
            len: len as u8,
            bytes,
        })
    }

    /// The name's bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    /// The name, if it is UTF-8 (one a peer sent may not be).
    pub fn as_str(&self) -> Option<&str> {
        core::str::from_utf8(self.as_bytes()).ok()
    }
}

/// Why a manifest did not decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestError {
    /// Shorter than the header, or a record runs past the end.
    Truncated,
    /// Another format version.
    Version(u8),
    /// A known record is shorter than its body (the tag).
    Short(u8),
}

/// Groups of ABI imports a node actually serves, one bit each.
///
/// A bit means the runtime links the imports *and* the board does
/// something with them: an ESP32 without I2C links `i2c_open` (it is in
/// ABI 3) but answers `Unsupported`, so it leaves [`Groups::I2C`] out.
/// Unnamed bits are reserved: sent as 0, kept by decoders.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Groups(pub u16);

impl Groups {
    /// No groups.
    pub const NONE: Groups = Groups(0);
    /// `gpio_*` (ABI 1).
    pub const GPIO: Groups = Groups(1 << 0);
    /// `timer_*` (ABI 2).
    pub const TIMERS: Groups = Groups(1 << 1);
    /// `gpio_irq` (ABI 2).
    pub const IRQ: Groups = Groups(1 << 2);
    /// `spi_*` (ABI 2).
    pub const SPI: Groups = Groups(1 << 3);
    /// `uart_*` (ABI 2).
    pub const UART: Groups = Groups(1 << 4);
    /// `dma_read` (ABI 2).
    pub const DMA: Groups = Groups(1 << 5);
    /// `pwm_*` (ABI 3).
    pub const PWM: Groups = Groups(1 << 6);
    /// `adc_read` (ABI 3).
    pub const ADC: Groups = Groups(1 << 7);
    /// `i2c_*` (ABI 3).
    pub const I2C: Groups = Groups(1 << 8);
    /// `route_*` (ABI 4).
    pub const ROUTES: Groups = Groups(1 << 9);
    /// `serve`, `node_id` with requests routed to the payload (ABI 5;
    /// [`Caps::SERVE`]).
    pub const SERVE: Groups = Groups(1 << 10);
    /// `route_bridge` (ABI 6; [`Caps::BRIDGE`]).
    pub const BRIDGE: Groups = Groups(1 << 11);
    /// `channel_open`, `channel_publish` with bindings (ABI 8).
    pub const CHANNELS: Groups = Groups(1 << 12);

    /// Every named group with its name, lowest bit first.
    pub const NAMED: [(Groups, &'static str); 13] = [
        (Groups::GPIO, "gpio"),
        (Groups::TIMERS, "timers"),
        (Groups::IRQ, "irq"),
        (Groups::SPI, "spi"),
        (Groups::UART, "uart"),
        (Groups::DMA, "dma"),
        (Groups::PWM, "pwm"),
        (Groups::ADC, "adc"),
        (Groups::I2C, "i2c"),
        (Groups::ROUTES, "routes"),
        (Groups::SERVE, "serve"),
        (Groups::BRIDGE, "bridge"),
        (Groups::CHANNELS, "channels"),
    ];

    /// Whether every bit of `other` is set.
    pub const fn contains(self, other: Groups) -> bool {
        self.0 & other.0 == other.0
    }

    /// The union of both sets.
    pub const fn with(self, other: Groups) -> Groups {
        Groups(self.0 | other.0)
    }
}

/// The node's I2C controllers: buses `0..buses`, each between `min_hz`
/// and `max_hz`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct I2cBuses {
    /// Number of buses.
    pub buses: u8,
    /// Slowest clock, Hz.
    pub min_hz: u32,
    /// Fastest clock, Hz.
    pub max_hz: u32,
}

/// The node's SPI controllers: buses `0..buses`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpiBuses {
    /// Number of buses.
    pub buses: u8,
}

/// Bit `i` of a mask over a table: whether the table's entry `i` is in.
const fn bit(mask: u8, i: usize) -> bool {
    i < 8 && mask & (1 << i) != 0
}

/// The mask with every entry of a table of `n` entries.
const fn all(n: usize) -> u8 {
    if n >= 8 { u8::MAX } else { (1u8 << n) - 1 }
}

/// Index of `v` in a `u16` table.
const fn index_u16(table: &[u16], v: u16) -> Option<usize> {
    let mut i = 0;
    while i < table.len() {
        if table[i] == v {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// The IMU source ([`SourceKind::Imu`]): which of [`IMU_RATES_HZ`] and
/// [`IMU_RANGES_G`] it takes, with and without the magnetometer (a
/// sensor in hybrid mode may sample both at fewer rates).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImuSource {
    /// Bit `i`: [`IMU_RATES_HZ`]`[i]` is offered for 3-channel samples.
    pub rates: u8,
    /// Bit `i`: [`IMU_RANGES_G`]`[i]` is offered.
    pub ranges: u8,
    /// Bit `i`: [`IMU_RATES_HZ`]`[i]` is offered for 6-channel samples
    /// (accelerometer and magnetometer); 0 without a magnetometer.
    pub mag_rates: u8,
}

impl ImuSource {
    /// Every rate and range of the ABI tables, with or without the
    /// magnetometer.
    pub const ALL: ImuSource = ImuSource {
        rates: all(IMU_RATES_HZ.len()),
        ranges: all(IMU_RANGES_G.len()),
        mag_rates: all(IMU_RATES_HZ.len()),
    };

    /// The mask with each of `rates_hz` (entries of [`IMU_RATES_HZ`]);
    /// other rates are left out.
    pub const fn rates_mask(rates_hz: &[u16]) -> u8 {
        let mut m = 0;
        let mut i = 0;
        while i < rates_hz.len() {
            if let Some(b) = index_u16(&IMU_RATES_HZ, rates_hz[i]) {
                m |= 1 << b;
            }
            i += 1;
        }
        m
    }

    /// The offered 3-channel rates in Hz, lowest first.
    pub fn rates_hz(&self) -> impl Iterator<Item = u16> + '_ {
        IMU_RATES_HZ
            .iter()
            .enumerate()
            .filter(|(i, _)| bit(self.rates, *i))
            .map(|(_, r)| *r)
    }

    /// The offered 6-channel rates in Hz, lowest first.
    pub fn mag_rates_hz(&self) -> impl Iterator<Item = u16> + '_ {
        IMU_RATES_HZ
            .iter()
            .enumerate()
            .filter(|(i, _)| bit(self.mag_rates, *i))
            .map(|(_, r)| *r)
    }

    /// The offered ranges in ±g, smallest first.
    pub fn ranges_g(&self) -> impl Iterator<Item = u8> + '_ {
        IMU_RANGES_G
            .iter()
            .enumerate()
            .filter(|(i, _)| bit(self.ranges, *i))
            .map(|(_, r)| *r)
    }

    /// Whether a (valid) config asks only for what is offered.
    pub const fn accepts(&self, cfg: &ImuConfig) -> bool {
        let mask = if cfg.mag { self.mag_rates } else { self.rates };
        let rate = match index_u16(&IMU_RATES_HZ, cfg.rate_hz) {
            Some(i) => bit(mask, i),
            None => false,
        };
        let mut range = false;
        let mut i = 0;
        while i < IMU_RANGES_G.len() {
            if IMU_RANGES_G[i] == cfg.range_g && bit(self.ranges, i) {
                range = true;
            }
            i += 1;
        }
        rate && range
    }
}

/// The microphone source ([`SourceKind::Microphone`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MicSource {
    /// Bit `i`: [`MIC_RATES_HZ`]`[i]` is offered.
    pub rates: u8,
    /// Bit `m`: [`AudioMode`] code `m` is offered.
    pub modes: u8,
    /// Highest input gain, dB.
    pub max_gain_db: u8,
}

impl MicSource {
    /// The offered rates in Hz, lowest first.
    pub fn rates_hz(&self) -> impl Iterator<Item = u16> + '_ {
        MIC_RATES_HZ
            .iter()
            .enumerate()
            .filter(|(i, _)| bit(self.rates, *i))
            .map(|(_, r)| *r)
    }

    /// Whether `mode` is offered.
    pub const fn has_mode(&self, mode: AudioMode) -> bool {
        bit(self.modes, mode as usize)
    }

    /// Whether a (valid) config asks only for what is offered.
    pub const fn accepts(&self, cfg: &MicConfig) -> bool {
        let rate = match index_u16(&MIC_RATES_HZ, cfg.rate_hz) {
            Some(i) => bit(self.rates, i),
            None => false,
        };
        rate && self.has_mode(cfg.mode) && cfg.gain_db <= self.max_gain_db
    }
}

/// The frame sources ([`SourceKind::TestPattern`], [`SourceKind::Camera`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameSources {
    /// Bit `k`: [`SourceKind`] code `k` is offered.
    pub kinds: u8,
    /// Bit `f`: [`PixelFormat`](crate::wasm_abi::route::PixelFormat) code `f` is offered.
    pub formats: u8,
    /// Widest frame, pixels.
    pub max_width: u16,
    /// Tallest frame, pixels.
    pub max_height: u16,
}

impl FrameSources {
    /// Whether `kind` is offered.
    pub const fn has(&self, kind: SourceKind) -> bool {
        bit(self.kinds, kind as usize)
    }

    /// Whether a (valid) frame source config asks only for what is
    /// offered.
    pub const fn accepts(&self, kind: SourceKind, cfg: &SourceConfig) -> bool {
        self.has(kind)
            && bit(self.formats, cfg.format as usize)
            && cfg.width <= self.max_width
            && cfg.height <= self.max_height
    }
}

/// SPI sink routes (`route_sink`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpiSink {
    /// Largest frame, bytes.
    pub max_frame: u32,
}

/// What a link carries; codes outside the named ones are kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LinkKind(pub u8);

impl LinkKind {
    /// A UART (the serial framing of [`crate::serial`]).
    pub const SERIAL: LinkKind = LinkKind(0);
    /// USB CDC-ACM with the serial framing.
    pub const USB: LinkKind = LinkKind(1);
    /// A Zenoh session ([`crate::zenoh_lite`]).
    pub const ZENOH: LinkKind = LinkKind(2);
    /// ESP-NOW.
    pub const ESPNOW: LinkKind = LinkKind(3);

    /// The kind's name, `None` for an unnamed code.
    pub const fn name(self) -> Option<&'static str> {
        match self.0 {
            0 => Some("serial"),
            1 => Some("usb"),
            2 => Some("zenoh"),
            3 => Some("espnow"),
            _ => None,
        }
    }
}

/// One link: the id `route_bridge` names and what it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Link {
    /// The link id (`route_bridge`'s `src_link` / `dst_link`).
    pub id: u8,
    /// What it carries.
    pub kind: LinkKind,
}

/// Runtime limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Routes a payload may run at once.
    pub routes: u8,
    /// Subscriptions a payload may hold.
    pub subscriptions: u8,
    /// Channels a payload may declare (ABI 8; 0 before).
    pub channels: u8,
    /// Largest message, bytes.
    pub max_message: u16,
}

impl Limits {
    /// The ABI's own limits ([`crate::wasm_abi`]).
    pub const ABI: Limits = Limits {
        routes: crate::wasm_abi::route::MAX_ROUTES as u8,
        subscriptions: crate::wasm_abi::MAX_SUBSCRIPTIONS as u8,
        channels: crate::wasm_abi::channels::MAX_CHANNELS as u8,
        max_message: crate::wasm_abi::MAX_MESSAGE as u16,
    };
}

/// A node's manifest. Build one with [`Manifest::new`] and the `with_*`
/// methods (all `const`), encode it with [`Manifest::encode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Manifest {
    /// [`crate::wasm_abi::ABI_VERSION`] of the runtime.
    pub abi: u16,
    /// The node id, as in `sys/info`.
    pub node: Option<SenderId>,
    /// Import groups served.
    pub groups: Groups,
    /// I2C controllers.
    pub i2c: Option<I2cBuses>,
    /// SPI controllers.
    pub spi: Option<SpiBuses>,
    /// The IMU source.
    pub imu: Option<ImuSource>,
    /// The microphone source.
    pub mic: Option<MicSource>,
    /// Frame sources.
    pub frames: Option<FrameSources>,
    /// SPI sink routes.
    pub sink_spi: Option<SpiSink>,
    links: [Link; MAX_LINKS],
    n_links: u8,
    /// Runtime limits.
    pub limits: Option<Limits>,
    /// The board.
    pub board: Option<BoardName>,
}

const NO_LINK: Link = Link {
    id: 0,
    kind: LinkKind(0),
};

impl Manifest {
    /// An empty manifest for a runtime of ABI `abi`.
    pub const fn new(abi: u16) -> Self {
        Manifest {
            abi,
            node: None,
            groups: Groups::NONE,
            i2c: None,
            spi: None,
            imu: None,
            mic: None,
            frames: None,
            sink_spi: None,
            links: [NO_LINK; MAX_LINKS],
            n_links: 0,
            limits: None,
            board: None,
        }
    }

    /// With the board's name.
    pub const fn with_board(self, board: BoardName) -> Self {
        Manifest {
            board: Some(board),
            ..self
        }
    }

    /// With the node id.
    pub const fn with_node(self, node: SenderId) -> Self {
        Manifest {
            node: Some(node),
            ..self
        }
    }

    /// Without a node id.
    pub const fn without_node(self) -> Self {
        Manifest { node: None, ..self }
    }

    /// With these import groups.
    pub const fn with_groups(self, groups: Groups) -> Self {
        Manifest { groups, ..self }
    }

    /// With I2C controllers.
    pub const fn with_i2c(self, i2c: I2cBuses) -> Self {
        Manifest {
            i2c: Some(i2c),
            ..self
        }
    }

    /// With SPI controllers.
    pub const fn with_spi(self, spi: SpiBuses) -> Self {
        Manifest {
            spi: Some(spi),
            ..self
        }
    }

    /// With the IMU source.
    pub const fn with_imu(self, imu: ImuSource) -> Self {
        Manifest {
            imu: Some(imu),
            ..self
        }
    }

    /// With the microphone source.
    pub const fn with_mic(self, mic: MicSource) -> Self {
        Manifest {
            mic: Some(mic),
            ..self
        }
    }

    /// With frame sources.
    pub const fn with_frames(self, frames: FrameSources) -> Self {
        Manifest {
            frames: Some(frames),
            ..self
        }
    }

    /// With SPI sink routes.
    pub const fn with_sink_spi(self, sink: SpiSink) -> Self {
        Manifest {
            sink_spi: Some(sink),
            ..self
        }
    }

    /// With one more link; past [`MAX_LINKS`] the link is left out.
    pub const fn with_link(self, link: Link) -> Self {
        let mut m = self;
        if (m.n_links as usize) < MAX_LINKS {
            m.links[m.n_links as usize] = link;
            m.n_links += 1;
        }
        m
    }

    /// With runtime limits.
    pub const fn with_limits(self, limits: Limits) -> Self {
        Manifest {
            limits: Some(limits),
            ..self
        }
    }

    /// The same manifest for a firmware built without the WASM runtime:
    /// no import groups and no sources or sinks (what a payload could
    /// use), node, buses and links kept.
    pub const fn without_runtime(self) -> Self {
        Manifest {
            groups: Groups::NONE,
            imu: None,
            mic: None,
            frames: None,
            sink_spi: None,
            limits: None,
            ..self
        }
    }

    /// The links, in record order.
    pub fn links(&self) -> &[Link] {
        &self.links[..usize::from(self.n_links)]
    }

    /// The `sys/info` capability bits this manifest implies:
    /// [`Caps::SERVE`] and [`Caps::BRIDGE`] from the groups,
    /// [`Caps::SOURCE_IMU`] and [`Caps::SOURCE_MICROPHONE`] from the
    /// sources.
    pub const fn caps(&self) -> Caps {
        let mut c = Caps::NONE;
        if self.groups.contains(Groups::SERVE) {
            c = c.with(Caps::SERVE);
        }
        if self.groups.contains(Groups::BRIDGE) {
            c = c.with(Caps::BRIDGE);
        }
        if self.imu.is_some() {
            c = c.with(Caps::SOURCE_IMU);
        }
        if self.mic.is_some() {
            c = c.with(Caps::SOURCE_MICROPHONE);
        }
        c
    }

    /// Whether a sampled source config asks only for what the manifest
    /// offers (`false` for a source it lacks).
    pub const fn accepts_samples(&self, source: &SampleSource) -> bool {
        match (source, &self.imu, &self.mic) {
            (SampleSource::Imu(c), Some(imu), _) => imu.accepts(c),
            (SampleSource::Microphone(c), _, Some(mic)) => mic.accepts(c),
            _ => false,
        }
    }

    /// Writes the manifest into `out`; returns its length, `None` if `out`
    /// is too short.
    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        let mut w = Writer { out, at: 0 };
        w.put(&[VERSION])?;
        w.put(&self.abi.to_le_bytes())?;
        if let Some(node) = self.node {
            w.record(tag::NODE, &node.0)?;
        }
        w.record(tag::IMPORT_GROUPS, &self.groups.0.to_le_bytes())?;
        if let Some(i) = self.i2c {
            let (lo, hi) = (i.min_hz.to_le_bytes(), i.max_hz.to_le_bytes());
            w.record(
                tag::I2C,
                &[
                    i.buses, lo[0], lo[1], lo[2], lo[3], hi[0], hi[1], hi[2], hi[3],
                ],
            )?;
        }
        if let Some(s) = self.spi {
            w.record(tag::SPI, &[s.buses])?;
        }
        if let Some(i) = self.imu {
            w.record(tag::SOURCE_IMU, &[i.rates, i.ranges, i.mag_rates])?;
        }
        if let Some(m) = self.mic {
            w.record(tag::SOURCE_MIC, &[m.rates, m.modes, m.max_gain_db])?;
        }
        if let Some(f) = self.frames {
            let (wd, ht) = (f.max_width.to_le_bytes(), f.max_height.to_le_bytes());
            w.record(
                tag::SOURCE_FRAMES,
                &[f.kinds, f.formats, wd[0], wd[1], ht[0], ht[1]],
            )?;
        }
        if let Some(s) = self.sink_spi {
            w.record(tag::SINK_SPI, &s.max_frame.to_le_bytes())?;
        }
        for l in self.links() {
            w.record(tag::LINK, &[l.id, l.kind.0])?;
        }
        if let Some(l) = self.limits {
            let m = l.max_message.to_le_bytes();
            w.record(
                tag::LIMITS,
                &[l.routes, l.subscriptions, l.channels, m[0], m[1]],
            )?;
        }
        if let Some(b) = &self.board {
            w.record(tag::BOARD, b.as_bytes())?;
        }
        Some(w.at)
    }

    /// Decodes a `sys/manifest` response (see the module docs for what is
    /// skipped and what refused).
    pub fn decode(bytes: &[u8]) -> Result<Self, ManifestError> {
        let (abi, records) = records(bytes)?;
        let mut m = Manifest::new(abi);
        for r in records {
            let (t, body) = r?;
            let need = |n: usize| body.get(..n).ok_or(ManifestError::Short(t));
            let u32_at =
                |b: &[u8], at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
            match t {
                tag::NODE => {
                    let b = need(8)?;
                    let mut id = [0u8; 8];
                    id.copy_from_slice(b);
                    m.node = Some(SenderId(id));
                }
                tag::IMPORT_GROUPS => {
                    let b = need(2)?;
                    m.groups = Groups(u16::from_le_bytes([b[0], b[1]]));
                }
                tag::I2C => {
                    let b = need(9)?;
                    m.i2c = Some(I2cBuses {
                        buses: b[0],
                        min_hz: u32_at(b, 1),
                        max_hz: u32_at(b, 5),
                    });
                }
                tag::SPI => m.spi = Some(SpiBuses { buses: need(1)?[0] }),
                tag::SOURCE_IMU => {
                    let b = need(3)?;
                    m.imu = Some(ImuSource {
                        rates: b[0],
                        ranges: b[1],
                        mag_rates: b[2],
                    });
                }
                tag::SOURCE_MIC => {
                    let b = need(3)?;
                    m.mic = Some(MicSource {
                        rates: b[0],
                        modes: b[1],
                        max_gain_db: b[2],
                    });
                }
                tag::SOURCE_FRAMES => {
                    let b = need(6)?;
                    m.frames = Some(FrameSources {
                        kinds: b[0],
                        formats: b[1],
                        max_width: u16::from_le_bytes([b[2], b[3]]),
                        max_height: u16::from_le_bytes([b[4], b[5]]),
                    });
                }
                tag::SINK_SPI => {
                    m.sink_spi = Some(SpiSink {
                        max_frame: u32_at(need(4)?, 0),
                    })
                }
                tag::LINK => {
                    let b = need(2)?;
                    m = m.with_link(Link {
                        id: b[0],
                        kind: LinkKind(b[1]),
                    });
                }
                tag::LIMITS => {
                    let b = need(5)?;
                    m.limits = Some(Limits {
                        routes: b[0],
                        subscriptions: b[1],
                        channels: b[2],
                        max_message: u16::from_le_bytes([b[3], b[4]]),
                    });
                }
                tag::BOARD => {
                    // Raw bytes, cut as a sender would have; a name that
                    // is not UTF-8 is kept and `as_str` says so.
                    need(1)?;
                    let n = body.len().min(BOARD_NAME_MAX);
                    let mut bytes = [0u8; BOARD_NAME_MAX];
                    bytes[..n].copy_from_slice(&body[..n]);
                    m.board = Some(BoardName {
                        len: n as u8,
                        bytes,
                    });
                }
                _ => {}
            }
        }
        Ok(m)
    }
}

struct Writer<'o> {
    out: &'o mut [u8],
    at: usize,
}

impl Writer<'_> {
    fn put(&mut self, bytes: &[u8]) -> Option<()> {
        let end = self.at.checked_add(bytes.len())?;
        if end > MANIFEST_MAX {
            return None;
        }
        self.out.get_mut(self.at..end)?.copy_from_slice(bytes);
        self.at = end;
        Some(())
    }

    fn record(&mut self, t: u8, body: &[u8]) -> Option<()> {
        self.put(&[t, u8::try_from(body.len()).ok()?])?;
        self.put(body)
    }
}

/// The records of an encoded manifest, in order: `(tag, body)`, unknown
/// tags included. Ends after the first error.
pub struct Records<'a> {
    rest: &'a [u8],
    failed: bool,
}

impl<'a> Iterator for Records<'a> {
    type Item = Result<(u8, &'a [u8]), ManifestError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.rest.is_empty() {
            return None;
        }
        let item = match self.rest {
            [t, len, tail @ ..] if tail.len() >= usize::from(*len) => {
                let (body, rest) = tail.split_at(usize::from(*len));
                self.rest = rest;
                Ok((*t, body))
            }
            _ => Err(ManifestError::Truncated),
        };
        self.failed = item.is_err();
        Some(item)
    }
}

/// Checks the header of an encoded manifest; returns its ABI and the
/// records that follow.
pub fn records(bytes: &[u8]) -> Result<(u16, Records<'_>), ManifestError> {
    match bytes {
        [VERSION, a0, a1, rest @ ..] => Ok((
            u16::from_le_bytes([*a0, *a1]),
            Records {
                rest,
                failed: false,
            },
        )),
        [v, _, _, ..] => Err(ManifestError::Version(*v)),
        _ => Err(ManifestError::Truncated),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wasm_abi::route::PixelFormat;

    fn full() -> Manifest {
        Manifest::new(7)
            .with_node(SenderId([1, 2, 3, 4, 5, 6, 0, 0]))
            .with_groups(Groups::GPIO.with(Groups::I2C).with(Groups::SERVE))
            .with_i2c(I2cBuses {
                buses: 1,
                min_hz: 100_000,
                max_hz: 400_000,
            })
            .with_spi(SpiBuses { buses: 2 })
            .with_imu(ImuSource::ALL)
            .with_mic(MicSource {
                rates: 0b0011,
                modes: 0b111,
                max_gain_db: 30,
            })
            .with_frames(FrameSources {
                kinds: 1 << SourceKind::TestPattern as u8,
                formats: 1 << PixelFormat::Rgb565Be as u8,
                max_width: 240,
                max_height: 240,
            })
            .with_sink_spi(SpiSink { max_frame: 115_200 })
            .with_link(Link {
                id: 0,
                kind: LinkKind::USB,
            })
            .with_link(Link {
                id: 1,
                kind: LinkKind::ZENOH,
            })
            .with_limits(Limits::ABI)
            .with_board(BoardName::new("ZynqMP KD240 revA/B/1").unwrap())
    }

    fn encode(m: &Manifest) -> ([u8; MANIFEST_MAX], usize) {
        let mut out = [0u8; MANIFEST_MAX];
        let n = m.encode(&mut out).expect("fits");
        (out, n)
    }

    #[test]
    fn mf1_every_record_round_trips() {
        let m = full();
        let (out, n) = encode(&m);
        assert_eq!(Manifest::decode(&out[..n]), Ok(m));
        assert_eq!(m.links().len(), 2);
        let empty = Manifest::new(6);
        let (out, n) = encode(&empty);
        // Header and the groups record only.
        assert_eq!(&out[..n], &[1, 6, 0, tag::IMPORT_GROUPS, 2, 0, 0]);
        assert_eq!(Manifest::decode(&out[..n]), Ok(empty));
    }

    #[test]
    fn mf1_unknown_tags_are_skipped_and_longer_records_keep_their_prefix() {
        let (out, n) = encode(&full());
        let mut v = out[..n].to_vec();
        // An unknown record in the middle and at the end.
        v.splice(3..3, [200u8, 3, 9, 9, 9]);
        v.extend_from_slice(&[201, 0]);
        assert_eq!(Manifest::decode(&v), Ok(full()));
        // A SPI record with an appended field.
        let mut w = vec![1, 7, 0, tag::SPI, 3, 4, 0xAA, 0xBB];
        assert_eq!(
            Manifest::decode(&w).map(|m| m.spi),
            Ok(Some(SpiBuses { buses: 4 }))
        );
        // ...but not a shorter one.
        w = vec![1, 7, 0, tag::I2C, 2, 1, 0];
        assert_eq!(Manifest::decode(&w), Err(ManifestError::Short(tag::I2C)));
        let ids: Vec<u8> = records(&v).unwrap().1.map(|r| r.unwrap().0).collect();
        assert_eq!(ids.first(), Some(&200));
        assert_eq!(ids.last(), Some(&201));
    }

    #[test]
    fn mf1_truncation_and_other_versions_are_refused() {
        let (out, n) = encode(&full());
        for cut in 0..n {
            let d = Manifest::decode(&out[..cut]);
            // A cut exactly on a record boundary is a shorter, valid
            // manifest; anything else is refused.
            if let Ok(m) = d {
                assert_ne!(m, full(), "cut {cut}");
            } else {
                assert_eq!(d, Err(ManifestError::Truncated), "cut {cut}");
            }
        }
        let mut v = out[..n].to_vec();
        v[0] = 2;
        assert_eq!(Manifest::decode(&v), Err(ManifestError::Version(2)));
        // At most MAX_LINKS links are kept.
        let mut m = Manifest::new(7);
        for id in 0..6 {
            m = m.with_link(Link {
                id,
                kind: LinkKind::SERIAL,
            });
        }
        assert_eq!(m.links().len(), MAX_LINKS);
    }

    #[test]
    fn mf1_the_largest_manifest_fits_a_reply() {
        let mut m = full().with_board(BoardName::new(&"x".repeat(80)).unwrap());
        for id in 2..MAX_LINKS as u8 {
            m = m.with_link(Link {
                id,
                kind: LinkKind::SERIAL,
            });
        }
        let (_, n) = encode(&m);
        assert!(n <= MANIFEST_MAX && MANIFEST_MAX <= crate::wasm_abi::MAX_MESSAGE);
        let mut short = [0u8; 8];
        assert_eq!(m.encode(&mut short), None);
    }

    #[test]
    fn mf6_board_names() {
        let kd = BoardName::new("ZynqMP KD240 revA/B/1").unwrap();
        assert_eq!(kd.as_str(), Some("ZynqMP KD240 revA/B/1"));
        assert_eq!(BoardName::new(""), None);
        // Cut to the limit, never inside a character.
        let long = BoardName::new(&"a".repeat(40)).unwrap();
        assert_eq!(long.as_bytes().len(), BOARD_NAME_MAX);
        let name = format!("{}é", "a".repeat(BOARD_NAME_MAX - 1));
        let cut = BoardName::new(&name).unwrap();
        assert_eq!(cut.as_str(), Some(&name[..BOARD_NAME_MAX - 1]));
        // On the wire: tag 11, the bytes; an older decoder skips it.
        let m = Manifest::new(8).with_board(kd);
        let (out, n) = encode(&m);
        assert_eq!(&out[n - 23..n - 21], &[tag::BOARD, 21]);
        assert_eq!(Manifest::decode(&out[..n]).unwrap().board, Some(kd));
        assert_eq!(
            Manifest::decode(&[1, 8, 0, tag::BOARD, 0]),
            Err(ManifestError::Short(tag::BOARD))
        );
        // A peer's name that is not UTF-8 is kept as bytes.
        let odd = Manifest::decode(&[1, 8, 0, tag::BOARD, 2, 0xff, 0xfe])
            .unwrap()
            .board
            .unwrap();
        assert_eq!((odd.as_bytes(), odd.as_str()), (&[0xff, 0xfe][..], None));
    }

    #[test]
    fn mf2_caps_follow_the_groups_and_sources() {
        assert_eq!(Manifest::new(7).caps(), Caps::NONE);
        let serve = Manifest::new(7).with_groups(Groups::SERVE);
        assert_eq!(serve.caps(), Caps::SERVE);
        let bridge = Manifest::new(7).with_groups(Groups::BRIDGE);
        assert_eq!(bridge.caps(), Caps::BRIDGE);
        let imu = Manifest::new(7).with_imu(ImuSource::ALL);
        assert_eq!(imu.caps(), Caps::SOURCE_IMU);
        let mic = Manifest::new(7).with_mic(MicSource {
            rates: 1,
            modes: 1,
            max_gain_db: 0,
        });
        assert_eq!(mic.caps(), Caps::SOURCE_MICROPHONE);
        assert_eq!(full().without_runtime().caps(), Caps::NONE);
        // Every source kind with a cap bit maps to one record.
        for kind in [SourceKind::Imu, SourceKind::Microphone] {
            let m = match kind {
                SourceKind::Imu => imu,
                _ => mic,
            };
            assert_eq!(Caps::source(kind), Some(m.caps()));
        }
        let names: Vec<_> = Groups::NAMED.iter().map(|(_, n)| *n).collect();
        assert_eq!(names.len(), 13);
        for (i, (g, _)) in Groups::NAMED.iter().enumerate() {
            assert_eq!(g.0, 1 << i);
        }
    }

    #[test]
    fn mf2_sources_accept_exactly_what_they_list() {
        let imu = ImuSource {
            rates: ImuSource::rates_mask(&[100, 200, 44]),
            ranges: 0b001, // ±2 g
            mag_rates: ImuSource::rates_mask(&[100]),
        };
        assert_eq!(imu.rates, 0b00110);
        assert_eq!(imu.rates_hz().collect::<Vec<_>>(), [100, 200]);
        assert_eq!(imu.mag_rates_hz().collect::<Vec<_>>(), [100]);
        assert_eq!(imu.ranges_g().collect::<Vec<_>>(), [2]);
        let cfg = |rate, range, mag| ImuConfig::new(rate, range, mag, 10).unwrap();
        assert!(imu.accepts(&cfg(100, 2, false)));
        assert!(!imu.accepts(&cfg(50, 2, false)));
        assert!(!imu.accepts(&cfg(100, 4, false)));
        assert!(imu.accepts(&cfg(100, 2, true)));
        assert!(!imu.accepts(&cfg(200, 2, true)));
        let mic = MicSource {
            rates: 0b0010,
            modes: 1 << AudioMode::Levels as u8,
            max_gain_db: 20,
        };
        assert_eq!(mic.rates_hz().collect::<Vec<_>>(), [16_000]);
        let levels = MicConfig::new(16_000, 20, AudioMode::Levels, 10, 160).unwrap();
        assert!(mic.accepts(&levels));
        assert!(!mic.accepts(&MicConfig {
            gain_db: 21,
            ..levels
        }));
        assert!(!mic.accepts(&MicConfig {
            rate_hz: 8_000,
            ..levels
        }));
        let pcm = MicConfig::new(16_000, 0, AudioMode::Pcm, 200, 0).unwrap();
        assert!(!mic.accepts(&pcm));
        let m = Manifest::new(7).with_mic(mic);
        assert!(m.accepts_samples(&SampleSource::Microphone(levels)));
        assert!(!m.accepts_samples(&SampleSource::Imu(cfg(100, 2, false))));
        let frames = full().frames.unwrap();
        let fc = SourceConfig::parse(&[240, 0, 240, 0, 0, 0, 100, 0, 0, 0]).unwrap();
        assert!(frames.accepts(SourceKind::TestPattern, &fc));
        assert!(!frames.accepts(SourceKind::Camera, &fc));
    }
}
