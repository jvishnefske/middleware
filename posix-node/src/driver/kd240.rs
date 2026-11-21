//! The Kria KD240's inputs (PN18): switches on GPIO lines, and whether the
//! programmable logic's check design is loaded. **Input only.**
//!
//! Nothing here drives an output. Every point is [`Flow::In`] and
//! [`Driver::write`] refuses everything, so no payload, bound however, can
//! move the lathe through this driver. Outputs (step generation, the
//! spindle, the turret) belong to the programmable logic or a kernel
//! driver, and come only with a separate, explicitly approved change.
//!
//! The lines come from `MW_POSIX_NODE_KD240_INPUTS`, comma-separated
//! `point=chip:offset[:low]` ([`parse_inputs`]): `x_home=gpiochip2:12:low`
//! is a digital point `x_home` read from line 12 of `/dev/gpiochip2`,
//! active low. Each line is requested as an input through the kernel's
//! character device (uAPI v2) and read every [`READ_PERIOD_US`]; a change
//! is sent as a [`Digital`]. A line that cannot be requested stays in the
//! list, never sends, and is named in [`Kd240Driver::problems`].
//!
//! `pl_present` is on while a UIO device whose name starts with `emco` is
//! present (`/sys/class/uio/uio*/name`): the PL check overlay the lathe's
//! NixOS loads at boot. It is a placeholder for the PL's own status
//! registers, which a later driver will read through that UIO device.

use std::path::{Path, PathBuf};

use middleware::wasm_abi::machine::Digital;

use super::{Driver, DriverError, PointSpec};
#[cfg(doc)]
use middleware::loader::control::Flow;

/// How often lines are read, µs.
pub const READ_PERIOD_US: u64 = 10_000;
/// How often the PL's UIO device is looked for, µs.
pub const PL_PERIOD_US: u64 = 1_000_000;
/// The PL presence point.
pub const PL_PRESENT: &str = "pl_present";

/// One input line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputLine {
    /// The point's name.
    pub point: String,
    /// The chip, `/dev/<chip>`.
    pub chip: String,
    /// The line's offset on the chip.
    pub offset: u32,
    /// Asserted when low.
    pub active_low: bool,
}

impl InputLine {
    /// The chip's device path.
    pub fn chip_path(&self) -> PathBuf {
        if self.chip.starts_with('/') { PathBuf::from(&self.chip) } else { Path::new("/dev").join(&self.chip) }
    }
}

/// Parses `MW_POSIX_NODE_KD240_INPUTS` (module docs). Pure.
pub fn parse_inputs(text: &str) -> Result<Vec<InputLine>, String> {
    let mut out: Vec<InputLine> = Vec::new();
    for item in text.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (point, spec) = item.split_once('=').ok_or_else(|| format!("{item:?}: want point=chip:offset[:low]"))?;
        if !middleware::wasm_abi::channels::valid_name(point.as_bytes()) || point == PL_PRESENT {
            return Err(format!("{point:?}: not a point name (a-z0-9_, at most 24)"));
        }
        if out.iter().any(|l| l.point == point) {
            return Err(format!("{point}: named twice"));
        }
        let parts: Vec<&str> = spec.split(':').collect();
        let (chip, offset, low) = match parts.as_slice() {
            [c, o] => (*c, *o, false),
            [c, o, "low"] => (*c, *o, true),
            [c, o, "high"] => (*c, *o, false),
            _ => return Err(format!("{item:?}: want point=chip:offset[:low]")),
        };
        if chip.is_empty() || chip.contains("..") {
            return Err(format!("{item:?}: bad chip"));
        }
        let offset = offset.parse().map_err(|_| format!("{item:?}: bad line offset"))?;
        out.push(InputLine { point: point.to_owned(), chip: chip.to_owned(), offset, active_low: low });
    }
    Ok(out)
}

/// Reads the lines and the PL's presence: the hardware, or a fake.
pub trait Inputs: Send {
    /// Line `i`'s asserted state; `None` when it cannot be read.
    fn line(&mut self, i: usize) -> Option<bool>;
    /// Whether the PL check design is loaded.
    fn pl_present(&mut self) -> bool;
}

/// The board's lines through `/dev/gpiochipN`.
pub struct GpioInputs {
    requests: Vec<Option<LineRequest>>,
    uio: PathBuf,
}

#[cfg(target_os = "linux")]
type LineRequest = (gpiocdev::Request, u32);
#[cfg(not(target_os = "linux"))]
type LineRequest = ((), u32);

impl GpioInputs {
    /// Requests each line as an input; the ones that fail are `None`, with
    /// why.
    pub fn open(lines: &[InputLine]) -> (Self, Vec<String>) {
        let mut problems = Vec::new();
        let requests = lines
            .iter()
            .map(|l| match request(l) {
                Ok(r) => Some(r),
                Err(e) => {
                    problems.push(format!("{}: {}:{}: {e}", l.point, l.chip_path().display(), l.offset));
                    None
                }
            })
            .collect();
        (GpioInputs { requests, uio: PathBuf::from("/sys/class/uio") }, problems)
    }
}

#[cfg(target_os = "linux")]
fn request(l: &InputLine) -> Result<LineRequest, String> {
    let mut b = gpiocdev::Request::builder();
    b.on_chip(l.chip_path()).with_consumer("mw-posix-node").with_line(l.offset).as_input();
    if l.active_low {
        b.as_active_low();
    }
    b.request().map(|r| (r, l.offset)).map_err(|e| e.to_string())
}

#[cfg(not(target_os = "linux"))]
fn request(_: &InputLine) -> Result<LineRequest, String> {
    Err("GPIO character devices are Linux only".into())
}

impl Inputs for GpioInputs {
    #[cfg(target_os = "linux")]
    fn line(&mut self, i: usize) -> Option<bool> {
        let (req, offset) = self.requests.get(i)?.as_ref()?;
        req.value(*offset).ok().map(|v| v == gpiocdev::line::Value::Active)
    }

    #[cfg(not(target_os = "linux"))]
    fn line(&mut self, _: usize) -> Option<bool> {
        None
    }

    fn pl_present(&mut self) -> bool {
        pl_in(&self.uio)
    }
}

/// Whether `uio` (`/sys/class/uio`) lists a device named `emco…`.
pub fn pl_in(uio: &Path) -> bool {
    std::fs::read_dir(uio)
        .into_iter()
        .flatten()
        .flatten()
        .any(|d| std::fs::read_to_string(d.path().join("name")).is_ok_and(|n| n.trim().starts_with("emco")))
}

/// The driver (module docs).
pub struct Kd240Driver<I: Inputs = GpioInputs> {
    points: Vec<PointSpec>,
    inputs: I,
    problems: Vec<String>,
    sent: Vec<Option<bool>>,
    next_read: u64,
    next_pl: u64,
    pl: bool,
}

impl Kd240Driver<GpioInputs> {
    /// Requests `lines` on the board.
    pub fn open(lines: Vec<InputLine>) -> Self {
        let (inputs, problems) = GpioInputs::open(&lines);
        Kd240Driver::with_inputs(&lines, inputs, problems)
    }
}

impl<I: Inputs> Kd240Driver<I> {
    /// The driver over `inputs` (a fake in tests).
    pub fn with_inputs(lines: &[InputLine], inputs: I, problems: Vec<String>) -> Self {
        let mut points: Vec<PointSpec> = lines.iter().map(|l| PointSpec::switch(&l.point)).collect();
        points.push(PointSpec::switch(PL_PRESENT));
        let n = points.len();
        Kd240Driver { points, inputs, problems, sent: vec![None; n], next_read: 0, next_pl: 0, pl: false }
    }

    /// Lines that could not be requested, and why.
    pub fn problems(&self) -> &[String] {
        &self.problems
    }

    fn read(&mut self, now_us: u64, all: bool) -> Vec<(usize, Vec<u8>)> {
        let lines = self.points.len() - 1;
        if all || now_us >= self.next_pl {
            self.pl = self.inputs.pl_present();
            self.next_pl = now_us + PL_PERIOD_US;
        }
        let mut out = Vec::new();
        if !all && now_us < self.next_read && lines > 0 {
            return out;
        }
        self.next_read = now_us + READ_PERIOD_US;
        for i in 0..=lines {
            let v = if i == lines { Some(self.pl) } else { self.inputs.line(i) };
            let Some(on) = v else { continue };
            if all || self.sent[i] != Some(on) {
                self.sent[i] = Some(on);
                out.push((i, Digital { on, at_us: now_us }.encode().to_vec()));
            }
        }
        out
    }
}

impl<I: Inputs> Driver for Kd240Driver<I> {
    fn name(&self) -> &str {
        "kd240"
    }

    fn points(&self) -> &[PointSpec] {
        &self.points
    }

    /// Always refused: this driver has no outputs.
    fn write(&mut self, _: usize, _: &[u8], _: u64) -> Result<(), DriverError> {
        Err(DriverError::NotWritable)
    }

    fn poll(&mut self, now_us: u64) -> Vec<(usize, Vec<u8>)> {
        self.read(now_us, false)
    }

    fn snapshot(&mut self, now_us: u64) -> Vec<(usize, Vec<u8>)> {
        self.read(now_us, true)
    }

    fn next_wake_us(&self, _: u64) -> Option<u64> {
        let lines = self.points.len() > 1;
        Some(if lines { self.next_read.min(self.next_pl) } else { self.next_pl })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::loader::control::Flow;

    struct Fake {
        lines: Vec<Option<bool>>,
        pl: bool,
    }

    impl Inputs for Fake {
        fn line(&mut self, i: usize) -> Option<bool> {
            self.lines[i]
        }
        fn pl_present(&mut self) -> bool {
            self.pl
        }
    }

    #[test]
    fn pn18_inputs_parse() {
        let l = parse_inputs("x_home=gpiochip2:12:low, estop=gpiochip0:5").unwrap();
        assert_eq!(
            l,
            [
                InputLine { point: "x_home".into(), chip: "gpiochip2".into(), offset: 12, active_low: true },
                InputLine { point: "estop".into(), chip: "gpiochip0".into(), offset: 5, active_low: false },
            ]
        );
        assert_eq!(l[0].chip_path(), PathBuf::from("/dev/gpiochip2"));
        assert_eq!(parse_inputs("").unwrap(), []);
        for bad in [
            "x",
            "X=gpiochip0:1",
            "a=gpiochip0",
            "a=gpiochip0:x",
            "a=../x:1",
            "a=c:1,a=c:2",
            "a=c:1:sideways",
            "pl_present=c:1",
        ] {
            assert!(parse_inputs(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn pn18_it_reads_and_never_writes() {
        let lines = parse_inputs("x_home=gpiochip2:12:low,estop=gpiochip0:5").unwrap();
        let fake = Fake { lines: vec![Some(false), Some(true)], pl: true };
        let mut d = Kd240Driver::with_inputs(&lines, fake, Vec::new());
        let names: Vec<&str> = d.points().iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["x_home", "estop", "pl_present"]);
        assert!(d.points().iter().all(|p| p.flow == Flow::In), "inputs only");
        for i in 0..3 {
            assert_eq!(d.write(i, &Digital { on: true, at_us: 0 }.encode(), 0), Err(DriverError::NotWritable));
        }
        let snap = d.snapshot(0);
        assert_eq!(snap.len(), 3);
        assert_eq!(Digital::decode(&snap[2].1).map(|x| x.on), Some(true));
        assert!(d.poll(READ_PERIOD_US).is_empty(), "no change");
        d.inputs.lines[0] = Some(true);
        assert!(d.poll(READ_PERIOD_US + 1).is_empty(), "not yet due");
        let c = d.poll(2 * READ_PERIOD_US);
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].0, Digital::decode(&c[0].1).map(|x| x.on)), (0, Some(true)));
        // An unreadable line sends nothing.
        d.inputs.lines[1] = None;
        assert!(d.poll(3 * READ_PERIOD_US).is_empty());
        assert_eq!(d.next_wake_us(0), Some(4 * READ_PERIOD_US));
    }

    #[test]
    fn pn18_without_lines_it_only_watches_the_pl() {
        let mut d = Kd240Driver::with_inputs(&[], Fake { lines: vec![], pl: false }, Vec::new());
        assert_eq!(d.snapshot(0).len(), 1);
        d.inputs.pl = true;
        assert!(d.poll(10).is_empty());
        assert_eq!(d.poll(PL_PERIOD_US).len(), 1);
        assert_eq!(d.next_wake_us(PL_PERIOD_US), Some(2 * PL_PERIOD_US));
    }

    #[test]
    fn pn18_the_pl_is_found_by_its_uio_name() {
        let dir = std::env::temp_dir().join(format!("mw-uio-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("uio0")).unwrap();
        std::fs::write(dir.join("uio0/name"), "axi-other\n").unwrap();
        assert!(!pl_in(&dir));
        std::fs::create_dir_all(dir.join("uio1")).unwrap();
        std::fs::write(dir.join("uio1/name"), "emco-pl-check\n").unwrap();
        assert!(pl_in(&dir));
        assert!(!pl_in(&dir.join("missing")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
