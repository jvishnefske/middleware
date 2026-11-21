//! A two-axis turning machine in software (PN17): what a physical-model
//! payload drives when there is no machine.
//!
//! | point | kind | flow | |
//! |-------|------|------|-|
//! | `x_pos`, `z_pos` | position | in | step counts ([`AxisSim::counts_per_mm`] per mm) |
//! | `x_home`, `z_home` | digital | in | the reference switch, made within [`HOME_ZONE_UM`] of the axis' home end |
//! | `x_limit_min`, … `z_limit_max` | digital | in | only with [`SimConfig::limits`] |
//! | `spindle_rpm` | setpoint | out | commanded speed, signed (negative: reverse) |
//! | `spindle_actual` | setpoint | in | measured speed, ramping at [`SimConfig::spindle_accel_rpm_s`] |
//! | `turret_station` | setpoint | out | 1..=[`SimConfig::turret_stations`] |
//! | `turret_ready` | digital | in | off while indexing |
//! | `estop` | digital | in | on while the chain is closed (machine may move) |
//! | `motion` | segments | both | the segment queue and its [`QueueState`] |
//!
//! The position a payload sees is the **step count**, what an open-loop
//! stepper controller knows; the simulator also keeps the true position.
//! They agree until an axis is driven past its mechanical stop: on a
//! machine without limit switches (the EMCO PC Turn 50) the stepper then
//! stalls, the true position stays at the stop and the count runs on, so
//! the machine has lost its position, exactly as the real one does (only a
//! new reference finds it again). With [`SimConfig::limits`] the limit
//! switch trips first instead and motion stops cleanly.
//!
//! Motion stops ([`QueueState::STOPPED`], queue emptied, spindle to 0) when
//! the e-stop opens or a limit trips; it stays stopped until the e-stop is
//! closed again and the payload sends a [`Segment::FLUSH`] to acknowledge.

use middleware::loader::control::{Flow, Unit};
use middleware::wasm_abi::channels::DataKind;
use middleware::wasm_abi::machine::{Digital, Position, QueueState, Segment, Setpoint};
use std::collections::VecDeque;

use super::{Driver, DriverError, PointSpec};

/// The reference switch is made this close to the home end, µm.
pub const HOME_ZONE_UM: i32 = 200;
/// Mechanical stops sit this far past the travel ends, µm.
pub const STOP_MARGIN_UM: i32 = 2_000;
/// Positions are sent at most this often while an axis moves, µs.
pub const POSITION_PERIOD_US: u64 = 10_000;
/// The simulator steps at least this often while anything moves, µs.
pub const STEP_US: u64 = 5_000;

/// One axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AxisSim {
    /// Travel, µm.
    pub min_um: i32,
    /// Travel, µm.
    pub max_um: i32,
    /// Home end: the reference switch is at the maximum (true) or minimum.
    pub home_at_max: bool,
    /// Where the axis is at power-up (unknown to the payload), µm.
    pub start_um: i32,
    /// Step counts per mm.
    pub counts_per_mm: u32,
    /// Rapid rate, µm/s.
    pub rapid_um_s: u32,
}

/// The simulated machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimConfig {
    /// X (radius) and Z.
    pub x: AxisSim,
    /// Z.
    pub z: AxisSim,
    /// End-of-travel limit switches present.
    pub limits: bool,
    /// Highest spindle speed, rpm.
    pub spindle_max_rpm: i32,
    /// Spindle acceleration, rpm/s.
    pub spindle_accel_rpm_s: i32,
    /// Turret stations.
    pub turret_stations: u8,
    /// One index, ms.
    pub turret_index_ms: u32,
    /// Segment queue slots.
    pub queue: u16,
}

impl SimConfig {
    /// The EMCO PC Turn 50 (the lathe repository's `MachineProfile`
    /// defaults): X −0.5..48 mm of radius, Z −228..0.5 mm, 1 µm steps,
    /// 750 mm/min rapids, 3000 rpm, 6 stations, no limit switches.
    pub fn pc_turn_50() -> Self {
        let rapid = 750_000 / 60;
        SimConfig {
            x: AxisSim {
                min_um: -500,
                max_um: 48_000,
                home_at_max: true,
                start_um: 20_000,
                counts_per_mm: 1000,
                rapid_um_s: rapid,
            },
            z: AxisSim {
                min_um: -228_000,
                max_um: 500,
                home_at_max: true,
                start_um: -100_000,
                counts_per_mm: 1000,
                rapid_um_s: rapid,
            },
            limits: false,
            spindle_max_rpm: 3000,
            spindle_accel_rpm_s: 1500,
            turret_stations: 6,
            turret_index_ms: 2000,
            queue: 32,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Axis {
    cfg: AxisSim,
    /// Commanded position (what the step count says), µm.
    cmd: f64,
    /// Where the slide really is, µm.
    real: f64,
    sent_counts: Option<i64>,
    sent_at: u64,
}

impl Axis {
    fn new(cfg: AxisSim) -> Self {
        Axis { cfg, cmd: f64::from(cfg.start_um), real: f64::from(cfg.start_um), sent_counts: None, sent_at: 0 }
    }

    fn counts(&self) -> i64 {
        (self.cmd * f64::from(self.cfg.counts_per_mm) / 1000.0).round() as i64
    }

    /// Moves the commanded position; the slide follows up to its stops.
    fn step_to(&mut self, um: f64) {
        let d = um - self.cmd;
        self.cmd = um;
        let lo = f64::from(self.cfg.min_um - STOP_MARGIN_UM);
        let hi = f64::from(self.cfg.max_um + STOP_MARGIN_UM);
        self.real = (self.real + d).clamp(lo, hi);
    }

    fn home(&self) -> bool {
        let r = self.real.round() as i32;
        if self.cfg.home_at_max { r >= self.cfg.max_um - HOME_ZONE_UM } else { r <= self.cfg.min_um + HOME_ZONE_UM }
    }

    fn limit_min(&self) -> bool {
        self.real.round() as i32 <= self.cfg.min_um
    }

    fn limit_max(&self) -> bool {
        self.real.round() as i32 >= self.cfg.max_um
    }
}

// Point indices, in [`SimDriver::points`] order.
const X_POS: usize = 0;
const Z_POS: usize = 1;
const X_HOME: usize = 2;
const Z_HOME: usize = 3;
const SPINDLE_RPM: usize = 4;
const SPINDLE_ACTUAL: usize = 5;
const TURRET_STATION: usize = 6;
const TURRET_READY: usize = 7;
const ESTOP: usize = 8;
const MOTION: usize = 9;
const X_LIMIT_MIN: usize = 10;

/// The simulator (module docs).
pub struct SimDriver {
    cfg: SimConfig,
    points: Vec<PointSpec>,
    x: Axis,
    z: Axis,
    queue: VecDeque<Segment>,
    done: u32,
    stopped: bool,
    overrun: bool,
    estop_ok: bool,
    spindle_cmd: i32,
    spindle: f64,
    spindle_sent: Option<i32>,
    spindle_sent_at: u64,
    station: u8,
    index_until: Option<u64>,
    last_us: Option<u64>,
    digital_sent: Vec<Option<bool>>,
    queue_sent: Option<QueueState>,
}

impl SimDriver {
    /// A machine at rest, e-stop closed, turret on station 1.
    pub fn new(cfg: SimConfig) -> Self {
        let pos = |name: &str, a: &AxisSim| {
            let scale = a.counts_per_mm;
            let to_counts = |um: i32| (i64::from(um - STOP_MARGIN_UM) * i64::from(scale) / 1000) as i32;
            PointSpec::new(
                name,
                DataKind::Position,
                Flow::In,
                Unit::Count,
                to_counts(a.min_um),
                to_counts(a.max_um + 2 * STOP_MARGIN_UM),
            )
            .scaled(scale)
        };
        let mut points = vec![
            pos("x_pos", &cfg.x),
            pos("z_pos", &cfg.z),
            PointSpec::switch("x_home"),
            PointSpec::switch("z_home"),
            PointSpec::new(
                "spindle_rpm",
                DataKind::Setpoint,
                Flow::Out,
                Unit::Rpm,
                -cfg.spindle_max_rpm,
                cfg.spindle_max_rpm,
            ),
            PointSpec::new(
                "spindle_actual",
                DataKind::Setpoint,
                Flow::In,
                Unit::Rpm,
                -cfg.spindle_max_rpm,
                cfg.spindle_max_rpm,
            ),
            PointSpec::new(
                "turret_station",
                DataKind::Setpoint,
                Flow::Out,
                Unit::Station,
                1,
                i32::from(cfg.turret_stations),
            ),
            PointSpec::switch("turret_ready"),
            PointSpec::switch("estop"),
            PointSpec::new("motion", DataKind::Segments, Flow::Both, Unit::None, 0, i32::from(cfg.queue)),
        ];
        if cfg.limits {
            for n in ["x_limit_min", "x_limit_max", "z_limit_min", "z_limit_max"] {
                points.push(PointSpec::switch(n));
            }
        }
        let n = points.len();
        SimDriver {
            x: Axis::new(cfg.x),
            z: Axis::new(cfg.z),
            cfg,
            points,
            queue: VecDeque::new(),
            done: 0,
            stopped: false,
            overrun: false,
            estop_ok: true,
            spindle_cmd: 0,
            spindle: 0.0,
            spindle_sent: None,
            spindle_sent_at: 0,
            station: 1,
            index_until: None,
            last_us: None,
            digital_sent: vec![None; n],
            queue_sent: None,
        }
    }

    /// The configuration.
    pub fn config(&self) -> &SimConfig {
        &self.cfg
    }

    /// Opens (`false`) or closes the e-stop chain, as the operator does.
    pub fn set_estop(&mut self, closed: bool) {
        self.estop_ok = closed;
        if !closed {
            self.stop();
        }
    }

    /// Where the slides really are, µm (X, Z): not what the step counts
    /// say after a stall.
    pub fn real_um(&self) -> (i32, i32) {
        (self.x.real.round() as i32, self.z.real.round() as i32)
    }

    /// What the step counts say, µm.
    pub fn commanded_um(&self) -> (i32, i32) {
        (self.x.cmd.round() as i32, self.z.cmd.round() as i32)
    }

    /// The spindle's speed now, rpm.
    pub fn spindle_rpm(&self) -> i32 {
        self.spindle.round() as i32
    }

    /// The turret's station (while indexing, the one it goes to).
    pub fn station(&self) -> u8 {
        self.station
    }

    fn stop(&mut self) {
        self.queue.clear();
        self.stopped = true;
        self.spindle_cmd = 0;
    }

    fn moving(&self) -> bool {
        !self.queue.is_empty()
    }

    fn queue_state(&self) -> QueueState {
        let mut flags = 0;
        if self.queue.is_empty() {
            flags |= QueueState::IDLE;
        }
        if self.stopped {
            flags |= QueueState::STOPPED;
        }
        if self.overrun {
            flags |= QueueState::OVERRUN;
        }
        QueueState { free: self.cfg.queue - self.queue.len() as u16, done: self.done, flags }
    }

    fn digital(&self, index: usize) -> bool {
        match index {
            X_HOME => self.x.home(),
            Z_HOME => self.z.home(),
            TURRET_READY => self.index_until.is_none(),
            ESTOP => self.estop_ok,
            i if self.cfg.limits && i >= X_LIMIT_MIN => match i - X_LIMIT_MIN {
                0 => self.x.limit_min(),
                1 => self.x.limit_max(),
                2 => self.z.limit_min(),
                _ => self.z.limit_max(),
            },
            _ => false,
        }
    }

    fn limit_tripped(&self) -> bool {
        self.cfg.limits && (self.x.limit_min() || self.x.limit_max() || self.z.limit_min() || self.z.limit_max())
    }

    /// Runs the queue for `dt` seconds.
    fn run_motion(&mut self, mut dt: f64) {
        while dt > 0.0 {
            let Some(seg) = self.queue.front().copied() else { break };
            let (tx, tz) = (f64::from(seg.x_um), f64::from(seg.z_um));
            let (dx, dz) = (tx - self.x.cmd, tz - self.z.cmd);
            let dist = dx.hypot(dz);
            let speed = if seg.flags & Segment::RAPID != 0 {
                // Each axis at its own rapid rate: the slower one decides.
                let tx_s = dx.abs() / f64::from(self.cfg.x.rapid_um_s.max(1));
                let tz_s = dz.abs() / f64::from(self.cfg.z.rapid_um_s.max(1));
                let t = tx_s.max(tz_s);
                if t > 0.0 { dist / t } else { f64::INFINITY }
            } else if seg.flags & Segment::SYNC != 0 {
                f64::from(seg.feed) * self.spindle.abs() / 60.0
            } else {
                f64::from(seg.feed)
            };
            if dist <= 0.5 {
                self.x.step_to(tx);
                self.z.step_to(tz);
                self.queue.pop_front();
                self.done = seg.seq;
                continue;
            }
            if speed <= 0.0 {
                // A synchronised move with the spindle stopped waits for it.
                break;
            }
            let reach = speed * dt;
            if reach >= dist {
                dt -= dist / speed;
                self.x.step_to(tx);
                self.z.step_to(tz);
                self.queue.pop_front();
                self.done = seg.seq;
            } else {
                let f = reach / dist;
                self.x.step_to(self.x.cmd + dx * f);
                self.z.step_to(self.z.cmd + dz * f);
                dt = 0.0;
            }
            if self.limit_tripped() {
                self.stop();
                break;
            }
        }
    }

    fn advance(&mut self, now_us: u64) {
        let dt_us = self.last_us.map_or(0, |l| now_us.saturating_sub(l));
        self.last_us = Some(now_us);
        // Step in pieces, so limits and switches are seen on the way.
        let mut left = dt_us;
        while left > 0 {
            let step = left.min(STEP_US);
            left -= step;
            let dt = step as f64 / 1e6;
            let target = f64::from(self.spindle_cmd);
            let dv = f64::from(self.cfg.spindle_accel_rpm_s) * dt;
            self.spindle = if (target - self.spindle).abs() <= dv {
                target
            } else {
                self.spindle + dv * (target - self.spindle).signum()
            };
            if !self.stopped {
                self.run_motion(dt);
            }
        }
        if self.index_until.is_some_and(|t| now_us >= t) {
            self.index_until = None;
        }
    }

    fn changes(&mut self, now_us: u64, all: bool) -> Vec<(usize, Vec<u8>)> {
        let mut out = Vec::new();
        for (index, axis) in [(X_POS, &mut self.x), (Z_POS, &mut self.z)] {
            let c = axis.counts();
            let due = now_us.saturating_sub(axis.sent_at) >= POSITION_PERIOD_US;
            if all || (axis.sent_counts != Some(c) && due) {
                axis.sent_counts = Some(c);
                axis.sent_at = now_us;
                out.push((index, Position { counts: c, at_us: now_us }.encode().to_vec()));
            }
        }
        let rpm = self.spindle.round() as i32;
        let settled = rpm == self.spindle_cmd;
        if all
            || (self.spindle_sent != Some(rpm) && (settled || now_us.saturating_sub(self.spindle_sent_at) >= 100_000))
        {
            self.spindle_sent = Some(rpm);
            self.spindle_sent_at = now_us;
            out.push((SPINDLE_ACTUAL, Setpoint { value: rpm }.encode().to_vec()));
        }
        for index in 0..self.points.len() {
            if self.points[index].kind != DataKind::Digital {
                continue;
            }
            let on = self.digital(index);
            if all || self.digital_sent[index] != Some(on) {
                self.digital_sent[index] = Some(on);
                out.push((index, Digital { on, at_us: now_us }.encode().to_vec()));
            }
        }
        let q = self.queue_state();
        if all || self.queue_sent != Some(q) {
            self.queue_sent = Some(q);
            self.overrun = false;
            out.push((MOTION, q.encode().to_vec()));
        }
        out
    }
}

impl Driver for SimDriver {
    fn name(&self) -> &str {
        "sim"
    }

    fn points(&self) -> &[PointSpec] {
        &self.points
    }

    fn write(&mut self, index: usize, data: &[u8], now_us: u64) -> Result<(), DriverError> {
        self.advance(now_us);
        match index {
            MOTION => {
                let seg = Segment::decode(data).ok_or(DriverError::Malformed)?;
                if seg.flags & Segment::FLUSH != 0 {
                    self.queue.clear();
                    // The acknowledgement: motion may resume once the
                    // chain is closed (off a limit, the next segment moves
                    // away from it).
                    if self.estop_ok {
                        self.stopped = false;
                    }
                } else if self.stopped || self.queue.len() >= usize::from(self.cfg.queue) {
                    self.overrun = true;
                } else {
                    self.queue.push_back(seg);
                }
                Ok(())
            }
            SPINDLE_RPM => {
                let v = Setpoint::decode(data).ok_or(DriverError::Malformed)?.value;
                if v.abs() > self.cfg.spindle_max_rpm {
                    return Err(DriverError::OutOfRange);
                }
                if !self.stopped || v == 0 {
                    self.spindle_cmd = v;
                }
                Ok(())
            }
            TURRET_STATION => {
                let v = Setpoint::decode(data).ok_or(DriverError::Malformed)?.value;
                if v < 1 || v > i32::from(self.cfg.turret_stations) {
                    return Err(DriverError::OutOfRange);
                }
                if v as u8 != self.station || self.index_until.is_some() {
                    self.station = v as u8;
                    self.index_until = Some(now_us + u64::from(self.cfg.turret_index_ms) * 1000);
                }
                Ok(())
            }
            _ => Err(DriverError::NotWritable),
        }
    }

    fn poll(&mut self, now_us: u64) -> Vec<(usize, Vec<u8>)> {
        self.advance(now_us);
        self.changes(now_us, false)
    }

    fn snapshot(&mut self, now_us: u64) -> Vec<(usize, Vec<u8>)> {
        self.advance(now_us);
        self.changes(now_us, true)
    }

    fn next_wake_us(&self, now_us: u64) -> Option<u64> {
        let busy = self.moving() || self.spindle.round() as i32 != self.spindle_cmd;
        let index = self.index_until;
        match (busy, index) {
            (true, _) => Some(now_us + STEP_US),
            (false, Some(t)) => Some(t),
            (false, None) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(seq: u32, x: i32, z: i32, feed: u32, flags: u8) -> Vec<u8> {
        Segment { seq, x_um: x, z_um: z, feed, flags }.encode().to_vec()
    }

    fn last<T>(changes: &[(usize, Vec<u8>)], index: usize, decode: fn(&[u8]) -> Option<T>) -> Option<T> {
        changes.iter().rev().find(|(i, _)| *i == index).and_then(|(_, d)| decode(d))
    }

    fn run(sim: &mut SimDriver, from_us: u64, to_us: u64) -> Vec<(usize, Vec<u8>)> {
        let mut out = Vec::new();
        let mut t = from_us;
        while t < to_us {
            t += 1000;
            out.extend(sim.poll(t));
        }
        out
    }

    #[test]
    fn pn17_points_and_snapshot() {
        let mut sim = SimDriver::new(SimConfig::pc_turn_50());
        let names: Vec<&str> = sim.points().iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "x_pos",
                "z_pos",
                "x_home",
                "z_home",
                "spindle_rpm",
                "spindle_actual",
                "turret_station",
                "turret_ready",
                "estop",
                "motion"
            ]
        );
        assert!(sim.points().iter().all(|p| p.point().check().is_ok()));
        let snap = sim.snapshot(0);
        assert_eq!(last(&snap, X_POS, Position::decode).unwrap().counts, 20_000);
        assert_eq!(last(&snap, ESTOP, Digital::decode).map(|d| d.on), Some(true));
        assert_eq!(
            last(&snap, MOTION, QueueState::decode),
            Some(QueueState { free: 32, done: 0, flags: QueueState::IDLE })
        );
        assert!(sim.poll(1000).is_empty(), "nothing changed");
        assert_eq!(sim.next_wake_us(1000), None);
        let limits = SimDriver::new(SimConfig { limits: true, ..SimConfig::pc_turn_50() });
        assert_eq!(limits.points().len(), 14);
    }

    #[test]
    fn pn17_a_feed_move_takes_its_time_and_reports_done() {
        let mut sim = SimDriver::new(SimConfig::pc_turn_50());
        sim.snapshot(0);
        // 10 mm of Z at 1000 µm/s: ten seconds.
        sim.write(MOTION, &seg(1, 20_000, -110_000, 1000, 0), 0).unwrap();
        assert_eq!(sim.next_wake_us(0), Some(STEP_US));
        let c = run(&mut sim, 0, 5_000_000);
        let z = last(&c, Z_POS, Position::decode).unwrap();
        assert!((z.counts + 105_000).abs() <= 20, "{}", z.counts);
        assert_eq!(sim.queue_state().done, 0);
        let c = run(&mut sim, 5_000_000, 10_100_000);
        assert_eq!(last(&c, Z_POS, Position::decode).unwrap().counts, -110_000);
        let q = last(&c, MOTION, QueueState::decode).unwrap();
        assert_eq!((q.done, q.flags & QueueState::IDLE), (1, QueueState::IDLE));
        assert_eq!(sim.real_um(), (20_000, -110_000));
    }

    #[test]
    fn pn17_a_rapid_runs_each_axis_at_its_rate() {
        let mut sim = SimDriver::new(SimConfig::pc_turn_50());
        sim.write(MOTION, &seg(1, 20_000, -87_500, 0, Segment::RAPID), 0).unwrap();
        // 12.5 mm at 12.5 mm/s: one second.
        run(&mut sim, 0, 990_000);
        assert_eq!(sim.queue_state().done, 0);
        run(&mut sim, 990_000, 1_010_000);
        assert_eq!(sim.queue_state().done, 1);
    }

    #[test]
    fn pn17_past_the_stop_without_limits_the_axis_stalls_and_loses_position() {
        let mut sim = SimDriver::new(SimConfig::pc_turn_50());
        // Z to +10 mm: past its +0.5 mm travel and the stop 2 mm beyond.
        sim.write(MOTION, &seg(1, 20_000, 10_000, 0, Segment::RAPID), 0).unwrap();
        let c = run(&mut sim, 0, 20_000_000);
        assert_eq!(sim.queue_state().done, 1, "nothing stops it");
        assert_eq!(last(&c, Z_POS, Position::decode).unwrap().counts, 10_000, "the count ran on");
        assert_eq!(sim.real_um().1, 500 + STOP_MARGIN_UM, "the slide did not");
        assert_eq!(last(&c, Z_HOME, Digital::decode).map(|d| d.on), Some(true));
        // Back to -100 mm by the count: the 7.5 mm of steps lost at the
        // stop put the slide 7.5 mm further toward the chuck than the count
        // says, the crash an unreferenced machine risks.
        sim.write(MOTION, &seg(2, 20_000, -100_000, 0, Segment::RAPID), 20_000_000).unwrap();
        run(&mut sim, 20_000_000, 40_000_000);
        assert_eq!(sim.commanded_um().1, -100_000);
        assert_eq!(sim.real_um().1, -100_000 - 7_500);
    }

    #[test]
    fn pn17_with_limits_a_limit_stops_motion_until_acknowledged() {
        let mut sim = SimDriver::new(SimConfig { limits: true, ..SimConfig::pc_turn_50() });
        sim.snapshot(0);
        sim.write(MOTION, &seg(1, 20_000, 10_000, 0, Segment::RAPID), 0).unwrap();
        sim.write(MOTION, &seg(2, 20_000, -50_000, 0, Segment::RAPID), 0).unwrap();
        let c = run(&mut sim, 0, 20_000_000);
        let q = last(&c, MOTION, QueueState::decode).unwrap();
        assert_ne!(q.flags & QueueState::STOPPED, 0);
        assert_eq!(q.done, 0, "the queue was emptied");
        assert!(sim.real_um().1 <= 500 + 5, "stopped at the switch, not the stop");
        let z_max = X_LIMIT_MIN + 3;
        assert_eq!(last(&c, z_max, Digital::decode).map(|d| d.on), Some(true));
        // Refused while stopped, counted as an overrun.
        sim.write(MOTION, &seg(3, 20_000, -50_000, 0, Segment::RAPID), 20_000_000).unwrap();
        let q = last(&sim.poll(20_001_000), MOTION, QueueState::decode).unwrap();
        assert_ne!(q.flags & QueueState::OVERRUN, 0);
        // A flush acknowledges; then it moves off the switch.
        sim.write(MOTION, &Segment::flush(4).encode(), 20_001_000).unwrap();
        sim.write(MOTION, &seg(5, 20_000, -50_000, 0, Segment::RAPID), 20_001_000).unwrap();
        run(&mut sim, 20_001_000, 40_000_000);
        assert_eq!(sim.queue_state().done, 5);
    }

    #[test]
    fn pn17_the_estop_stops_everything() {
        let mut sim = SimDriver::new(SimConfig::pc_turn_50());
        sim.snapshot(0);
        sim.write(SPINDLE_RPM, &Setpoint { value: 1200 }.encode(), 0).unwrap();
        sim.write(MOTION, &seg(1, 20_000, -200_000, 500, 0), 0).unwrap();
        run(&mut sim, 0, 1_000_000);
        assert_eq!(sim.spindle_rpm(), 1200);
        sim.set_estop(false);
        let c = run(&mut sim, 1_000_000, 2_000_000);
        assert_eq!(last(&c, ESTOP, Digital::decode).map(|d| d.on), Some(false));
        assert_eq!(sim.spindle_rpm(), 0, "spun down within a second");
        let q = last(&c, MOTION, QueueState::decode).unwrap();
        assert_ne!(q.flags & QueueState::STOPPED, 0);
        // The spindle stays off while stopped; a flush with the chain
        // still open does not clear it.
        sim.write(SPINDLE_RPM, &Setpoint { value: 500 }.encode(), 2_000_000).unwrap();
        sim.write(MOTION, &Segment::flush(2).encode(), 2_000_000).unwrap();
        assert_ne!(sim.queue_state().flags & QueueState::STOPPED, 0);
        sim.set_estop(true);
        sim.write(MOTION, &Segment::flush(3).encode(), 2_000_000).unwrap();
        assert_eq!(sim.queue_state().flags & QueueState::STOPPED, 0);
    }

    #[test]
    fn pn17_threading_follows_the_spindle_and_the_turret_indexes() {
        let mut sim = SimDriver::new(SimConfig::pc_turn_50());
        sim.write(SPINDLE_RPM, &Setpoint { value: 300 }.encode(), 0).unwrap();
        run(&mut sim, 0, 1_000_000);
        // 1.5 mm/rev at 300 rpm: 7.5 mm/s; 15 mm takes two seconds.
        sim.write(MOTION, &seg(1, 20_000, -115_000, 1500, Segment::SYNC), 1_000_000).unwrap();
        run(&mut sim, 1_000_000, 2_990_000);
        assert_eq!(sim.queue_state().done, 0);
        run(&mut sim, 2_990_000, 3_020_000);
        assert_eq!(sim.queue_state().done, 1);

        assert_eq!(sim.write(TURRET_STATION, &Setpoint { value: 7 }.encode(), 0), Err(DriverError::OutOfRange));
        sim.write(TURRET_STATION, &Setpoint { value: 3 }.encode(), 3_020_000).unwrap();
        let c = sim.poll(3_021_000);
        assert_eq!(last(&c, TURRET_READY, Digital::decode).map(|d| d.on), Some(false));
        assert_eq!(sim.next_wake_us(3_021_000), Some(5_020_000));
        let c = sim.poll(5_020_000);
        assert_eq!(last(&c, TURRET_READY, Digital::decode).map(|d| d.on), Some(true));
        assert_eq!(sim.station(), 3);
        assert_eq!(sim.write(X_HOME, &[0; 9], 0), Err(DriverError::NotWritable));
        assert_eq!(sim.write(MOTION, &[0; 3], 0), Err(DriverError::Malformed));
    }

    #[test]
    fn pn17_a_full_queue_overruns() {
        let mut sim = SimDriver::new(SimConfig { queue: 2, ..SimConfig::pc_turn_50() });
        for s in 1..=3 {
            sim.write(MOTION, &seg(s, 20_000, -100_000 - 1000 * s as i32, 100, 0), 0).unwrap();
        }
        let q = sim.queue_state();
        assert_eq!((q.free, q.flags & QueueState::OVERRUN), (0, QueueState::OVERRUN));
    }
}
