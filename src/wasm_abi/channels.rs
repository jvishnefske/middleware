//! ABI 8: a payload's logical channels, declared in its image (CH1–CH6).
//!
//! A payload names the data it reads and writes ("accel", "audio", "cmd")
//! instead of choosing route kinds and keys itself. The declarations live
//! in a WebAssembly custom section named [`SECTION`] (`mw.channels`), so
//! they travel inside the image and the image's signed commit covers them.
//! A separate signed binding record ([`crate::loader::binding`]) maps each
//! name to a target: a local source, a key, a key from one other node, or
//! an adapted stream. The runtime resolves the bindings before `init`.
//!
//! # `mw.channels` v1
//!
//! `[version u8 = 1][n u8]`, then `n` entries ([`ENTRY_FIXED`] bytes after
//! the name):
//!
//! | bytes | field                                                         |
//! |-------|---------------------------------------------------------------|
//! | 1     | name length, `1..=MAX_NAME`                                    |
//! | …     | name: `a-z`, `0-9`, `_`                                        |
//! | 1     | [`Direction`]: 0 in, 1 out                                     |
//! | 1     | [`DataKind`]: 0 bytes, 1 samples, 2 frames                     |
//! | 1     | sensor: a [`SensorKind`] code, or [`ANY`] (always for bytes and frames) |
//! | 1     | formats: mask over [`SampleFormat`] codes (samples) or [`PixelFormat`](super::route::PixelFormat) codes (frames); 0 = any; 0 for bytes |
//! | 1     | channels per unit, 0 = any                                    |
//! | 4     | lowest rate, Hz (`u32` LE)                                     |
//! | 4     | highest rate, Hz, `>=` the lowest; `0, u32::MAX` = any         |
//! | 1     | flags: bit 0 required; other bits 0                           |
//!
//! Names are unique and at most [`MAX_CHANNELS`] are declared. The section
//! must end after the last entry; anything else is refused
//! ([`ChannelError`]), and so is a module with the section twice.
//!
//! # Imports
//!
//! | name              | params                        | result            |
//! |-------------------|-------------------------------|-------------------|
//! | `channel_open`    | `name_ptr, name_len`          | handle ≥ 0 or error |
//! | `channel_publish` | `handle, data_ptr, data_len`  | 0 or error        |
//!
//! An input channel's handle is the subscription slot `on_message`
//! delivers it on (`0..MAX_SUBSCRIPTIONS`); an output channel's handle is
//! [`OUTPUT_BASE`] plus its index in the declaration. `channel_open` of a
//! name the image does not declare is `InvalidArgument`; of a declared
//! channel the binding left unbound (an optional one) `Unsupported`.
//! `channel_publish` on an input handle, or one not open, is
//! `InvalidArgument`.

use crate::stream::samples::{SampleFormat, SensorKind};

/// Name of the custom section.
pub const SECTION: &str = "mw.channels";

/// The section format version.
pub const VERSION: u8 = 1;

/// Most channels a payload declares.
pub const MAX_CHANNELS: usize = 8;

/// Longest channel name, bytes.
pub const MAX_NAME: usize = 24;

/// Bytes of an entry after its name (the name length byte excluded).
pub const ENTRY_FIXED: usize = 14;

/// "Any" for the sensor byte.
pub const ANY: u8 = 0xFF;

/// First output channel handle.
pub const OUTPUT_BASE: i32 = 16;

/// Largest `mw.channels` section: header and eight longest entries.
pub const MAX_SECTION: usize = 2 + MAX_CHANNELS * (1 + MAX_NAME + ENTRY_FIXED);

/// Whether the payload reads or writes the channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Data reaches the payload (`on_message`).
    In = 0,
    /// The payload publishes (`channel_publish`).
    Out = 1,
}

/// What a channel carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataKind {
    /// Opaque messages.
    Bytes = 0,
    /// [`crate::stream::samples`] batches.
    Samples = 1,
    /// [`crate::stream`] frame chunks.
    Frames = 2,
}

/// Why a `mw.channels` section was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelError {
    /// The section ends inside an entry, or bytes follow the last one.
    Length,
    /// Another format version.
    Version,
    /// More than [`MAX_CHANNELS`].
    TooMany,
    /// A name is empty, too long, or has a character outside `a-z0-9_`.
    Name,
    /// Two entries share a name.
    Duplicate,
    /// A field is out of range (direction, kind, sensor, rates, flags).
    Field,
    /// The module declares the section more than once, or its section
    /// headers are malformed.
    Module,
}

/// One declared channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Channel<'a> {
    /// The name the payload opens and the binding names.
    pub name: &'a str,
    /// In or out.
    pub dir: Direction,
    /// What it carries.
    pub kind: DataKind,
    /// For samples, the sensor it wants; `None` = any.
    pub sensor: Option<SensorKind>,
    /// Mask over format codes; 0 = any.
    pub formats: u8,
    /// Channels per unit; 0 = any.
    pub channels: u8,
    /// Lowest acceptable rate, Hz.
    pub rate_min: u32,
    /// Highest acceptable rate, Hz.
    pub rate_max: u32,
    /// Whether the payload cannot start without it.
    pub required: bool,
}

impl<'a> Channel<'a> {
    /// A channel of opaque messages.
    pub const fn bytes(name: &'a str, dir: Direction, required: bool) -> Self {
        Channel {
            name,
            dir,
            kind: DataKind::Bytes,
            sensor: None,
            formats: 0,
            channels: 0,
            rate_min: 0,
            rate_max: u32::MAX,
            required,
        }
    }

    /// A channel of sample batches from `sensor` (`None` = any) at a rate
    /// in `rate_min..=rate_max`.
    pub const fn samples(
        name: &'a str,
        dir: Direction,
        sensor: Option<SensorKind>,
        rate_min: u32,
        rate_max: u32,
        required: bool,
    ) -> Self {
        Channel {
            kind: DataKind::Samples,
            sensor,
            rate_min,
            rate_max,
            ..Channel::bytes(name, dir, required)
        }
    }

    /// A channel of frame chunks.
    pub const fn frames(name: &'a str, dir: Direction, required: bool) -> Self {
        Channel {
            kind: DataKind::Frames,
            ..Channel::bytes(name, dir, required)
        }
    }

    /// The same channel accepting only these format codes (a mask).
    pub const fn with_formats(self, formats: u8) -> Self {
        Channel { formats, ..self }
    }

    /// The same channel with this many channels per unit.
    pub const fn with_channels(self, channels: u8) -> Self {
        Channel { channels, ..self }
    }

    /// Whether a stream of sample batches with these properties fits the
    /// declaration (`false` for a channel that is not of samples).
    pub const fn accepts_samples(
        &self,
        sensor: SensorKind,
        format: SampleFormat,
        channels: u8,
        rate_hz: u32,
    ) -> bool {
        let sensor_ok = match self.sensor {
            Some(s) => s as u8 == sensor as u8,
            None => true,
        };
        matches!(self.kind, DataKind::Samples)
            && sensor_ok
            && (self.formats == 0 || self.formats & (1 << format as u8) != 0)
            && (self.channels == 0 || self.channels == channels)
            && rate_hz >= self.rate_min
            && rate_hz <= self.rate_max
    }

    /// Checks the field rules (name, sensor only for samples, formats
    /// zero for bytes, ordered rates).
    pub const fn validated(self) -> Result<Self, ChannelError> {
        if !valid_name(self.name.as_bytes()) {
            return Err(ChannelError::Name);
        }
        let plain = !matches!(self.kind, DataKind::Samples);
        if (plain && self.sensor.is_some())
            || (matches!(self.kind, DataKind::Bytes) && (self.formats != 0 || self.channels != 0))
            || self.rate_min > self.rate_max
        {
            return Err(ChannelError::Field);
        }
        Ok(self)
    }
}

/// Whether `name` is a valid channel name.
pub const fn valid_name(name: &[u8]) -> bool {
    if name.is_empty() || name.len() > MAX_NAME {
        return false;
    }
    let mut i = 0;
    while i < name.len() {
        let c = name[i];
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_') {
            return false;
        }
        i += 1;
    }
    true
}

/// Bytes [`encode`] writes for `decls`.
pub const fn encoded_len(decls: &[Channel<'_>]) -> usize {
    let mut n = 2;
    let mut i = 0;
    while i < decls.len() {
        n += 1 + decls[i].name.len() + ENTRY_FIXED;
        i += 1;
    }
    n
}

/// Encodes `decls` as a `mw.channels` section of exactly `N` =
/// [`encoded_len`] bytes. `const`, so the SDK's `channels!` macro builds
/// the section at compile time; a wrong `N`, an invalid entry, a duplicate
/// name or too many entries fail the build (a const panic).
pub const fn encode<const N: usize>(decls: &[Channel<'_>]) -> [u8; N] {
    assert!(N == encoded_len(decls), "mw.channels: wrong length");
    assert!(
        decls.len() <= MAX_CHANNELS,
        "mw.channels: too many channels"
    );
    let mut out = [0u8; N];
    out[0] = VERSION;
    out[1] = decls.len() as u8;
    let mut at = 2;
    let mut i = 0;
    while i < decls.len() {
        let c = decls[i];
        assert!(c.validated().is_ok(), "mw.channels: invalid channel");
        let mut j = 0;
        while j < i {
            assert!(
                !eq(decls[j].name.as_bytes(), c.name.as_bytes()),
                "mw.channels: duplicate name"
            );
            j += 1;
        }
        let name = c.name.as_bytes();
        out[at] = name.len() as u8;
        at += 1;
        let mut k = 0;
        while k < name.len() {
            out[at + k] = name[k];
            k += 1;
        }
        at += name.len();
        let lo = c.rate_min.to_le_bytes();
        let hi = c.rate_max.to_le_bytes();
        let fixed = [
            c.dir as u8,
            c.kind as u8,
            match c.sensor {
                Some(s) => s as u8,
                None => ANY,
            },
            c.formats,
            c.channels,
            lo[0],
            lo[1],
            lo[2],
            lo[3],
            hi[0],
            hi[1],
            hi[2],
            hi[3],
            c.required as u8,
        ];
        let mut k = 0;
        while k < ENTRY_FIXED {
            out[at + k] = fixed[k];
            k += 1;
        }
        at += ENTRY_FIXED;
        i += 1;
    }
    out
}

const fn eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// A parsed `mw.channels` section: up to [`MAX_CHANNELS`] channels in
/// declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Channels<'a> {
    items: [Option<Channel<'a>>; MAX_CHANNELS],
    n: usize,
}

impl<'a> Default for Channels<'a> {
    fn default() -> Self {
        Channels {
            items: [None; MAX_CHANNELS],
            n: 0,
        }
    }
}

impl<'a> Channels<'a> {
    /// No channels (a module without the section).
    pub fn none() -> Self {
        Self::default()
    }

    /// Parses a section body.
    pub fn parse(section: &'a [u8]) -> Result<Self, ChannelError> {
        let (head, mut rest) = section.split_at_checked(2).ok_or(ChannelError::Length)?;
        if head[0] != VERSION {
            return Err(ChannelError::Version);
        }
        let n = usize::from(head[1]);
        if n > MAX_CHANNELS {
            return Err(ChannelError::TooMany);
        }
        let mut out = Channels::default();
        for _ in 0..n {
            let (&len, tail) = rest.split_first().ok_or(ChannelError::Length)?;
            let (name, tail) = tail
                .split_at_checked(usize::from(len))
                .ok_or(ChannelError::Length)?;
            let (fixed, tail) = tail
                .split_at_checked(ENTRY_FIXED)
                .ok_or(ChannelError::Length)?;
            rest = tail;
            if !valid_name(name) {
                return Err(ChannelError::Name);
            }
            // valid_name admits ASCII only.
            let name = core::str::from_utf8(name).map_err(|_| ChannelError::Name)?;
            if out.get(name).is_some() {
                return Err(ChannelError::Duplicate);
            }
            let field = ChannelError::Field;
            let dir = match fixed[0] {
                0 => Direction::In,
                1 => Direction::Out,
                _ => return Err(field),
            };
            let kind = match fixed[1] {
                0 => DataKind::Bytes,
                1 => DataKind::Samples,
                2 => DataKind::Frames,
                _ => return Err(field),
            };
            let sensor = match fixed[2] {
                ANY => None,
                s => Some(SensorKind::from_u8(s).ok_or(field)?),
            };
            let u32_at = |at: usize| {
                u32::from_le_bytes([fixed[at], fixed[at + 1], fixed[at + 2], fixed[at + 3]])
            };
            if fixed[13] & !1 != 0 {
                return Err(field);
            }
            let c = Channel {
                name,
                dir,
                kind,
                sensor,
                formats: fixed[3],
                channels: fixed[4],
                rate_min: u32_at(5),
                rate_max: u32_at(9),
                required: fixed[13] & 1 != 0,
            }
            .validated()?;
            out.items[out.n] = Some(c);
            out.n += 1;
        }
        if !rest.is_empty() {
            return Err(ChannelError::Length);
        }
        Ok(out)
    }

    /// Reads and parses the `mw.channels` section of a module; no
    /// section is no channels.
    pub fn from_module(wasm: &'a [u8]) -> Result<Self, ChannelError> {
        match read_custom_section(wasm, SECTION)? {
            Some(s) => Self::parse(s),
            None => Ok(Self::none()),
        }
    }

    /// Number of channels.
    pub fn len(&self) -> usize {
        self.n
    }

    /// Whether none are declared.
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// The channels in declaration order.
    pub fn iter(&self) -> impl Iterator<Item = &Channel<'a>> + '_ {
        self.items[..self.n].iter().flatten()
    }

    /// The channel `name` and its index.
    pub fn get(&self, name: &str) -> Option<(usize, &Channel<'a>)> {
        self.iter().enumerate().find(|(_, c)| c.name == name)
    }
}

/// Reads an unsigned LEB128 integer at `*at`, advancing it.
pub fn leb_u64(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0u32..64).step_by(7) {
        let byte = *bytes.get(*at)?;
        *at += 1;
        value |= u64::from(byte & 0x7f).checked_shl(shift)?;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// The sections of a module after its 8-byte header: `(id, body)`, in
/// order. Yields `Err(ChannelError::Module)` once and stops when a header is malformed or a
/// section runs past the end. The magic and version are not checked
/// (the module is validated separately).
pub fn sections(wasm: &[u8]) -> impl Iterator<Item = Result<(u8, &[u8]), ChannelError>> + '_ {
    let mut at = 8usize;
    let mut failed = wasm.len() < 8;
    let mut first_failure = failed;
    core::iter::from_fn(move || {
        if first_failure {
            first_failure = false;
            return Some(Err(ChannelError::Module));
        }
        if failed || at >= wasm.len() {
            return None;
        }
        let item = (|| {
            let id = *wasm.get(at)?;
            at += 1;
            let len = usize::try_from(leb_u64(wasm, &mut at)?).ok()?;
            let end = at.checked_add(len).filter(|e| *e <= wasm.len())?;
            let body = wasm.get(at..end)?;
            at = end;
            Some((id, body))
        })();
        failed = item.is_none();
        Some(item.ok_or(ChannelError::Module))
    })
}

/// The body of the custom section `name` (after its name), `Ok(None)`
/// without one, [`ChannelError::Module`] for malformed section headers or
/// the section twice.
pub fn read_custom_section<'w>(
    wasm: &'w [u8],
    name: &str,
) -> Result<Option<&'w [u8]>, ChannelError> {
    let bad = ChannelError::Module;
    let mut found = None;
    for s in sections(wasm) {
        let (id, body) = s?;
        if id != 0 {
            continue;
        }
        let mut at = 0;
        let len = usize::try_from(leb_u64(body, &mut at).ok_or(bad)?).map_err(|_| bad)?;
        let end = at.checked_add(len).ok_or(bad)?;
        let n = body.get(at..end).ok_or(bad)?;
        if n == name.as_bytes() {
            if found.is_some() {
                return Err(bad);
            }
            found = Some(&body[end..]);
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DECLS: &[Channel<'static>] = &[
        Channel::samples("accel", Direction::In, Some(SensorKind::Imu), 25, 100, true)
            .with_channels(3),
        Channel::bytes("cmd", Direction::In, false),
        Channel::bytes("level", Direction::Out, false),
    ];
    const LEN: usize = encoded_len(DECLS);
    static SECTION_BYTES: [u8; LEN] = encode::<LEN>(DECLS);

    /// A module with only custom sections: header, then each section.
    pub(crate) fn module(customs: &[(&str, &[u8])]) -> Vec<u8> {
        let mut m = b"\0asm\x01\0\0\0".to_vec();
        for (name, body) in customs {
            let mut s = vec![name.len() as u8];
            s.extend_from_slice(name.as_bytes());
            s.extend_from_slice(body);
            m.push(0);
            m.push(s.len() as u8);
            m.extend_from_slice(&s);
        }
        m
    }

    #[test]
    fn ch1_declarations_round_trip_through_the_section() {
        let c = Channels::parse(&SECTION_BYTES).unwrap();
        assert_eq!(c.len(), 3);
        assert_eq!(c.iter().copied().collect::<Vec<_>>(), DECLS);
        assert_eq!(c.get("cmd").map(|(i, _)| i), Some(1));
        assert!(c.get("nope").is_none());
        assert_eq!(
            SECTION_BYTES.len(),
            2 + (1 + 5 + 14) + (1 + 3 + 14) + (1 + 5 + 14)
        );
        let m = module(&[("name", b"x"), (SECTION, &SECTION_BYTES)]);
        assert_eq!(Channels::from_module(&m), Ok(c));
        assert_eq!(Channels::from_module(&module(&[])), Ok(Channels::none()));
    }

    #[test]
    fn ch1_malformed_sections_are_refused() {
        let ok = SECTION_BYTES.to_vec();
        for cut in 0..ok.len() {
            assert!(Channels::parse(&ok[..cut]).is_err(), "cut {cut}");
        }
        let mut extra = ok.clone();
        extra.push(0);
        assert_eq!(Channels::parse(&extra), Err(ChannelError::Length));
        let mut v = ok.clone();
        v[0] = 2;
        assert_eq!(Channels::parse(&v), Err(ChannelError::Version));
        let mut v = ok.clone();
        v[1] = 9;
        assert_eq!(Channels::parse(&v), Err(ChannelError::TooMany));
        // "accel" -> "Accel".
        let mut v = ok.clone();
        v[3] = b'A';
        assert_eq!(Channels::parse(&v), Err(ChannelError::Name));
        // "level" -> "accel" (a duplicate).
        let mut v = ok.clone();
        let at = v.len() - 14 - 5;
        v[at..at + 5].copy_from_slice(b"accel");
        assert_eq!(Channels::parse(&v), Err(ChannelError::Duplicate));
        // accel's direction 2, sensor on a bytes channel, reserved flag,
        // reversed rates.
        for (offset, value) in [(0, 2u8), (13, 2)] {
            let mut v = ok.clone();
            v[2 + 1 + 5 + offset] = value;
            assert_eq!(Channels::parse(&v), Err(ChannelError::Field), "{offset}");
        }
        let cmd_sensor = 2 + 20 + 1 + 3 + 2;
        let mut v = ok.clone();
        v[cmd_sensor] = 0;
        assert_eq!(Channels::parse(&v), Err(ChannelError::Field));
        let rates = Channel::samples("x", Direction::In, None, 10, 5, false);
        assert_eq!(rates.validated(), Err(ChannelError::Field));
        // The section twice, and a section running past the end.
        let twice = module(&[(SECTION, &ok), (SECTION, &ok)]);
        assert_eq!(Channels::from_module(&twice), Err(ChannelError::Module));
        let mut cut = module(&[(SECTION, &ok)]);
        cut.truncate(cut.len() - 1);
        assert_eq!(Channels::from_module(&cut), Err(ChannelError::Module));
    }

    #[test]
    fn ch1_sample_constraints_decide_what_a_channel_accepts() {
        let accel = DECLS[0];
        assert!(accel.accepts_samples(SensorKind::Imu, SampleFormat::I16, 3, 50));
        assert!(!accel.accepts_samples(SensorKind::Imu, SampleFormat::I16, 6, 50));
        assert!(!accel.accepts_samples(SensorKind::Imu, SampleFormat::I16, 3, 200));
        assert!(!accel.accepts_samples(SensorKind::Microphone, SampleFormat::I16, 3, 50));
        let levels = Channel::samples("a", Direction::In, None, 0, u32::MAX, true)
            .with_formats(1 << SampleFormat::Levels as u8);
        assert!(levels.accepts_samples(SensorKind::Microphone, SampleFormat::Levels, 1, 16_000));
        assert!(!levels.accepts_samples(SensorKind::Microphone, SampleFormat::I16, 1, 16_000));
        assert!(!DECLS[1].accepts_samples(SensorKind::Imu, SampleFormat::I16, 3, 50));
    }

    #[test]
    fn sections_walk_and_stop_on_a_malformed_header() {
        let m = module(&[("a", b"1"), ("b", b"22")]);
        let ids: Vec<_> = sections(&m)
            .map(|s| s.map(|(id, b)| (id, b.len())))
            .collect();
        assert_eq!(ids, [Ok((0, 3)), Ok((0, 4))]);
        assert_eq!(read_custom_section(&m, "b"), Ok(Some(&b"22"[..])));
        assert_eq!(read_custom_section(&m, "c"), Ok(None));
        let mut bad = m.clone();
        bad.push(0);
        bad.push(0x80); // unterminated LEB
        assert_eq!(sections(&bad).last(), Some(Err(ChannelError::Module)));
        assert_eq!(read_custom_section(&bad, "c"), Err(ChannelError::Module));
        assert_eq!(
            sections(b"\0asm").collect::<Vec<_>>(),
            [Err(ChannelError::Module)]
        );
    }
}
