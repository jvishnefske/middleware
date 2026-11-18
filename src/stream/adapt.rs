//! Stream adapters: sample batches of one shape into another, applied by
//! the runtime before `on_message` (AD1–AD3).
//!
//! A binding ([`crate::loader::binding`]) may put an [`Adapter`] between a
//! channel and its source, so a payload declared for microphone levels can
//! run on a node that publishes PCM, or one declared for 25 Hz IMU data
//! on a 100 Hz stream. Each adapter is a small state machine
//! ([`AdapterState`]) fed one [`super::samples`] batch at a time; it
//! writes zero or one output batch.
//!
//! * [`Adapter::PcmToLevels`]: mono `i16` microphone batches into
//!   RMS/peak level units of `block` samples ([`samples::block_levels`]).
//!   Blocks are aligned to multiples of `block` from sample index 0, as a
//!   levels route's are, and may span input batches. A unit's index is its
//!   first sample's index divided by the block, so a level stream adapted
//!   from PCM carries the same `first_index` as the board's own levels
//!   route would.
//! * [`Adapter::PcmToMuLaw`]: `i16` microphone batches to G.711 µ-law
//!   ([`samples::mulaw_encode`]), unit for unit.
//! * [`Adapter::Decimate`]: keeps the units whose index is a multiple of
//!   `factor` (any sensor and format); output index = input index /
//!   factor, rate = rate / factor (rounded down, at least 1).
//! * [`Adapter::ImuToMagnitude`]: IMU batches to one channel, the
//!   magnitude of the accelerometer vector (saturating at `i16::MAX`).
//!
//! Loss passes through: the input's `first_index` gaps become output
//! gaps, never invented data. A gap inside a level block drops that
//! block (the next unit starts at the next block boundary). Input that
//! does not fit the adapter (another sensor, format or channel count, a
//! malformed batch) is dropped and counted ([`AdapterState::refused`]).

use super::samples::{
    self, Batch, BatchError, BatchHeader, MAX_BATCH_DATA, SampleFormat, SensorKind,
};
pub use crate::loader::binding::Adapter;

/// What a stream looks like: what [`Adapter::output`] maps, and what a
/// channel declaration ([`crate::wasm_abi::channels::Channel`]) checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    /// The sensor.
    pub sensor: SensorKind,
    /// The unit format.
    pub format: SampleFormat,
    /// Channels per unit.
    pub channels: u8,
    /// Units per second (for levels, still the audio rate, as
    /// [`BatchHeader::rate_hz`] says).
    pub rate_hz: u32,
}

impl Shape {
    /// The shape of a batch.
    pub const fn of(h: &BatchHeader) -> Self {
        Shape {
            sensor: h.kind,
            format: h.format,
            channels: h.channels,
            rate_hz: h.rate_hz,
        }
    }
}

impl Adapter {
    /// The shape this adapter turns `input` into, `None` when it does not
    /// take that input.
    pub const fn output(&self, input: Shape) -> Option<Shape> {
        let mic_pcm = matches!(input.sensor, SensorKind::Microphone)
            && matches!(input.format, SampleFormat::I16)
            && input.channels == 1;
        match *self {
            Adapter::PcmToLevels { .. } if mic_pcm => Some(Shape {
                format: SampleFormat::Levels,
                ..input
            }),
            Adapter::PcmToMuLaw if mic_pcm => Some(Shape {
                format: SampleFormat::MuLaw,
                ..input
            }),
            Adapter::Decimate { factor } => Some(Shape {
                rate_hz: decimated_rate(input.rate_hz, factor),
                ..input
            }),
            Adapter::ImuToMagnitude
                if matches!(input.sensor, SensorKind::Imu)
                    && matches!(input.format, SampleFormat::I16) =>
            {
                Some(Shape {
                    channels: 1,
                    ..input
                })
            }
            _ => None,
        }
    }
}

const fn decimated_rate(rate: u32, factor: u16) -> u32 {
    let r = rate / factor as u32;
    if r == 0 { 1 } else { r }
}

/// A running adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterState {
    adapter: Adapter,
    /// Level blocks: the sample index the accumulator expects next.
    next_sample: Option<u64>,
    acc_sum: u64,
    acc_peak: u16,
    acc_n: u32,
    /// Timestamp of the accumulator's first sample.
    acc_ts: u64,
    refused: u32,
}

impl AdapterState {
    /// A fresh adapter.
    pub const fn new(adapter: Adapter) -> Self {
        AdapterState {
            adapter,
            next_sample: None,
            acc_sum: 0,
            acc_peak: 0,
            acc_n: 0,
            acc_ts: 0,
            refused: 0,
        }
    }

    /// The adapter.
    pub const fn adapter(&self) -> Adapter {
        self.adapter
    }

    /// Input batches dropped because they did not fit.
    pub const fn refused(&self) -> u32 {
        self.refused
    }

    /// Feeds one input message; returns the output batch's length in `out`
    /// (at least [`samples::MAX_BATCH`] bytes), or `None` when this input
    /// completed no output (or was refused).
    pub fn feed(&mut self, msg: &[u8], out: &mut [u8]) -> Option<usize> {
        let r = samples::decode(msg).ok().and_then(|b| {
            let input = Shape::of(&b.header);
            self.adapter.output(input)?;
            self.apply(&b, out).ok()
        });
        match r {
            Some(n) => n,
            None => {
                self.refused = self.refused.saturating_add(1);
                None
            }
        }
    }

    fn apply(&mut self, b: &Batch<'_>, out: &mut [u8]) -> Result<Option<usize>, BatchError> {
        let h = b.header;
        let mut data = [0u8; MAX_BATCH_DATA];
        match self.adapter {
            Adapter::PcmToMuLaw => {
                for (o, v) in data.iter_mut().zip(b.i16_values()) {
                    *o = samples::mulaw_encode(v);
                }
                let oh = BatchHeader {
                    format: SampleFormat::MuLaw,
                    ..h
                };
                samples::encode(&oh, &data[..oh.data_len()], out).map(Some)
            }
            Adapter::ImuToMagnitude => {
                let ch = usize::from(h.channels);
                let mut n = 0;
                let mut values = b.i16_values();
                for o in data.chunks_exact_mut(2).take(usize::from(h.count)) {
                    let mut sq = 0u64;
                    for i in 0..ch {
                        let v = i64::from(values.next().unwrap_or(0));
                        if i < 3 {
                            sq += (v * v) as u64;
                        }
                    }
                    let m = u16::try_from(sq.isqrt()).unwrap_or(u16::MAX).min(i16::MAX as u16);
                    o.copy_from_slice(&(m as i16).to_le_bytes());
                    n += 1;
                }
                // One channel, the IMU's range and scale kept.
                let oh = BatchHeader {
                    channels: 1,
                    count: n,
                    ..h
                };
                samples::encode(&oh, &data[..usize::from(n) * 2], out).map(Some)
            }
            Adapter::Decimate { factor } => {
                let f = u64::from(factor);
                let unit = h.unit_bytes();
                let first = h.first_index.div_ceil(f) * f;
                let mut n: u16 = 0;
                let mut at = 0;
                let mut idx = first;
                while idx < h.next_index() {
                    let i = (idx - h.first_index) as usize;
                    data[at..at + unit].copy_from_slice(&b.data[i * unit..(i + 1) * unit]);
                    at += unit;
                    n += 1;
                    idx += f;
                }
                if n == 0 {
                    return Ok(None);
                }
                let skip_us = (first - h.first_index) * 1_000_000 / u64::from(h.rate_hz.max(1));
                let oh = BatchHeader {
                    first_index: first / f,
                    timestamp_us: h.timestamp_us.saturating_add(skip_us),
                    count: n,
                    rate_hz: decimated_rate(h.rate_hz, factor),
                    ..h
                };
                samples::encode(&oh, &data[..at], out).map(Some)
            }
            Adapter::PcmToLevels { block } => self.levels(b, block, &mut data, out),
        }
    }

    fn levels(
        &mut self,
        b: &Batch<'_>,
        block: u16,
        data: &mut [u8; MAX_BATCH_DATA],
        out: &mut [u8],
    ) -> Result<Option<usize>, BatchError> {
        let h = b.header;
        let blk = u64::from(block);
        let rate = u64::from(h.rate_hz.max(1));
        // A gap (or a restart) drops the block in progress.
        if self.next_sample != Some(h.first_index) {
            self.acc_n = 0;
            self.acc_sum = 0;
            self.acc_peak = 0;
        }
        let mut n: u16 = 0;
        let mut at = 0;
        let mut first_unit = None;
        let mut first_ts = 0;
        for (i, v) in b.i16_values().enumerate() {
            let idx = h.first_index + i as u64;
            if self.acc_n == 0 {
                if !idx.is_multiple_of(blk) {
                    continue; // realigning to the next block
                }
                self.acc_ts = h.timestamp_us + i as u64 * 1_000_000 / rate;
            }
            let m = v.unsigned_abs();
            self.acc_sum = self.acc_sum.saturating_add(u64::from(m) * u64::from(m));
            self.acc_peak = self.acc_peak.max(m);
            self.acc_n += 1;
            if self.acc_n == u32::from(block) {
                let rms = u16::try_from((self.acc_sum / blk).isqrt()).unwrap_or(u16::MAX);
                if at + 4 <= data.len() {
                    data[at..at + 2].copy_from_slice(&rms.to_le_bytes());
                    data[at + 2..at + 4].copy_from_slice(&self.acc_peak.to_le_bytes());
                    at += 4;
                    if first_unit.is_none() {
                        first_unit = Some((idx + 1 - blk) / blk);
                        first_ts = self.acc_ts;
                    }
                    n += 1;
                }
                self.acc_n = 0;
                self.acc_sum = 0;
                self.acc_peak = 0;
            }
        }
        self.next_sample = Some(h.next_index());
        let Some(first) = first_unit else {
            return Ok(None);
        };
        let oh = BatchHeader {
            format: SampleFormat::Levels,
            first_index: first,
            timestamp_us: first_ts,
            count: n,
            scale: u32::from(block),
            ..h
        };
        samples::encode(&oh, &data[..at], out).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::samples::{MAX_BATCH, accel_scale_ug};

    fn mic(first: u64, values: &[i16]) -> Vec<u8> {
        let h = BatchHeader {
            kind: SensorKind::Microphone,
            format: SampleFormat::I16,
            channels: 1,
            rate_hz: 16_000,
            first_index: first,
            timestamp_us: first * 1_000_000 / 16_000,
            count: values.len() as u16,
            range: 20,
            scale: 0,
        };
        let mut out = [0u8; MAX_BATCH];
        let n = samples::encode_i16(&h, values, &mut out).unwrap();
        out[..n].to_vec()
    }

    fn feed(a: &mut AdapterState, msg: &[u8]) -> Option<(BatchHeader, Vec<u8>)> {
        let mut out = [0u8; MAX_BATCH];
        let n = a.feed(msg, &mut out)?;
        let h = BatchHeader::parse(&out[..n]).ok()?;
        Some((h, out[samples::HEADER_LEN..n].to_vec()))
    }

    fn levels(data: &[u8]) -> Vec<(u16, u16)> {
        data.chunks_exact(4)
            .map(|b| (u16::from_le_bytes([b[0], b[1]]), u16::from_le_bytes([b[2], b[3]])))
            .collect()
    }

    #[test]
    fn ad1_pcm_to_levels_spans_batches_and_matches_block_levels() {
        let pcm: Vec<i16> = (0..400).map(|i| ((i * 37) % 2000 - 1000) as i16).collect();
        let mut a = AdapterState::new(Adapter::PcmToLevels { block: 160 });
        // 150 + 150 + 100 samples: blocks complete in the 2nd and 3rd.
        assert_eq!(feed(&mut a, &mic(0, &pcm[..150])), None);
        let (h, d) = feed(&mut a, &mic(150, &pcm[150..300])).unwrap();
        assert_eq!((h.format, h.first_index, h.count, h.scale), (SampleFormat::Levels, 0, 1, 160));
        assert_eq!(h.timestamp_us, 0);
        assert_eq!(levels(&d), [samples::block_levels(&pcm[..160])]);
        let (h, d) = feed(&mut a, &mic(300, &pcm[300..400])).unwrap();
        assert_eq!((h.first_index, h.count), (1, 1));
        assert_eq!(h.timestamp_us, 160 * 1_000_000 / 16_000);
        assert_eq!(levels(&d), [samples::block_levels(&pcm[160..320])]);
        assert_eq!(a.refused(), 0);
    }

    #[test]
    fn ad1_a_gap_drops_the_block_in_progress_and_realigns() {
        let pcm = vec![100i16; 200];
        let mut a = AdapterState::new(Adapter::PcmToLevels { block: 100 });
        // Samples 0..50, then 120..200 (70 lost): block 0 is dropped, the
        // next starts at sample 200.
        assert_eq!(feed(&mut a, &mic(0, &pcm[..50])), None);
        assert_eq!(feed(&mut a, &mic(120, &pcm[..80])), None);
        let (h, _) = feed(&mut a, &mic(200, &pcm[..100])).unwrap();
        assert_eq!((h.first_index, h.count), (2, 1));
    }

    #[test]
    fn ad2_mulaw_and_decimation_keep_exact_indices() {
        let pcm: Vec<i16> = (0..10).map(|i| i * 1000).collect();
        let mut a = AdapterState::new(Adapter::PcmToMuLaw);
        let (h, d) = feed(&mut a, &mic(40, &pcm)).unwrap();
        assert_eq!((h.format, h.first_index, h.count), (SampleFormat::MuLaw, 40, 10));
        let want: Vec<u8> = pcm.iter().map(|v| samples::mulaw_encode(*v)).collect();
        assert_eq!(d, want);
        let mut a = AdapterState::new(Adapter::Decimate { factor: 4 });
        // Indices 41..51: keeps 44 and 48 -> output 11, 12.
        let (h, d) = feed(&mut a, &mic(41, &pcm)).unwrap();
        assert_eq!((h.first_index, h.count, h.rate_hz), (11, 2, 4_000));
        assert_eq!(d, [&pcm[3].to_le_bytes()[..], &pcm[7].to_le_bytes()[..]].concat());
        // The input timestamp plus the three skipped samples.
        assert_eq!(h.timestamp_us, 41 * 1_000_000 / 16_000 + 3 * 1_000_000 / 16_000);
        // A batch with no multiple of the factor yields nothing.
        assert_eq!(feed(&mut a, &mic(45, &pcm[..3])), None);
    }

    #[test]
    fn ad2_imu_magnitude_and_refused_input() {
        let h = BatchHeader {
            kind: SensorKind::Imu,
            format: SampleFormat::I16,
            channels: 3,
            rate_hz: 100,
            first_index: 7,
            timestamp_us: 70_000,
            count: 2,
            range: 2,
            scale: accel_scale_ug(2),
        };
        let mut out = [0u8; MAX_BATCH];
        let n = samples::encode_i16(&h, &[3, 4, 0, 30000, 30000, 0], &mut out).unwrap();
        let mut a = AdapterState::new(Adapter::ImuToMagnitude);
        let (oh, d) = feed(&mut a, &out[..n]).unwrap();
        assert_eq!((oh.channels, oh.first_index, oh.count), (1, 7, 2));
        assert_eq!(d, [5i16.to_le_bytes(), i16::MAX.to_le_bytes()].concat());
        // A microphone batch is refused by the IMU adapter, garbage too.
        assert_eq!(feed(&mut a, &mic(0, &[1, 2])), None);
        assert_eq!(a.feed(b"junk", &mut out), None);
        assert_eq!(a.refused(), 2);
    }

    #[test]
    fn ad3_shapes_say_which_adapter_takes_which_stream() {
        let pcm = Shape {
            sensor: SensorKind::Microphone,
            format: SampleFormat::I16,
            channels: 1,
            rate_hz: 16_000,
        };
        let imu = Shape {
            sensor: SensorKind::Imu,
            channels: 3,
            rate_hz: 100,
            ..pcm
        };
        let lv = Adapter::PcmToLevels { block: 160 };
        assert_eq!(lv.output(pcm).map(|s| s.format), Some(SampleFormat::Levels));
        assert_eq!(lv.output(imu), None);
        assert_eq!(Adapter::PcmToMuLaw.output(imu), None);
        assert_eq!(Adapter::ImuToMagnitude.output(imu).map(|s| s.channels), Some(1));
        assert_eq!(Adapter::ImuToMagnitude.output(pcm), None);
        let d = Adapter::Decimate { factor: 4 };
        assert_eq!(d.output(imu).map(|s| s.rate_hz), Some(25));
        let levels = Shape { format: SampleFormat::Levels, ..pcm };
        assert_eq!(lv.output(levels), None);
    }
}
