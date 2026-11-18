//! A node's control-module points: what its native drivers expose to a
//! machine payload (ABI 8, CM1–CM4).
//!
//! A point is one signal of one piece of equipment (the X axis' position,
//! a limit switch, the spindle's speed setpoint, the motion queue), with
//! the channel kind it carries ([`DataKind`], one of the machine kinds),
//! which way it flows, its unit and range. A payload's machine channels
//! bind to points by name ([`super::binding::Target::Driver`]); a tool
//! reads the list to see what a node can do and to generate a physical
//! model for it (autoprobe).
//!
//! The manifest says a node has points and how many pages they take
//! ([`super::manifest::tag::CONTROL_MODULES`]); the points themselves are
//! paged under `sys/manifest/cm` ([`super::keys::CONTROL_MODULES`]), so a
//! long list never crowds the manifest's 384 bytes.
//!
//! # `sys/manifest/cm`
//!
//! Request body: empty (page 0) or `[page u8]`. Reply, one page:
//! `[version u8 = 1][pages u8][page u8][n u8]`, then `n` entries
//!
//! | bytes | field |
//! |-------|-------|
//! | 1 + len | name: length `1..=24`, `a-z0-9_` (channel-name rules) |
//! | 1 | [`DataKind`] code, 3..=6 |
//! | 1 | [`Flow`]: 0 in (the payload reads it), 1 out (the payload writes it), 2 both (a segment queue: segments out, its state in) |
//! | 1 | [`Unit`] |
//! | 4 | lowest value, i32 LE |
//! | 4 | highest value, i32 LE, `>=` the lowest |
//! | 4 | scale, u32 LE: counts per mm for a position point, 0 otherwise |
//!
//! Pages are filled in order up to [`PAGE_MAX`] bytes each; a page beyond
//! the last is `NotFound`. A node has at most [`MAX_POINTS`] points.

use crate::wasm_abi::channels::{DataKind, Direction, MAX_NAME, valid_name};

/// The page format version.
pub const VERSION: u8 = 1;

/// Most points a node exposes.
pub const MAX_POINTS: usize = 64;

/// Largest page: fits one reply (512 bytes) with room.
pub const PAGE_MAX: usize = 480;

/// Bytes of an entry after its name.
pub const ENTRY_FIXED: usize = 15;

/// Bytes before the first entry of a page.
pub const PAGE_HEADER: usize = 4;

/// Which way a point's data flows, seen from the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// The payload reads it (a sensor: a switch, a position).
    In = 0,
    /// The payload writes it (an actuator: a setpoint).
    Out = 1,
    /// Both: a segment queue takes segments and reports its state.
    Both = 2,
}

impl Flow {
    /// Whether a channel of direction `dir` may bind to it.
    pub const fn admits(self, dir: Direction) -> bool {
        matches!(
            (self, dir),
            (Flow::Both, _) | (Flow::In, Direction::In) | (Flow::Out, Direction::Out)
        )
    }

    const fn from_u8(b: u8) -> Option<Flow> {
        match b {
            0 => Some(Flow::In),
            1 => Some(Flow::Out),
            2 => Some(Flow::Both),
            _ => None,
        }
    }

    /// Lower-case name.
    pub const fn name(self) -> &'static str {
        match self {
            Flow::In => "in",
            Flow::Out => "out",
            Flow::Both => "both",
        }
    }
}

/// A point's unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// None (a queue).
    None = 0,
    /// On or off.
    Bool = 1,
    /// Micrometres.
    Micrometre = 2,
    /// Encoder or step counts ([`Point::scale`] per mm).
    Count = 3,
    /// Revolutions per minute.
    Rpm = 4,
    /// A turret or tool-changer station number.
    Station = 5,
    /// Micrometres per second.
    MicrometrePerSecond = 6,
}

impl Unit {
    const fn from_u8(b: u8) -> Option<Unit> {
        Some(match b {
            0 => Unit::None,
            1 => Unit::Bool,
            2 => Unit::Micrometre,
            3 => Unit::Count,
            4 => Unit::Rpm,
            5 => Unit::Station,
            6 => Unit::MicrometrePerSecond,
            _ => return None,
        })
    }

    /// Short name, as tools print it.
    pub const fn name(self) -> &'static str {
        match self {
            Unit::None => "none",
            Unit::Bool => "bool",
            Unit::Micrometre => "um",
            Unit::Count => "count",
            Unit::Rpm => "rpm",
            Unit::Station => "station",
            Unit::MicrometrePerSecond => "um/s",
        }
    }
}

/// One control-module point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point<'a> {
    /// The name a binding names (channel-name rules).
    pub name: &'a str,
    /// What it carries: a machine kind.
    pub kind: DataKind,
    /// Which way.
    pub flow: Flow,
    /// Its unit.
    pub unit: Unit,
    /// Lowest value.
    pub min: i32,
    /// Highest value.
    pub max: i32,
    /// Counts per mm for a position point; 0 otherwise.
    pub scale: u32,
}

/// Why a page or a point was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlError {
    /// Ends inside an entry or bytes follow the last one.
    Length,
    /// Another format version.
    Version,
    /// A bad name, kind, flow, unit or range.
    Field,
    /// Too many points.
    TooMany,
}

impl Point<'_> {
    /// Checks the field rules (name, a machine kind, an ordered range).
    pub const fn check(&self) -> Result<(), ControlError> {
        if !valid_name(self.name.as_bytes()) || !self.kind.is_machine() || self.min > self.max {
            return Err(ControlError::Field);
        }
        Ok(())
    }

    /// Encoded length.
    pub const fn encoded_len(&self) -> usize {
        1 + self.name.len() + ENTRY_FIXED
    }

    fn write(&self, out: &mut [u8]) -> usize {
        let n = self.name.len();
        out[0] = n as u8;
        out[1..1 + n].copy_from_slice(self.name.as_bytes());
        let f = &mut out[1 + n..1 + n + ENTRY_FIXED];
        f[0] = self.kind as u8;
        f[1] = self.flow as u8;
        f[2] = self.unit as u8;
        f[3..7].copy_from_slice(&self.min.to_le_bytes());
        f[7..11].copy_from_slice(&self.max.to_le_bytes());
        f[11..15].copy_from_slice(&self.scale.to_le_bytes());
        1 + n + ENTRY_FIXED
    }
}

/// The page each point falls on, filling pages in order up to
/// [`PAGE_MAX`]: `(page, index)` pairs are in point order.
fn page_of<'p>(points: &'p [Point<'_>]) -> impl Iterator<Item = u8> + 'p {
    let mut page = 0u8;
    let mut used = PAGE_HEADER;
    points.iter().map(move |p| {
        let len = p.encoded_len();
        if used + len > PAGE_MAX {
            page += 1;
            used = PAGE_HEADER;
        }
        used += len;
        page
    })
}

/// How many pages `points` take (0 for none).
pub fn pages(points: &[Point<'_>]) -> u8 {
    page_of(points).last().map_or(0, |p| p + 1)
}

/// Writes page `page` of `points` into `out`; its length, or `None` for a
/// page beyond the last, `out` too short, or an invalid point.
pub fn encode_page(points: &[Point<'_>], page: u8, out: &mut [u8]) -> Option<usize> {
    if points.len() > MAX_POINTS || points.iter().any(|p| p.check().is_err()) {
        return None;
    }
    let total = pages(points);
    if page >= total {
        return None;
    }
    let mut at = PAGE_HEADER;
    let mut n = 0u8;
    for (p, on) in points.iter().zip(page_of(points)) {
        if on == page {
            at += p.write(out.get_mut(at..at + p.encoded_len())?);
            n += 1;
        }
    }
    out.get_mut(..PAGE_HEADER)?.copy_from_slice(&[VERSION, total, page, n]);
    Some(at)
}

/// The page a request asks for: empty is page 0.
pub fn requested_page(body: &[u8]) -> Option<u8> {
    match body {
        [] => Some(0),
        [p] => Some(*p),
        _ => None,
    }
}

/// A decoded page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page<'a> {
    /// Pages in all.
    pub pages: u8,
    /// This page.
    pub page: u8,
    n: u8,
    entries: &'a [u8],
}

impl<'a> Page<'a> {
    /// Decodes and checks a page.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, ControlError> {
        let (head, entries) = bytes.split_at_checked(PAGE_HEADER).ok_or(ControlError::Length)?;
        if head[0] != VERSION {
            return Err(ControlError::Version);
        }
        if head[2] >= head[1] {
            return Err(ControlError::Field);
        }
        let page = Page { pages: head[1], page: head[2], n: head[3], entries };
        let mut count = 0usize;
        let mut rest = entries;
        while !rest.is_empty() {
            let (p, tail) = read_point(rest)?;
            p.check()?;
            rest = tail;
            count += 1;
        }
        if count != usize::from(page.n) {
            return Err(ControlError::Length);
        }
        if count > MAX_POINTS {
            return Err(ControlError::TooMany);
        }
        Ok(page)
    }

    /// The page's points, in order.
    pub fn points(&self) -> impl Iterator<Item = Point<'a>> + 'a {
        let mut rest = self.entries;
        core::iter::from_fn(move || {
            let (p, tail) = read_point(rest).ok()?;
            rest = tail;
            Some(p)
        })
    }
}

fn read_point(b: &[u8]) -> Result<(Point<'_>, &[u8]), ControlError> {
    let (&nl, tail) = b.split_first().ok_or(ControlError::Length)?;
    let nl = usize::from(nl);
    if nl == 0 || nl > MAX_NAME {
        return Err(ControlError::Field);
    }
    let (name, tail) = tail.split_at_checked(nl).ok_or(ControlError::Length)?;
    let (f, tail) = tail.split_at_checked(ENTRY_FIXED).ok_or(ControlError::Length)?;
    let name = core::str::from_utf8(name).map_err(|_| ControlError::Field)?;
    let i32_at = |at: usize| i32::from_le_bytes([f[at], f[at + 1], f[at + 2], f[at + 3]]);
    let p = Point {
        name,
        kind: DataKind::from_u8(f[0]).ok_or(ControlError::Field)?,
        flow: Flow::from_u8(f[1]).ok_or(ControlError::Field)?,
        unit: Unit::from_u8(f[2]).ok_or(ControlError::Field)?,
        min: i32_at(3),
        max: i32_at(7),
        scale: u32::from_le_bytes([f[11], f[12], f[13], f[14]]),
    };
    Ok((p, tail))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: Point<'static> = Point {
        name: "x_limit_min",
        kind: DataKind::Digital,
        flow: Flow::In,
        unit: Unit::Bool,
        min: 0,
        max: 1,
        scale: 0,
    };

    fn lathe() -> [Point<'static>; 4] {
        [
            Point { name: "x_pos", kind: DataKind::Position, flow: Flow::In, unit: Unit::Count, min: -60_000, max: 1_000, scale: 200 },
            LIMIT,
            Point { name: "spindle_rpm", kind: DataKind::Setpoint, flow: Flow::Out, unit: Unit::Rpm, min: 0, max: 4000, scale: 0 },
            Point { name: "motion", kind: DataKind::Segments, flow: Flow::Both, unit: Unit::None, min: 0, max: 32, scale: 0 },
        ]
    }

    #[test]
    fn cm1_a_page_round_trips() {
        let pts = lathe();
        assert_eq!(pages(&pts), 1);
        let mut out = [0u8; PAGE_MAX];
        let n = encode_page(&pts, 0, &mut out).unwrap();
        assert_eq!(&out[..4], &[VERSION, 1, 0, 4]);
        let page = Page::decode(&out[..n]).unwrap();
        assert!(page.points().eq(pts.iter().copied()));
        assert_eq!(encode_page(&pts, 1, &mut out), None, "no such page");
        assert_eq!(pages(&[]), 0);
        assert_eq!(encode_page(&[], 0, &mut out), None);
        assert_eq!(requested_page(&[]), Some(0));
        assert_eq!(requested_page(&[3]), Some(3));
        assert_eq!(requested_page(&[1, 2]), None);
    }

    #[test]
    fn cm1_long_lists_page_and_every_page_fits_a_reply() {
        let names: Vec<String> = (0..MAX_POINTS).map(|i| format!("point_with_long_name_{i:02}")).collect();
        let pts: Vec<Point<'_>> = names.iter().map(|n| Point { name: n, ..LIMIT }).collect();
        let total = pages(&pts);
        assert!(total > 1);
        let mut seen = Vec::new();
        for pg in 0..total {
            let mut out = [0u8; PAGE_MAX];
            let n = encode_page(&pts, pg, &mut out).unwrap();
            assert!(n <= PAGE_MAX && PAGE_MAX < crate::wasm_abi::MAX_MESSAGE);
            let page = Page::decode(&out[..n]).unwrap();
            assert_eq!((page.pages, page.page), (total, pg));
            seen.extend(page.points().map(|p| p.name.to_string()));
        }
        assert_eq!(seen, names, "every point once, in order");
        let too_many: Vec<Point<'_>> = core::iter::repeat_n(LIMIT, MAX_POINTS + 1).collect();
        assert_eq!(encode_page(&too_many, 0, &mut [0u8; PAGE_MAX]), None);
    }

    #[test]
    fn cm1_bad_points_and_pages_are_refused() {
        let mut out = [0u8; PAGE_MAX];
        for bad in [
            Point { name: "X", ..LIMIT },
            Point { kind: DataKind::Bytes, ..LIMIT },
            Point { min: 2, max: 1, ..LIMIT },
        ] {
            assert_eq!(bad.check(), Err(ControlError::Field));
            assert_eq!(encode_page(&[bad], 0, &mut out), None);
        }
        let n = encode_page(&lathe(), 0, &mut out).unwrap();
        let good = out[..n].to_vec();
        assert_eq!(Page::decode(&good[..n - 1]), Err(ControlError::Length));
        let mut v = good.clone();
        v[0] = 2;
        assert_eq!(Page::decode(&v), Err(ControlError::Version));
        let mut v = good.clone();
        v[3] = 5;
        assert_eq!(Page::decode(&v), Err(ControlError::Length), "count disagrees");
        let mut v = good.clone();
        v[2] = 1;
        assert_eq!(Page::decode(&v), Err(ControlError::Field), "page past the total");
        let mut v = good;
        let kind_at = PAGE_HEADER + 1 + "x_pos".len();
        v[kind_at] = 1;
        assert_eq!(Page::decode(&v), Err(ControlError::Field), "a sample kind is not a point");
    }

    #[test]
    fn cm2_flows_admit_directions() {
        assert!(Flow::In.admits(Direction::In) && !Flow::In.admits(Direction::Out));
        assert!(Flow::Out.admits(Direction::Out) && !Flow::Out.admits(Direction::In));
        assert!(Flow::Both.admits(Direction::In) && Flow::Both.admits(Direction::Out));
    }
}
