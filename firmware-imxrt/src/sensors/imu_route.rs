//! The IMU source route's core (SC4 on the board, SC7): the FXOS8700CQ
//! polled on the route's clock into `middleware::stream::samples` batches.
//!
//! The shell runs a [`ImuRoute`] like this:
//!
//! 1. [`ImuRoute::new`] from the payload's [`ImuConfig`] (its rate must be
//!    one of the FXOS8700CQ's output data rates, with the magnetometer
//!    the hybrid rates); write [`ImuRoute::plan`] over I2C (standby,
//!    range, hybrid, rate and active).
//! 2. Whenever [`ImuRoute::due_us`] has passed, read
//!    [`ImuRoute::burst_len`] bytes from `STATUS` and hand them to
//!    [`ImuRoute::on_burst`], which keeps the sample if the data-ready
//!    flag says one is there and returns a whole batch to publish once
//!    `samples` have been taken. Not ready yet: polled again an eighth of
//!    a period later. An overwrite flag (a sample the device replaced
//!    before it was read) counts a lost sample, so the receiver's gap from
//!    `first_index` stays exact for what was published and the route's
//!    `lost` counter says what the device dropped.
//! 3. A failed read ([`ImuRoute::on_failure`]) is counted and retried on the
//!    next period.
//! 4. On stop, write [`fxos8700::Config::stop`].
//!
//! The schedule never drifts: the next due time steps by one period from
//! the last, and resynchronises to "now plus a period" when the shell fell
//! more than a period behind.

use heapless::Vec;
use middleware::stream::samples::{self, MAX_BATCH_DATA};
use middleware::wasm_abi::AbiError;
use middleware::wasm_abi::route::{ImuConfig, SampleSource};

use super::fxos8700::{self, Odr, Range, Readiness, RegWrite};

/// Most `i16` values one batch holds.
pub const MAX_VALUES: usize = MAX_BATCH_DATA / 2;

/// A running IMU route.
#[derive(Debug, Clone)]
pub struct ImuRoute {
    cfg: ImuConfig,
    fx: fxos8700::Config,
    period_us: u64,
    next_us: u64,
    first_index: u64,
    batch_ts: u64,
    values: Vec<i16, MAX_VALUES>,
    taken: u16,
    lost: u32,
    failures: u32,
    batches: u32,
}

impl ImuRoute {
    /// A route for `cfg` starting at `now_us`: `InvalidArgument` when the
    /// rate is not an FXOS8700CQ output rate (in hybrid mode with the
    /// magnetometer) or the range is not one it has.
    pub fn new(cfg: ImuConfig, now_us: u64) -> Result<Self, AbiError> {
        let cfg = cfg.validated()?;
        let odr = Odr::from_hz(u32::from(cfg.rate_hz), cfg.mag).ok_or(AbiError::InvalidArgument)?;
        let range = Range::from_g(cfg.range_g).ok_or(AbiError::InvalidArgument)?;
        let fx = fxos8700::Config {
            odr,
            range,
            magnetometer: cfg.mag,
        };
        let period_us = 1_000_000 / u64::from(cfg.rate_hz.max(1));
        if usize::from(cfg.samples) * usize::from(cfg.channels()) > MAX_VALUES {
            return Err(AbiError::InvalidArgument);
        }
        Ok(ImuRoute {
            cfg,
            fx,
            period_us,
            next_us: now_us.saturating_add(period_us),
            first_index: 0,
            batch_ts: now_us,
            values: Vec::new(),
            taken: 0,
            lost: 0,
            failures: 0,
            batches: 0,
        })
    }

    /// The register writes that start the device.
    pub const fn plan(&self) -> [RegWrite; 5] {
        self.fx.plan()
    }

    /// Bytes to read from `STATUS` at each poll.
    pub const fn burst_len(&self) -> usize {
        self.fx.burst_len()
    }

    /// When the next poll is due, in µs.
    pub const fn due_us(&self) -> u64 {
        self.next_us
    }

    /// Samples the device overwrote before they were read.
    pub const fn lost(&self) -> u32 {
        self.lost
    }

    /// Failed reads.
    pub const fn failures(&self) -> u32 {
        self.failures
    }

    /// Batches produced.
    pub const fn batches(&self) -> u32 {
        self.batches
    }

    fn step(&mut self, now_us: u64) {
        let next = self.next_us.saturating_add(self.period_us);
        self.next_us = if next.saturating_add(self.period_us) <= now_us {
            now_us.saturating_add(self.period_us)
        } else {
            next
        };
    }

    /// A read failed at `now_us`: counted, retried next period.
    pub fn on_failure(&mut self, now_us: u64) {
        self.failures = self.failures.saturating_add(1);
        self.step(now_us);
    }

    /// The burst read at `now_us` (from `STATUS`): keeps the sample if one
    /// was ready and, when the batch is complete, encodes it into `out`
    /// and returns its length.
    pub fn on_burst(&mut self, now_us: u64, burst: &[u8], out: &mut [u8]) -> Option<usize> {
        let status = *burst.first()?;
        match fxos8700::data_ready(status) {
            Readiness::NotReady => {
                self.next_us = now_us.saturating_add((self.period_us / 8).max(100));
                return None;
            }
            Readiness::Overrun => self.lost = self.lost.saturating_add(1),
            Readiness::Ready => {}
        }
        self.step(now_us);
        let sample = fxos8700::decode(&self.fx, burst)?;
        if self.taken == 0 {
            self.batch_ts = now_us;
        }
        let mut one = [0i16; 6];
        let n = sample.write_to(self.cfg.mag, &mut one);
        for v in one.get(..n).unwrap_or_default() {
            // Capacity checked in `new` for a whole batch.
            let _ = self.values.push(*v);
        }
        self.taken = self.taken.saturating_add(1);
        if self.taken < self.cfg.samples {
            return None;
        }
        let header =
            SampleSource::Imu(self.cfg).header(self.first_index, self.batch_ts, self.taken);
        let len = samples::encode_i16(&header, &self.values, out).ok();
        self.first_index = self.first_index.saturating_add(u64::from(self.taken));
        self.taken = 0;
        self.values.clear();
        if len.is_some() {
            self.batches = self.batches.saturating_add(1);
        }
        len
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use middleware::stream::samples::{SampleFormat, SensorKind, decode};

    /// A burst with the data-ready flag and Z = +1 g at ±2 g (4096 counts,
    /// left-justified 14 bits: 0x4000).
    fn burst_1g() -> [u8; 7] {
        [fxos8700::ZYXDR, 0, 0, 0, 0, 0x40, 0x00]
    }

    #[test]
    fn sc7_a_batch_of_samples_is_published_once_full_with_exact_indices() {
        let cfg = ImuConfig::new(100, 2, false, 4).unwrap();
        let mut r = ImuRoute::new(cfg, 1_000).unwrap();
        assert_eq!(r.due_us(), 11_000);
        assert_eq!(r.burst_len(), 7);
        let mut out = [0u8; 512];
        let mut published = std::vec::Vec::new();
        let mut now = 11_000;
        for _ in 0..8 {
            if let Some(n) = r.on_burst(now, &burst_1g(), &mut out) {
                published.push(out[..n].to_vec());
            }
            now = r.due_us();
        }
        assert_eq!(published.len(), 2);
        let b = decode(&published[1]).unwrap();
        assert_eq!(b.header.kind, SensorKind::Imu);
        assert_eq!(b.header.format, SampleFormat::I16);
        assert_eq!(
            (b.header.first_index, b.header.count, b.header.channels),
            (4, 4, 3)
        );
        let z: std::vec::Vec<i16> = b.i16_values().skip(2).step_by(3).collect();
        assert_eq!(
            z, [16384; 4],
            "+1 g on Z: 16384 per g at +-2 g, as the header says"
        );
        let ug = i64::from(z[0]) * i64::from(b.header.scale);
        assert!(
            (ug - 1_000_000).abs() < 1_000,
            "value x scale is about 1 g in ug: {ug}"
        );
        assert_eq!(r.batches(), 2);
    }

    #[test]
    fn sc7_not_ready_polls_again_soon_and_overruns_count_lost_samples() {
        let cfg = ImuConfig::new(100, 2, false, 2).unwrap();
        let mut r = ImuRoute::new(cfg, 0).unwrap();
        let mut out = [0u8; 512];
        assert_eq!(r.on_burst(10_000, &[0; 7], &mut out), None);
        assert_eq!(r.due_us(), 11_250, "an eighth of the 10 ms period");
        let mut over = burst_1g();
        over[0] |= fxos8700::ZYXOW;
        assert_eq!(r.on_burst(11_250, &over, &mut out), None);
        assert_eq!(r.lost(), 1);
        r.on_failure(20_000);
        assert_eq!(r.failures(), 1);
        assert!(r.on_burst(r.due_us(), &burst_1g(), &mut out).is_some());
    }

    #[test]
    fn sc7_a_late_shell_resynchronises_instead_of_bursting() {
        let cfg = ImuConfig::new(100, 2, false, 2).unwrap();
        let mut r = ImuRoute::new(cfg, 0).unwrap();
        let mut out = [0u8; 512];
        let _ = r.on_burst(1_000_000, &burst_1g(), &mut out);
        assert_eq!(r.due_us(), 1_010_000);
    }

    #[test]
    fn sc7_rates_the_device_lacks_are_refused() {
        // 50, 100, 200, 400, 800 Hz are FXOS8700 rates; with the
        // magnetometer (hybrid) 800 Hz is not.
        for hz in [50, 100, 200, 400, 800] {
            assert!(
                ImuRoute::new(ImuConfig::new(hz, 4, false, 16).unwrap(), 0).is_ok(),
                "{hz}"
            );
        }
        assert_eq!(
            ImuConfig::new(800, 4, true, 16)
                .and_then(|c| ImuRoute::new(c, 0))
                .err(),
            Some(AbiError::InvalidArgument)
        );
    }
}
