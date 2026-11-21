//! Native drivers: what a node's control-module points are wired to
//! (ABI 8, design.md PN16–PN19).
//!
//! A machine payload never touches hardware. It declares machine channels
//! ([`middleware::wasm_abi::channels::DataKind::Digital`] and after) and a
//! binding names the points they connect to
//! ([`middleware::loader::binding::Target::Driver`]). The point list and the
//! hardware behind it belong to a [`Driver`] here, in the node:
//!
//! * [`sim`]: a two-axis turning machine in software (axes that stall at
//!   their mechanical stops when it has no limit switches, reference
//!   switches, a spindle with acceleration, a turret, the e-stop chain,
//!   and a segment queue). The same payload runs against it as against
//!   hardware.
//! * [`kd240`]: the Kria KD240's inputs, read-only: switches on GPIO lines
//!   (`/dev/gpiochipN`) and whether the programmable logic's check design
//!   is loaded. **It has no outputs**: nothing a payload sends reaches the
//!   machine.
//! * [`NoDriver`]: no points (the default).
//!
//! The controller ([`crate::controller`]) passes what the payload publishes
//! on `drv/<point>` to [`Driver::write`], delivers what [`Driver::poll`]
//! reads to the payload on `drv/<point>`, and serves the point list on
//! `sys/manifest/cm` ([`serve_page`]). Before a payload starts,
//! [`check_binding`] makes sure every driver target names a point of the
//! right kind and direction. A driver's timing is its own: positions and
//! motion are integrated in [`Driver::poll`] from the clock, so a late poll
//! moves further in one step but never changes a segment's path. Step
//! generation for a real machine is not here and never will be: it stays
//! in the programmable logic or the kernel.

pub mod kd240;
pub mod sim;

use middleware::loader::binding::{Binding, Target};
use middleware::loader::control::{self, Flow, Point, Unit};
use middleware::loader::manifest::ControlModules;
use middleware::transport::HandlerError;
use middleware::wasm_abi::channels::DataKind;
use wasm_host::channels::Decl;

/// One point a driver exposes (an owned [`Point`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointSpec {
    /// Its name (channel-name rules).
    pub name: String,
    /// A machine kind.
    pub kind: DataKind,
    /// Which way, seen from the payload.
    pub flow: Flow,
    /// Its unit.
    pub unit: Unit,
    /// Lowest value.
    pub min: i32,
    /// Highest value.
    pub max: i32,
    /// Counts per mm for a position point.
    pub scale: u32,
}

impl PointSpec {
    /// A point without a scale.
    pub fn new(name: &str, kind: DataKind, flow: Flow, unit: Unit, min: i32, max: i32) -> Self {
        PointSpec { name: name.to_owned(), kind, flow, unit, min, max, scale: 0 }
    }

    /// A digital input.
    pub fn switch(name: &str) -> Self {
        PointSpec::new(name, DataKind::Digital, Flow::In, Unit::Bool, 0, 1)
    }

    /// With counts per mm.
    pub fn scaled(self, scale: u32) -> Self {
        PointSpec { scale, ..self }
    }

    /// As the wire type.
    pub fn point(&self) -> Point<'_> {
        Point {
            name: &self.name,
            kind: self.kind,
            flow: self.flow,
            unit: self.unit,
            min: self.min,
            max: self.max,
            scale: self.scale,
        }
    }
}

/// Why a driver refused a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverError {
    /// The point is not written by payloads (an input, or no outputs at
    /// all, as on the KD240).
    NotWritable,
    /// The message is not one of the point's kind.
    Malformed,
    /// The value is outside the point's range.
    OutOfRange,
}

/// A node's native driver (module docs).
pub trait Driver: Send {
    /// A short name for logs (`sim`, `kd240`).
    fn name(&self) -> &str;
    /// The points, in a fixed order; indices below are into it.
    fn points(&self) -> &[PointSpec];
    /// The payload wrote `data` to point `index` at `now_us`.
    fn write(&mut self, index: usize, data: &[u8], now_us: u64) -> Result<(), DriverError>;
    /// Advances to `now_us` and returns what changed, as `(index, message)`.
    fn poll(&mut self, now_us: u64) -> Vec<(usize, Vec<u8>)>;
    /// Every input's current value (for a payload that just started).
    fn snapshot(&mut self, now_us: u64) -> Vec<(usize, Vec<u8>)>;
    /// When [`Driver::poll`] next has something to do; `None`: only on a
    /// write.
    fn next_wake_us(&self, now_us: u64) -> Option<u64>;
}

impl core::fmt::Debug for dyn Driver {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Driver({}, {} points)", self.name(), self.points().len())
    }
}

/// A node without control modules.
#[derive(Debug, Default)]
pub struct NoDriver;

impl Driver for NoDriver {
    fn name(&self) -> &str {
        "none"
    }
    fn points(&self) -> &[PointSpec] {
        &[]
    }
    fn write(&mut self, _: usize, _: &[u8], _: u64) -> Result<(), DriverError> {
        Err(DriverError::NotWritable)
    }
    fn poll(&mut self, _: u64) -> Vec<(usize, Vec<u8>)> {
        Vec::new()
    }
    fn snapshot(&mut self, _: u64) -> Vec<(usize, Vec<u8>)> {
        Vec::new()
    }
    fn next_wake_us(&self, _: u64) -> Option<u64> {
        None
    }
}

/// The index of the point named `name`.
pub fn index_of(points: &[PointSpec], name: &str) -> Option<usize> {
    points.iter().position(|p| p.name == name)
}

fn wire(points: &[PointSpec]) -> Vec<Point<'_>> {
    points.iter().map(PointSpec::point).collect()
}

/// The manifest's record for `points`; `None` without any (or when they
/// would not encode).
pub fn control_summary(points: &[PointSpec]) -> Option<ControlModules> {
    let w = wire(points);
    let pages = control::pages(&w);
    (pages > 0 && w.len() <= control::MAX_POINTS && w.iter().all(|p| p.check().is_ok()))
        .then_some(ControlModules { points: w.len() as u8, pages })
}

/// `sys/manifest/cm`: the page `body` asks for, into `out`. `NotFound`
/// past the last page (or with no points), `InvalidRequest` for a body
/// that is not one page number.
pub fn serve_page(points: &[PointSpec], body: &[u8], out: &mut [u8]) -> Result<usize, HandlerError> {
    let page = control::requested_page(body).ok_or(HandlerError::InvalidRequest)?;
    control::encode_page(&wire(points), page, out).ok_or(HandlerError::NotFound)
}

/// Checks the driver targets of `binding` against `points`: each names a
/// point, of the channel's kind, flowing the channel's way. `Err` names the
/// channel and says why.
pub fn check_binding(decls: &[Decl], binding: Option<&Binding<'_>>, points: &[PointSpec]) -> Result<(), String> {
    let Some(binding) = binding else { return Ok(()) };
    for d in decls {
        let Some(Target::Driver(name)) = binding.target(&d.name) else { continue };
        let Some(p) = points.iter().find(|p| p.name == name) else {
            return Err(format!("channel {}: this node has no point {name}", d.name));
        };
        if p.kind != d.kind {
            return Err(format!(
                "channel {}: point {name} carries {}, the channel {}",
                d.name,
                p.kind.name(),
                d.kind.name()
            ));
        }
        if !p.flow.admits(d.dir) {
            return Err(format!("channel {}: point {name} is {}, the channel {:?}", d.name, p.flow.name(), d.dir));
        }
    }
    Ok(())
}

/// Which driver a node runs (`MW_POSIX_NODE_DRIVER`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DriverChoice {
    /// [`NoDriver`].
    #[default]
    None,
    /// [`sim::SimDriver`] with the EMCO PC Turn 50's numbers.
    Sim,
    /// [`kd240::Kd240Driver`], with its input lines
    /// (`MW_POSIX_NODE_KD240_INPUTS`, [`kd240::parse_inputs`]).
    Kd240(Vec<kd240::InputLine>),
}

impl DriverChoice {
    /// The driver.
    pub fn open(&self) -> Box<dyn Driver> {
        match self {
            DriverChoice::None => Box::new(NoDriver),
            DriverChoice::Sim => Box::new(sim::SimDriver::new(sim::SimConfig::pc_turn_50())),
            DriverChoice::Kd240(lines) => Box::new(kd240::Kd240Driver::open(lines.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::loader::trust::Version;
    use middleware::wasm_abi::channels::{Channel, Direction};

    fn decls() -> Vec<Decl> {
        [
            Channel::machine("limit", Direction::In, DataKind::Digital, true),
            Channel::machine("motion", Direction::Out, DataKind::Segments, true),
            Channel::machine("queue", Direction::In, DataKind::Segments, true),
        ]
        .iter()
        .map(Decl::from_channel)
        .collect()
    }

    fn points() -> Vec<PointSpec> {
        vec![
            PointSpec::switch("x_home"),
            PointSpec::new("spindle_rpm", DataKind::Setpoint, Flow::Out, Unit::Rpm, 0, 3000),
            PointSpec::new("motion", DataKind::Segments, Flow::Both, Unit::None, 0, 32),
        ]
    }

    #[test]
    fn pn16_driver_targets_are_checked_against_the_points() {
        let pts = points();
        let bind = |limit, motion| {
            let mut b = Binding::new([0; 32], Version(1));
            b.bind("limit", Target::Driver(limit)).unwrap();
            b.bind("motion", Target::Driver(motion)).unwrap();
            b.bind("queue", Target::Driver("motion")).unwrap();
            b
        };
        assert_eq!(check_binding(&decls(), Some(&bind("x_home", "motion")), &pts), Ok(()));
        assert_eq!(check_binding(&decls(), None, &pts), Ok(()));
        let e = check_binding(&decls(), Some(&bind("z_home", "motion")), &pts).unwrap_err();
        assert!(e.contains("no point z_home"), "{e}");
        let e = check_binding(&decls(), Some(&bind("spindle_rpm", "motion")), &pts).unwrap_err();
        assert!(e.contains("carries setpoint"), "{e}");
        // A digital output channel on a digital input point.
        let out = [Decl::from_channel(&Channel::machine("lamp", Direction::Out, DataKind::Digital, true))];
        let mut b = Binding::new([0; 32], Version(1));
        b.bind("lamp", Target::Driver("x_home")).unwrap();
        let e = check_binding(&out, Some(&b), &pts).unwrap_err();
        assert!(e.contains("is in"), "{e}");
    }

    #[test]
    fn pn16_the_points_are_served_in_pages() {
        let pts = points();
        assert_eq!(control_summary(&pts), Some(ControlModules { points: 3, pages: 1 }));
        assert_eq!(control_summary(&[]), None);
        let mut out = [0u8; control::PAGE_MAX];
        let n = serve_page(&pts, &[], &mut out).unwrap();
        let page = control::Page::decode(&out[..n]).unwrap();
        assert_eq!(page.points().map(|p| p.name).collect::<Vec<_>>(), ["x_home", "spindle_rpm", "motion"]);
        assert_eq!(serve_page(&pts, &[1], &mut out), Err(HandlerError::NotFound));
        assert_eq!(serve_page(&pts, &[0, 0], &mut out), Err(HandlerError::InvalidRequest));
        assert_eq!(serve_page(&[], &[], &mut out), Err(HandlerError::NotFound));
    }
}
