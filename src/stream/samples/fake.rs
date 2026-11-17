//! Deterministic fake sensors for the simulators (SC6): what
//! `SourceKind::Imu` and `SourceKind::Microphone` serve on a board
//! without the hardware, as [`super::pattern`] is the fake camera.
//!
//! Every unit is a pure function of its index and the route's config, so a
//! host builds any batch on demand and two simulators agree byte for byte.
//!
//! * IMU ([`imu_sample`]): the board turns about its X axis once every
//!   [`TILT_PERIOD_MS`]: Y reads `g·sin φ`, Z `g·cos φ` (flat, +1 g on Z,
//!   at index 0; upside down, −1 g, half a turn later), X reads 0. With the
//!   magnetometer, a 20 µT field along X and 40 µT turning with the board
//!   in Y/Z.
//! * Microphone ([`mic_sample`]): a one-second cycle, half amplitude: a
//!   [`TONE_HZ`] tone for the first half, then a linear chirp from
//!   [`CHIRP_FROM_HZ`] to [`CHIRP_TO_HZ`].
//!
//! [`FakeRoute`] paces a fake route on a host clock: batch `b` covers
//! units `b·n .. (b+1)·n` and is due when its last unit has been taken.
//!
//! Sines come from an integer approximation ([`sine_q15`], Bhaskara I,
//! within 0.2 % of full scale), so nothing here uses floating point.

use super::{BatchError, MAX_BATCH_DATA, encode, mulaw_encode};
use crate::wasm_abi::route::{AudioMode, ImuConfig, MicConfig, SampleSource};

/// One full turn of the fake IMU, in ms.
pub const TILT_PERIOD_MS: u32 = 8_000;

/// The fake microphone's tone, in Hz.
pub const TONE_HZ: u32 = 1_000;

/// Where the fake microphone's chirp starts, in Hz.
pub const CHIRP_FROM_HZ: u32 = 250;

/// Where the fake microphone's chirp ends, in Hz (below the Nyquist
/// frequency of the lowest rate, 8 kHz).
pub const CHIRP_TO_HZ: u32 = 3_000;

/// Half of `i16` full scale: the fake microphone's amplitude.
pub const MIC_AMPLITUDE: i32 = 16_384;

/// `sin(2π · phase / 65536)` in Q15 (`-32767..=32767`).
pub const fn sine_q15(phase: u16) -> i16 {
    // Bhaskara I on the half turn: sin x ≈ 16v / (5π² − 4v) with
    // v = x(π − x); in units of a half turn u (0..32768), v scales to
    // u(32768 − u) and 5π² to 5 · 2^30.
    let negative = phase >= 0x8000;
    let u = (phase & 0x7fff) as i64;
    let v = u * (32_768 - u);
    let s = (32_767 * 16 * v) / (5 * (1i64 << 30) - 4 * v);
    if negative { -(s as i16) } else { s as i16 }
}

/// `cos(2π · phase / 65536)` in Q15.
pub const fn cosine_q15(phase: u16) -> i16 {
    sine_q15(phase.wrapping_add(0x4000))
}

/// The tilt phase of IMU sample `index` at `rate_hz` (a fraction of a turn
/// in 1/65536).
const fn tilt_phase(index: u64, rate_hz: u32) -> u16 {
    let period = rate_hz as u64 * TILT_PERIOD_MS as u64 / 1000;
    if period == 0 {
        return 0;
    }
    ((index % period) * 65_536 / period) as u16
}

const fn scaled(q15: i16, full: i32) -> i16 {
    let v = q15 as i32 * full / 32_767;
    if v > i16::MAX as i32 {
        i16::MAX
    } else if v < i16::MIN as i32 {
        i16::MIN
    } else {
        v as i16
    }
}

/// IMU sample `index` of a route at `rate_hz` with a full scale of
/// ±`range_g` g: accelerometer X, Y, Z (1 g = `32768 / range_g` LSB) then
/// magnetometer X, Y, Z at [`super::MAG_LSB_NT`].
pub const fn imu_sample(index: u64, rate_hz: u32, range_g: u16) -> [i16; 6] {
    let phase = tilt_phase(index, rate_hz);
    let one_g = if range_g == 0 {
        0
    } else {
        32_768 / range_g as i32
    };
    let (s, c) = (sine_q15(phase), cosine_q15(phase));
    [
        0,
        scaled(s, one_g),
        scaled(c, one_g),
        200,
        scaled(s, 400),
        scaled(c, 400),
    ]
}

/// Microphone sample `index` of a route at `rate_hz` (PCM, before any
/// encoding).
pub const fn mic_sample(index: u64, rate_hz: u32) -> i16 {
    if rate_hz < 2 {
        return 0;
    }
    let rate = rate_hz as u64;
    let p = index % rate;
    let half = rate / 2;
    // The phase in 1/65536 of a turn: f·n/rate for the tone, and for the
    // chirp f0·q/rate + (f1 − f0)·q²/(2·half·rate) (the integral of a
    // frequency rising linearly over `half` samples).
    let phase = if p < half {
        p * TONE_HZ as u64 * 65_536 / rate
    } else {
        let q = p - half;
        let lin = CHIRP_FROM_HZ as u64 * q * 65_536 / rate;
        let sweep = (CHIRP_TO_HZ - CHIRP_FROM_HZ) as u64 * q * q * 32_768 / (half * rate);
        lin.wrapping_add(sweep)
    };
    scaled(sine_q15(phase as u16), MIC_AMPLITUDE)
}

/// Writes the fake IMU batch of `count` samples starting at `first_index`
/// (taken at `timestamp_us`) for `cfg` into `out`; returns its length.
pub fn imu_batch(
    cfg: &ImuConfig,
    first_index: u64,
    timestamp_us: u64,
    count: u16,
    out: &mut [u8],
) -> Result<usize, BatchError> {
    let header = cfg.header(first_index, timestamp_us, count);
    header.check()?;
    let mut data = [0u8; MAX_BATCH_DATA];
    let channels = usize::from(header.channels);
    let mut at = 0;
    for i in 0..u64::from(count) {
        let s = imu_sample(first_index.wrapping_add(i), header.rate_hz, header.range);
        for v in s.iter().take(channels) {
            let dst = data.get_mut(at..at + 2).ok_or(BatchError::Length)?;
            dst.copy_from_slice(&v.to_le_bytes());
            at += 2;
        }
    }
    encode(&header, data.get(..at).ok_or(BatchError::Length)?, out)
}

/// Writes the fake microphone batch of `count` units (samples, or level
/// blocks of `cfg.block` samples) starting at unit `first_index` (taken at
/// `timestamp_us`) for `cfg` into `out`; returns its length.
pub fn mic_batch(
    cfg: &MicConfig,
    first_index: u64,
    timestamp_us: u64,
    count: u16,
    out: &mut [u8],
) -> Result<usize, BatchError> {
    let header = cfg.header(first_index, timestamp_us, count);
    header.check()?;
    let rate = header.rate_hz;
    let mut data = [0u8; MAX_BATCH_DATA];
    let mut at = 0;
    for i in 0..u64::from(count) {
        let unit = first_index.wrapping_add(i);
        let bytes: ([u8; 4], usize) = match cfg.mode {
            AudioMode::Pcm => {
                let [a, b] = mic_sample(unit, rate).to_le_bytes();
                ([a, b, 0, 0], 2)
            }
            AudioMode::MuLaw => ([mulaw_encode(mic_sample(unit, rate)), 0, 0, 0], 1),
            AudioMode::Levels => {
                let (rms, peak) = fake_levels(unit, cfg.block, rate);
                let [r0, r1] = rms.to_le_bytes();
                let [p0, p1] = peak.to_le_bytes();
                ([r0, r1, p0, p1], 4)
            }
        };
        let src = bytes.0.get(..bytes.1).ok_or(BatchError::Length)?;
        data.get_mut(at..at + src.len())
            .ok_or(BatchError::Length)?
            .copy_from_slice(src);
        at += src.len();
    }
    encode(&header, data.get(..at).ok_or(BatchError::Length)?, out)
}

/// The levels of fake microphone block `block_index` of `block` samples
/// ([`block_levels`] of those samples, without holding them).
fn fake_levels(block_index: u64, block: u16, rate: u32) -> (u16, u16) {
    let start = block_index.wrapping_mul(u64::from(block));
    let (sum, peak) = (0..u64::from(block)).fold((0u64, 0u16), |(sum, peak), k| {
        let m = mic_sample(start.wrapping_add(k), rate).unsigned_abs();
        (sum.saturating_add(u64::from(m) * u64::from(m)), peak.max(m))
    });
    let rms = if block == 0 {
        0
    } else {
        (sum / u64::from(block)).isqrt()
    };
    (u16::try_from(rms).unwrap_or(u16::MAX), peak)
}

/// A fake sampled route on a simulator clock: which batch is due when,
/// and its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FakeRoute {
    /// The sensor and its config.
    pub source: SampleSource,
    /// When the route started (unit 0 is taken then), µs.
    pub start_us: u64,
    /// The next batch to publish (batch `b` starts at unit `b · samples`).
    pub next_batch: u64,
}

/// One batch [`FakeRoute::poll`] wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Polled {
    /// Bytes written.
    pub len: usize,
    /// Its batch number.
    pub batch: u64,
    /// Due batches skipped before it because the clock jumped past them
    /// (their units show as a gap in `first_index`).
    pub skipped: u64,
}

impl FakeRoute {
    /// A route of `source` started at `now_us`.
    pub const fn new(source: SampleSource, now_us: u64) -> Self {
        FakeRoute {
            source,
            start_us: now_us,
            next_batch: 0,
        }
    }

    /// Samples one unit spans (a level block, else 1) and the rate in Hz.
    const fn unit(&self) -> (u128, u128) {
        match &self.source {
            SampleSource::Imu(c) => (1, c.rate_hz as u128),
            SampleSource::Microphone(c) => (c.samples_per_unit() as u128, c.rate_hz as u128),
        }
    }

    /// When unit `index` is taken, µs (exact, not an accumulated period).
    pub const fn unit_time_us(&self, index: u64) -> u64 {
        let (per_unit, rate) = self.unit();
        if rate == 0 {
            return self.start_us;
        }
        self.start_us + (index as u128 * per_unit * 1_000_000 / rate) as u64
    }

    /// When batch `batch` is due: when its last unit has been taken.
    pub const fn due_us(&self, batch: u64) -> u64 {
        let units = (batch as u128 + 1) * self.source.samples() as u128;
        let (per_unit, rate) = self.unit();
        if rate == 0 {
            return u64::MAX;
        }
        self.start_us + (units * per_unit * 1_000_000 / rate) as u64
    }

    /// When the next batch is due.
    pub const fn next_due_us(&self) -> u64 {
        self.due_us(self.next_batch)
    }

    /// Writes the latest batch due at `now_us` into `out` and moves past
    /// it; `None` when nothing is due. When the clock jumped over several
    /// batches only the latest is written ([`Polled::skipped`]).
    pub fn poll(&mut self, now_us: u64, out: &mut [u8]) -> Option<Result<Polled, BatchError>> {
        if now_us < self.next_due_us() {
            return None;
        }
        let (per_unit, rate) = self.unit();
        let span = (self.source.samples() as u128 * per_unit).max(1);
        let elapsed = (now_us - self.start_us) as u128;
        // Batches complete by `now_us`: floor(elapsed · rate / (span · 1e6)).
        let done = (elapsed * rate / (span * 1_000_000)) as u64;
        let batch = done.saturating_sub(1).max(self.next_batch);
        let skipped = batch - self.next_batch;
        let n = self.source.samples();
        let first = batch.saturating_mul(u64::from(n));
        let ts = self.unit_time_us(first);
        self.next_batch = batch + 1;
        Some(self.source.fake_batch(first, ts, n, out).map(|len| Polled {
            len,
            batch,
            skipped,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{SensorKind, block_levels, decode};
    use super::*;

    #[test]
    fn sc6_sine_is_close_to_the_real_one() {
        assert_eq!(sine_q15(0), 0);
        assert_eq!(sine_q15(0x4000), 32_767);
        assert_eq!(sine_q15(0xc000), -32_767);
        assert_eq!(cosine_q15(0), 32_767);
        for p in (0..=u16::MAX).step_by(97) {
            let real = (f64::from(p) / 65_536.0 * core::f64::consts::TAU).sin() * 32_767.0;
            assert!((f64::from(sine_q15(p)) - real).abs() < 70.0, "{p}");
        }
    }

    #[test]
    fn sc6_fake_imu_tilts_once_per_period() {
        // ±2 g: 1 g = 16384 LSB. Flat at 0, on its side a quarter turn
        // later, upside down at half a turn.
        let rate = 100;
        let flat = imu_sample(0, rate, 2);
        assert_eq!(&flat[..3], &[0, 0, 16_384]);
        let quarter = imu_sample(200, rate, 2);
        assert_eq!(&quarter[..3], &[0, 16_384, 0]);
        let down = imu_sample(400, rate, 2);
        assert_eq!(&down[..3], &[0, 0, -16_384]);
        assert_eq!(imu_sample(800, rate, 2), flat, "one turn per period");
        assert_eq!(imu_sample(0, rate, 8)[2], 4_096);
        assert_eq!(flat[3], 200);
    }

    #[test]
    fn sc6_fake_microphone_is_a_tone_then_a_chirp() {
        let rate = 16_000;
        // 1 kHz at 16 kHz: 16 samples per period.
        assert_eq!(mic_sample(0, rate), 0);
        assert_eq!(mic_sample(4, rate), 16_384);
        assert_eq!(mic_sample(12, rate), -16_384);
        assert_eq!(mic_sample(20, rate), mic_sample(4, rate));
        // The chirp's zero crossings get closer together.
        let crossings = |from: u64, n: u64| {
            (from..from + n)
                .filter(|&i| (mic_sample(i, rate) >= 0) != (mic_sample(i + 1, rate) >= 0))
                .count()
        };
        let early = crossings(8_000, 800);
        let late = crossings(15_200, 800);
        assert!(late > 3 * early, "{early} {late}");
        assert_eq!(
            mic_sample(16_000, rate),
            mic_sample(0, rate),
            "one-second cycle"
        );
    }

    #[test]
    fn sc6_fake_batches_decode() {
        let imu = ImuConfig::new(100, 4, true, 10).unwrap();
        let mut out = [0u8; super::super::MAX_BATCH];
        let n = imu_batch(&imu, 200, 2_000_000, 10, &mut out).unwrap();
        let b = decode(&out[..n]).unwrap();
        assert_eq!(
            (b.header.kind, b.header.channels, b.header.count),
            (SensorKind::Imu, 6, 10)
        );
        let first: [i16; 6] = core::array::from_fn(|i| b.i16_values().nth(i).unwrap_or(0));
        assert_eq!(first, imu_sample(200, 100, 4));

        let pcm = MicConfig::new(16_000, 0, AudioMode::Pcm, 160, 0).unwrap();
        let n = mic_batch(&pcm, 0, 0, 160, &mut out).unwrap();
        let b = decode(&out[..n]).unwrap();
        assert!(b.i16_values().eq((0..160).map(|i| mic_sample(i, 16_000))));

        let law = MicConfig::new(8_000, 0, AudioMode::MuLaw, 80, 0).unwrap();
        let n = mic_batch(&law, 4, 0, 80, &mut out).unwrap();
        let b = decode(&out[..n]).unwrap();
        assert_eq!(b.data[0], mulaw_encode(mic_sample(4, 8_000)));

        let lv = MicConfig::new(16_000, 0, AudioMode::Levels, 4, 400).unwrap();
        let n = mic_batch(&lv, 0, 0, 4, &mut out).unwrap();
        let b = decode(&out[..n]).unwrap();
        for (rms, peak) in b.levels() {
            // Half-scale sine: RMS ≈ 16384/√2, peak ≈ 16384.
            assert!((11_400..=11_800).contains(&rms), "{rms}");
            assert!((16_300..=16_384).contains(&peak), "{peak}");
        }
        let direct: [i16; 400] = core::array::from_fn(|i| mic_sample(i as u64, 16_000));
        assert_eq!(b.levels().next(), Some(block_levels(&direct)));
    }

    #[test]
    fn sc6_fake_route_paces_batches_on_the_clock() {
        use super::super::{Gap, gap};
        let imu = SampleSource::Imu(ImuConfig::new(100, 2, false, 10).unwrap());
        let mut r = FakeRoute::new(imu, 5_000);
        let mut out = [0u8; super::super::MAX_BATCH];
        assert_eq!(r.next_due_us(), 105_000, "10 samples at 100 Hz");
        assert!(r.poll(104_999, &mut out).is_none());
        let p = r.poll(105_000, &mut out).unwrap().unwrap();
        assert_eq!((p.batch, p.skipped), (0, 0));
        let a = decode(&out[..p.len]).unwrap().header;
        assert_eq!((a.first_index, a.timestamp_us, a.count), (0, 5_000, 10));
        assert!(r.poll(105_000, &mut out).is_none(), "once");
        // The clock jumps three batches: the latest is sent, the rest skipped.
        let p = r.poll(405_000, &mut out).unwrap().unwrap();
        assert_eq!((p.batch, p.skipped), (3, 2));
        let b = decode(&out[..p.len]).unwrap().header;
        assert_eq!(b.timestamp_us, 5_000 + 300_000);
        assert_eq!(gap(&a, &b), Gap::Lost(20));

        let mic =
            SampleSource::Microphone(MicConfig::new(16_000, 0, AudioMode::Levels, 2, 400).unwrap());
        let mut m = FakeRoute::new(mic, 0);
        assert_eq!(m.next_due_us(), 50_000, "2 blocks of 400 at 16 kHz");
        assert_eq!(m.unit_time_us(1), 25_000);
        let p = m.poll(50_000, &mut out).unwrap().unwrap();
        let h = decode(&out[..p.len]).unwrap().header;
        assert_eq!((h.first_index, h.count, h.scale), (0, 2, 400));
    }
}
