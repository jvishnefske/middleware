//! The microphone source route's core (SC8): microphone samples, as the
//! shell drains them from SAI1's receive ring, into
//! `middleware::stream::samples` batches of the route's [`AudioMode`].
//!
//! The shell runs a [`MicRoute`] like this:
//!
//! 1. [`MicRoute::new`] from the payload's [`MicConfig`] (its rate must be
//!    one the codec and SAI1 are planned for, [`AudioRate`]).
//! 2. Every [`POLL_US`] or so, hand the samples that arrived to
//!    [`MicRoute::feed`] with the time of the last one. It returns how
//!    many it took and, when they completed a batch, the batch's length in
//!    `out`: publish it, then feed the rest.
//! 3. When samples were lost (the ring overran while the shell was busy),
//!    call [`MicRoute::gap`] with how many: the batch in progress is
//!    dropped, so every published batch is whole and the receiver's gap
//!    from `first_index` says exactly how many units are missing; the
//!    route's [`MicRoute::lost`] counts the samples that were not
//!    published.
//!
//! Units: a PCM or µ-law unit is one sample; a level unit is the RMS and
//! peak ([`super::audio::levels`]'s arithmetic) of `cfg.block` samples,
//! blocks aligned to multiples of `block` from the route's start (after a
//! gap the samples up to the next boundary are skipped and counted lost),
//! so `first_index` times the block is the sample index. A batch's
//! timestamp is the first unit's first sample's, derived from the drain
//! time and the rate.

use heapless::Vec;
use middleware::stream::samples::{self, MAX_BATCH_DATA};
use middleware::wasm_abi::AbiError;
use middleware::wasm_abi::route::{AudioMode, MicConfig};

use super::AudioRate;
use super::audio::{isqrt, mulaw_encode};

/// How often the shell should drain the ring while a route runs, in µs.
pub const POLL_US: u64 = 5_000;

/// A running microphone route.
#[derive(Debug, Clone)]
pub struct MicRoute {
    cfg: MicConfig,
    rate: AudioRate,
    /// Samples per unit (1, or the level block).
    spu: u32,
    /// Index of the next sample since the route started, lost ones
    /// included.
    next_sample: u64,
    /// Samples still to skip to reach a level-block boundary.
    skip: u32,
    unit_first: u64,
    batch_ts: u64,
    units: u16,
    data: Vec<u8, MAX_BATCH_DATA>,
    acc_sum: u64,
    acc_peak: u16,
    acc_n: u32,
    lost: u32,
    batches: u32,
}

/// What [`MicRoute::feed`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fed {
    /// Samples taken from the slice.
    pub consumed: usize,
    /// A completed batch's length in `out`.
    pub batch: Option<usize>,
}

impl MicRoute {
    /// A route for `cfg` starting at `now_us`: `InvalidArgument` for a rate
    /// the board does not plan for or a config the ABI refuses.
    pub fn new(cfg: MicConfig, now_us: u64) -> Result<Self, AbiError> {
        let cfg = cfg.validated()?;
        let rate = AudioRate::from_hz(u32::from(cfg.rate_hz)).ok_or(AbiError::InvalidArgument)?;
        Ok(MicRoute {
            cfg,
            rate,
            spu: cfg.samples_per_unit().max(1),
            next_sample: 0,
            skip: 0,
            unit_first: 0,
            batch_ts: now_us,
            units: 0,
            data: Vec::new(),
            acc_sum: 0,
            acc_peak: 0,
            acc_n: 0,
            lost: 0,
            batches: 0,
        })
    }

    /// The route's rate, for the codec and SAI1 plans.
    pub const fn rate(&self) -> AudioRate {
        self.rate
    }

    /// The input gain the payload asked for, in dB.
    pub const fn gain_db(&self) -> u8 {
        self.cfg.gain_db
    }

    /// Samples not published (lost in the ring, or dropped with a partial
    /// batch or while realigning a level block).
    pub const fn lost(&self) -> u32 {
        self.lost
    }

    /// Batches produced.
    pub const fn batches(&self) -> u32 {
        self.batches
    }

    /// Samples in the batch in progress (what a gap would drop).
    const fn pending_samples(&self) -> u32 {
        match self.cfg.mode {
            AudioMode::Levels => self.units as u32 * self.spu + self.acc_n,
            AudioMode::MuLaw | AudioMode::Pcm => self.units as u32,
        }
    }

    /// `n` samples were lost before the next ones fed.
    pub fn gap(&mut self, n: u32) {
        if n == 0 {
            return;
        }
        let dropped = self.pending_samples();
        self.lost = self.lost.saturating_add(dropped).saturating_add(n);
        self.units = 0;
        self.data.clear();
        self.acc_sum = 0;
        self.acc_peak = 0;
        self.acc_n = 0;
        self.skip = 0;
        self.next_sample = self.next_sample.saturating_add(u64::from(n));
        let into_block = (self.next_sample % u64::from(self.spu)) as u32;
        if into_block != 0 {
            self.skip = self.spu - into_block;
        }
    }

    /// Takes samples from `samples`, the last of which was taken at
    /// `now_us`, until they run out or complete a batch, which is encoded
    /// into `out` (at least [`samples::MAX_BATCH`] bytes).
    pub fn feed(&mut self, now_us: u64, samples: &[i16], out: &mut [u8]) -> Fed {
        let rate = u64::from(self.cfg.rate_hz.max(1));
        let len = samples.len();
        for (i, &s) in samples.iter().enumerate() {
            if self.skip > 0 {
                self.skip -= 1;
                self.lost = self.lost.saturating_add(1);
                self.next_sample = self.next_sample.saturating_add(1);
                continue;
            }
            if self.units == 0 && self.acc_n == 0 {
                let behind = (len - 1 - i) as u64 * 1_000_000 / rate;
                self.batch_ts = now_us.saturating_sub(behind);
                self.unit_first = self.next_sample / u64::from(self.spu);
            }
            self.next_sample = self.next_sample.saturating_add(1);
            // Capacity: `MicConfig::validated` bounds a batch to
            // MAX_BATCH_DATA, so the pushes cannot fail.
            match self.cfg.mode {
                AudioMode::Pcm => {
                    let _ = self.data.extend_from_slice(&s.to_le_bytes());
                    self.units += 1;
                }
                AudioMode::MuLaw => {
                    let _ = self.data.push(mulaw_encode(s));
                    self.units += 1;
                }
                AudioMode::Levels => {
                    let m = s.unsigned_abs();
                    self.acc_peak = self.acc_peak.max(m);
                    self.acc_sum = self.acc_sum.saturating_add(u64::from(m) * u64::from(m));
                    self.acc_n += 1;
                    if self.acc_n == self.spu {
                        let rms = isqrt(self.acc_sum / u64::from(self.spu));
                        let rms = u16::try_from(rms).unwrap_or(u16::MAX);
                        let _ = self.data.extend_from_slice(&rms.to_le_bytes());
                        let _ = self.data.extend_from_slice(&self.acc_peak.to_le_bytes());
                        self.units += 1;
                        self.acc_sum = 0;
                        self.acc_peak = 0;
                        self.acc_n = 0;
                    }
                }
            }
            if self.units >= self.cfg.samples {
                let header = self.cfg.header(self.unit_first, self.batch_ts, self.units);
                let batch = samples::encode(&header, &self.data, out).ok();
                self.units = 0;
                self.data.clear();
                if batch.is_some() {
                    self.batches = self.batches.saturating_add(1);
                }
                return Fed { consumed: i + 1, batch };
            }
        }
        Fed { consumed: len, batch: None }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::super::audio;
    use super::*;
    use middleware::stream::samples::{MAX_BATCH, SampleFormat, SensorKind, decode, mulaw_decode};
    use std::vec::Vec as StdVec;

    /// `n` samples of a `hz` sine of amplitude `amp` at `rate`, from
    /// sample `start`.
    fn sine(hz: f64, amp: f64, rate: u32, start: usize, n: usize) -> StdVec<i16> {
        (start..start + n)
            .map(|i| (amp * (2.0 * core::f64::consts::PI * hz * i as f64 / f64::from(rate)).sin()) as i16)
            .collect()
    }

    /// Feeds `input` in drains of `chunk` samples, the drain at sample `k`
    /// timed `t0 + k / rate`; returns the batches.
    fn run(r: &mut MicRoute, input: &[i16], chunk: usize, t0: u64) -> StdVec<StdVec<u8>> {
        let rate = u64::from(r.cfg.rate_hz);
        let mut out = [0u8; MAX_BATCH];
        let mut batches = StdVec::new();
        let mut at = 0;
        while at < input.len() {
            let end = (at + chunk).min(input.len());
            let now = t0 + (end as u64 - 1) * 1_000_000 / rate;
            let mut s = &input[at..end];
            while !s.is_empty() {
                let fed = r.feed(now, s, &mut out);
                if let Some(n) = fed.batch {
                    batches.push(out[..n].to_vec());
                }
                s = &s[fed.consumed..];
            }
            at = end;
        }
        batches
    }

    #[test]
    fn sc8_pcm_batches_carry_the_samples_with_exact_indices_and_period() {
        let cfg = MicConfig::new(16_000, 0, AudioMode::Pcm, 160, 0).unwrap();
        let mut r = MicRoute::new(cfg, 0).unwrap();
        let input = sine(1000.0, 10_000.0, 16_000, 0, 16_000);
        // Drains of 80 samples (5 ms): two per 10 ms batch.
        let batches = run(&mut r, &input, 80, 1_000_000);
        assert_eq!(batches.len(), 100);
        assert_eq!(r.batches(), 100);
        assert_eq!(r.lost(), 0);
        let mut back = StdVec::new();
        for (k, b) in batches.iter().enumerate() {
            let b = decode(b).unwrap();
            assert_eq!((b.header.kind, b.header.format), (SensorKind::Microphone, SampleFormat::I16));
            assert_eq!((b.header.first_index, b.header.count), (k as u64 * 160, 160));
            // 10 ms apart, the first sample at t0 + index / rate.
            assert_eq!(b.header.timestamp_us, 1_000_000 + k as u64 * 10_000);
            back.extend(b.i16_values());
        }
        assert_eq!(back, input);
    }

    #[test]
    fn sc8_mulaw_batches_decode_close_to_the_input() {
        let cfg = MicConfig::new(8_000, 0, AudioMode::MuLaw, 80, 0).unwrap();
        let mut r = MicRoute::new(cfg, 0).unwrap();
        let input = sine(440.0, 20_000.0, 8_000, 0, 800);
        let batches = run(&mut r, &input, 33, 0);
        assert_eq!(batches.len(), 10);
        let decoded: StdVec<i16> = batches
            .iter()
            .flat_map(|b| decode(b).unwrap().data.iter().map(|c| mulaw_decode(*c)).collect::<StdVec<_>>())
            .collect();
        for (a, b) in input.iter().zip(&decoded) {
            // µ-law's step at 20000 is 1024; anywhere it is at most 1/16
            // of the magnitude plus the smallest step.
            let err = (i32::from(*a) - i32::from(*b)).abs();
            assert!(err <= i32::from(a.unsigned_abs() / 16) + 16, "{a} -> {b}");
        }
    }

    #[test]
    fn sc8_levels_match_the_block_arithmetic_for_a_tone_and_silence() {
        let cfg = MicConfig::new(16_000, 0, AudioMode::Levels, 2, 800).unwrap();
        let mut r = MicRoute::new(cfg, 0).unwrap();
        let mut input = sine(1000.0, 16_000.0, 16_000, 0, 3200);
        input.extend(core::iter::repeat_n(0i16, 3200));
        let batches = run(&mut r, &input, 100, 0);
        assert_eq!(batches.len(), 4, "two 800-sample blocks per batch");
        let mut lv = StdVec::new();
        for (k, b) in batches.iter().enumerate() {
            let b = decode(b).unwrap();
            assert_eq!(b.header.format, SampleFormat::Levels);
            assert_eq!((b.header.first_index, b.header.scale), (k as u64 * 2, 800));
            lv.extend(b.levels());
        }
        for (i, (rms, peak)) in lv.iter().enumerate() {
            let want = audio::levels(&input[i * 800..(i + 1) * 800]);
            assert_eq!((*rms, *peak), (want.rms, want.peak));
        }
        // The tone: RMS about amplitude / √2; silence: zero.
        assert!((11_000..=11_700).contains(&lv[0].0), "{:?}", lv[0]);
        assert!(lv[0].1 >= 15_900);
        assert_eq!(lv[7], (0, 0));
    }

    #[test]
    fn sc8_a_gap_drops_the_partial_batch_and_counts_what_was_lost() {
        let cfg = MicConfig::new(16_000, 0, AudioMode::Pcm, 160, 0).unwrap();
        let mut r = MicRoute::new(cfg, 0).unwrap();
        let mut out = [0u8; MAX_BATCH];
        assert_eq!(r.feed(0, &[1; 100], &mut out), Fed { consumed: 100, batch: None });
        r.gap(50);
        assert_eq!(r.lost(), 150, "100 pending + 50 lost");
        let fed = r.feed(20_000, &[2; 160], &mut out);
        let b = decode(&out[..fed.batch.unwrap()]).unwrap();
        assert_eq!(b.header.first_index, 150, "the receiver sees 150 missing");
        assert!(b.i16_values().all(|v| v == 2));
        r.gap(0);
        assert_eq!(r.lost(), 150, "an empty gap changes nothing");
    }

    #[test]
    fn sc8_level_blocks_realign_after_a_gap() {
        let cfg = MicConfig::new(8_000, 0, AudioMode::Levels, 1, 400).unwrap();
        let mut r = MicRoute::new(cfg, 0).unwrap();
        let mut out = [0u8; MAX_BATCH];
        // 300 samples into block 0, then 150 lost: sample 450, 350 short of
        // block 2's start (800).
        r.feed(0, &[7; 300], &mut out);
        r.gap(150);
        assert_eq!(r.lost(), 450);
        let fed = r.feed(0, &[9; 750], &mut out);
        assert_eq!(fed.consumed, 750);
        let b = decode(&out[..fed.batch.unwrap()]).unwrap();
        assert_eq!(b.header.first_index, 2, "block 2 = samples 800..1200");
        assert_eq!(r.lost(), 800, "the 350 realigning samples count too");
        assert_eq!(b.levels().next(), Some((9, 9)));
    }

    #[test]
    fn sc8_a_feed_stops_at_each_completed_batch() {
        let cfg = MicConfig::new(16_000, 0, AudioMode::Pcm, 160, 0).unwrap();
        let mut r = MicRoute::new(cfg, 0).unwrap();
        let mut out = [0u8; MAX_BATCH];
        let input = [3i16; 400];
        let a = r.feed(0, &input, &mut out);
        assert_eq!((a.consumed, a.batch.is_some()), (160, true));
        let b = r.feed(0, &input[160..], &mut out);
        assert_eq!((b.consumed, b.batch.is_some()), (160, true));
        let c = r.feed(0, &input[320..], &mut out);
        assert_eq!(c, Fed { consumed: 80, batch: None });
    }

    #[test]
    fn sc8_rates_the_board_lacks_are_refused() {
        // Every ABI rate is planned (levels of 10 ms blocks; PCM above
        // 16 kHz cannot fit a 10 ms batch in one message, the ABI refuses
        // it).
        for hz in middleware::wasm_abi::route::MIC_RATES_HZ {
            let cfg = MicConfig::new(hz, 0, AudioMode::Levels, 1, hz / 100).unwrap();
            assert_eq!(MicRoute::new(cfg, 0).unwrap().rate().hz(), u32::from(hz));
        }
        let bad = MicConfig { rate_hz: 11_025, gain_db: 0, mode: AudioMode::Pcm, samples: 441, block: 0 };
        assert!(MicRoute::new(bad, 0).is_err());
    }
}
