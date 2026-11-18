//! The sample batch wire format: one message of a sensor channel (the IMU
//! and microphone source routes, [`crate::wasm_abi::route::SampleSource`]).
//!
//! Unlike a frame [`chunk`](super::chunk), a batch stands alone: there is
//! no reassembly, and a receiver that misses a message knows exactly how
//! many samples it lost from the next one's `first_index` ([`gap`]).
//!
//! | bytes | field                                                         |
//! |-------|---------------------------------------------------------------|
//! | 1     | version [`VERSION`] (1)                                       |
//! | 1     | [`SensorKind`] (0 = IMU, 1 = microphone)                      |
//! | 1     | [`SampleFormat`] (0 = `i16` LE, 1 = µ-law, 2 = levels)        |
//! | 1     | channels per sample                                           |
//! | 4     | sample rate in Hz, `u32` LE, at least 1                       |
//! | 8     | `first_index`, `u64` LE: units since the route started         |
//! | 8     | timestamp of the first unit in µs, `u64` LE (the board clock)  |
//! | 2     | `count`, `u16` LE: units in this message, at least 1           |
//! | 2     | `range`, `u16` LE: IMU full scale in ±g; microphone gain in dB |
//! | 4     | `scale`, `u32` LE: IMU µg per accelerometer LSB; levels: audio samples per block; otherwise 0 |
//! | rest  | `count` units, exactly                                        |
//!
//! The header is [`HEADER_LEN`] (32) bytes and a whole batch is at most
//! [`MAX_BATCH`] bytes (one ABI message). A *unit* is what `first_index`
//! and `count` count:
//!
//! * [`SampleFormat::I16`]: one sample, `channels` interleaved `i16` LE.
//!   The IMU sends 3 channels (accelerometer X, Y, Z, `scale` µg per LSB)
//!   or 6 (then magnetometer X, Y, Z at [`MAG_LSB_NT`] nT per LSB), or 1:
//!   the magnitude of the acceleration, at `scale` µg per LSB (what the
//!   runtime's `ImuToMagnitude` adapter derives, [`crate::stream::adapt`]);
//!   the microphone sends PCM (`s16le`).
//! * [`SampleFormat::MuLaw`]: one sample, `channels` G.711 µ-law bytes.
//! * [`SampleFormat::Levels`]: one block of `scale` audio samples, per
//!   channel the RMS then the peak magnitude, each `u16` LE in `i16` full
//!   scale (0..=32768); the block rate is `rate / scale`.
//!
//! `first_index` is a `u64`: at 48 kHz a `u32` would wrap after about a
//! day, and the gap arithmetic must never see a wrap. [`decode`] refuses a
//! header that is truncated, has another version, an unknown kind or
//! format, a format or channel count the kind does not have, or a `count`
//! that does not match the bytes after it; nothing here allocates.

pub mod fake;

/// Batch format version.
pub const VERSION: u8 = 1;

/// Size of the fixed header.
pub const HEADER_LEN: usize = 32;

/// Largest batch, header included: one ABI message
/// ([`crate::wasm_abi::MAX_MESSAGE`]).
pub const MAX_BATCH: usize = crate::wasm_abi::MAX_MESSAGE;

/// Largest unit data of one batch.
pub const MAX_BATCH_DATA: usize = MAX_BATCH - HEADER_LEN;

/// Magnetometer resolution of 6-channel IMU samples: 100 nT (0.1 µT) per
/// LSB, the FXOS8700CQ's.
pub const MAG_LSB_NT: u32 = 100;

/// Which sensor produced a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensorKind {
    /// Accelerometer (3 channels) or accelerometer and magnetometer (6).
    Imu = 0,
    /// A microphone (1 or 2 channels).
    Microphone = 1,
}

impl SensorKind {
    /// Parses a kind code.
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(SensorKind::Imu),
            1 => Some(SensorKind::Microphone),
            _ => None,
        }
    }
}

/// How the units of a batch are encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    /// `i16` little-endian per channel (IMU axes, or PCM `s16le`).
    I16 = 0,
    /// G.711 µ-law, one byte per channel (microphone only).
    MuLaw = 1,
    /// RMS and peak per block, two `u16` LE per channel (microphone only).
    Levels = 2,
}

impl SampleFormat {
    /// Parses a format code.
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(SampleFormat::I16),
            1 => Some(SampleFormat::MuLaw),
            2 => Some(SampleFormat::Levels),
            _ => None,
        }
    }

    /// Bytes of one channel of one unit.
    pub const fn channel_bytes(self) -> usize {
        match self {
            SampleFormat::I16 => 2,
            SampleFormat::MuLaw => 1,
            SampleFormat::Levels => 4,
        }
    }
}

/// Why a batch was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchError {
    /// Shorter than [`HEADER_LEN`].
    Truncated,
    /// Unknown format version.
    Version,
    /// Unknown sensor kind or sample format code.
    Unknown,
    /// A format, channel count, range or scale the sensor kind does not
    /// have, or a zero rate.
    Mismatch,
    /// `count` is 0, or the message holds fewer unit bytes than `count`
    /// needs.
    CountPastEnd,
    /// Bytes after the last unit, or a batch above [`MAX_BATCH`].
    Length,
    /// The output buffer cannot hold the batch (encoding only).
    BufferTooSmall,
}

/// The header of one batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchHeader {
    /// The sensor.
    pub kind: SensorKind,
    /// The unit encoding.
    pub format: SampleFormat,
    /// Channels per unit.
    pub channels: u8,
    /// Sample rate in Hz (for levels, the audio rate).
    pub rate_hz: u32,
    /// Units since the route started (the first one is 0).
    pub first_index: u64,
    /// When the first unit was taken, µs on the board clock.
    pub timestamp_us: u64,
    /// Units in this batch.
    pub count: u16,
    /// IMU: full scale in ±g. Microphone: input gain in dB.
    pub range: u16,
    /// IMU: µg per accelerometer LSB. Levels: audio samples per block.
    /// Otherwise 0.
    pub scale: u32,
}

impl BatchHeader {
    /// Bytes of one unit.
    pub const fn unit_bytes(&self) -> usize {
        self.format.channel_bytes() * self.channels as usize
    }

    /// Bytes of the `count` units after the header.
    pub const fn data_len(&self) -> usize {
        self.unit_bytes() * self.count as usize
    }

    /// Bytes of the whole batch.
    pub const fn batch_len(&self) -> usize {
        HEADER_LEN + self.data_len()
    }

    /// Index just past this batch's last unit: what the next batch's
    /// `first_index` is when nothing was lost.
    pub const fn next_index(&self) -> u64 {
        self.first_index.saturating_add(self.count as u64)
    }

    /// Checks the kind, format, channel, rate, range and scale rules of
    /// the module docs, and that the batch fits [`MAX_BATCH`].
    pub const fn check(&self) -> Result<(), BatchError> {
        if self.rate_hz == 0 {
            return Err(BatchError::Mismatch);
        }
        let ok = match (self.kind, self.format) {
            (SensorKind::Imu, SampleFormat::I16) => {
                (self.channels == 1 || self.channels == 3 || self.channels == 6)
                    && self.range != 0
                    && self.scale != 0
            }
            (SensorKind::Imu, _) => false,
            (SensorKind::Microphone, SampleFormat::Levels) => {
                (self.channels == 1 || self.channels == 2) && self.scale != 0
            }
            (SensorKind::Microphone, _) => {
                (self.channels == 1 || self.channels == 2) && self.scale == 0
            }
        };
        if !ok {
            return Err(BatchError::Mismatch);
        }
        if self.count == 0 {
            return Err(BatchError::CountPastEnd);
        }
        if self.batch_len() > MAX_BATCH {
            return Err(BatchError::Length);
        }
        Ok(())
    }

    /// The encoded header, after [`BatchHeader::check`].
    pub const fn encode(&self) -> Result<[u8; HEADER_LEN], BatchError> {
        if let Err(e) = self.check() {
            return Err(e);
        }
        let mut out = [0u8; HEADER_LEN];
        out[0] = VERSION;
        out[1] = self.kind as u8;
        out[2] = self.format as u8;
        out[3] = self.channels;
        put(&mut out, 4, &self.rate_hz.to_le_bytes());
        put(&mut out, 8, &self.first_index.to_le_bytes());
        put(&mut out, 16, &self.timestamp_us.to_le_bytes());
        put(&mut out, 24, &self.count.to_le_bytes());
        put(&mut out, 26, &self.range.to_le_bytes());
        put(&mut out, 28, &self.scale.to_le_bytes());
        Ok(out)
    }

    /// Parses and checks a header (the unit bytes are not looked at).
    pub fn parse(bytes: &[u8]) -> Result<Self, BatchError> {
        let h: &[u8; HEADER_LEN] = bytes
            .get(..HEADER_LEN)
            .and_then(|h| h.try_into().ok())
            .ok_or(BatchError::Truncated)?;
        if h[0] != VERSION {
            return Err(BatchError::Version);
        }
        let header = BatchHeader {
            kind: SensorKind::from_u8(h[1]).ok_or(BatchError::Unknown)?,
            format: SampleFormat::from_u8(h[2]).ok_or(BatchError::Unknown)?,
            channels: h[3],
            rate_hz: u32::from_le_bytes([h[4], h[5], h[6], h[7]]),
            first_index: u64::from_le_bytes([h[8], h[9], h[10], h[11], h[12], h[13], h[14], h[15]]),
            timestamp_us: u64::from_le_bytes([
                h[16], h[17], h[18], h[19], h[20], h[21], h[22], h[23],
            ]),
            count: u16::from_le_bytes([h[24], h[25]]),
            range: u16::from_le_bytes([h[26], h[27]]),
            scale: u32::from_le_bytes([h[28], h[29], h[30], h[31]]),
        };
        header.check()?;
        Ok(header)
    }
}

const fn put<const N: usize>(out: &mut [u8; HEADER_LEN], at: usize, bytes: &[u8; N]) {
    let mut i = 0;
    while i < N {
        out[at + i] = bytes[i];
        i += 1;
    }
}

/// One decoded batch: its header and exactly [`BatchHeader::data_len`]
/// unit bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Batch<'a> {
    /// The header.
    pub header: BatchHeader,
    /// The units, still encoded.
    pub data: &'a [u8],
}

impl<'a> Batch<'a> {
    /// The `i16` values in wire order (unit by unit, channel by channel);
    /// empty unless the format is [`SampleFormat::I16`].
    pub fn i16_values(&self) -> impl Iterator<Item = i16> + 'a {
        let data: &'a [u8] = if self.header.format == SampleFormat::I16 {
            self.data
        } else {
            &[]
        };
        data.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]))
    }

    /// The `(rms, peak)` pairs in wire order (block by block, channel by
    /// channel); empty unless the format is [`SampleFormat::Levels`].
    pub fn levels(&self) -> impl Iterator<Item = (u16, u16)> + 'a {
        let data: &'a [u8] = if self.header.format == SampleFormat::Levels {
            self.data
        } else {
            &[]
        };
        data.chunks_exact(4).map(|b| {
            (
                u16::from_le_bytes([b[0], b[1]]),
                u16::from_le_bytes([b[2], b[3]]),
            )
        })
    }
}

/// Parses one batch message: the header must pass [`BatchHeader::parse`]
/// and the rest must be exactly `count` units.
pub fn decode(msg: &[u8]) -> Result<Batch<'_>, BatchError> {
    let header = BatchHeader::parse(msg)?;
    let data = msg.get(HEADER_LEN..).ok_or(BatchError::Truncated)?;
    let need = header.data_len();
    if data.len() < need {
        return Err(BatchError::CountPastEnd);
    }
    if data.len() > need {
        return Err(BatchError::Length);
    }
    Ok(Batch { header, data })
}

/// Writes `header` and its already encoded unit bytes into `out`; returns
/// the batch length. `data` must be exactly [`BatchHeader::data_len`]
/// bytes (`CountPastEnd` when short, `Length` when long).
pub fn encode(header: &BatchHeader, data: &[u8], out: &mut [u8]) -> Result<usize, BatchError> {
    let head = header.encode()?;
    let need = header.data_len();
    if data.len() < need {
        return Err(BatchError::CountPastEnd);
    }
    if data.len() > need {
        return Err(BatchError::Length);
    }
    let len = header.batch_len();
    let dst = out.get_mut(..len).ok_or(BatchError::BufferTooSmall)?;
    let (h, d) = dst.split_at_mut(HEADER_LEN);
    h.copy_from_slice(&head);
    d.copy_from_slice(data);
    Ok(len)
}

/// Writes `header` and `values` (`count * channels` `i16`, wire order)
/// into `out` for an [`SampleFormat::I16`] batch; returns its length.
pub fn encode_i16(header: &BatchHeader, values: &[i16], out: &mut [u8]) -> Result<usize, BatchError> {
    if header.format != SampleFormat::I16 {
        return Err(BatchError::Mismatch);
    }
    let head = header.encode()?;
    let need = usize::from(header.count) * usize::from(header.channels);
    if values.len() < need {
        return Err(BatchError::CountPastEnd);
    }
    if values.len() > need {
        return Err(BatchError::Length);
    }
    let len = header.batch_len();
    let dst = out.get_mut(..len).ok_or(BatchError::BufferTooSmall)?;
    let (h, d) = dst.split_at_mut(HEADER_LEN);
    h.copy_from_slice(&head);
    for (b, v) in d.chunks_exact_mut(2).zip(values) {
        b.copy_from_slice(&v.to_le_bytes());
    }
    Ok(len)
}

/// What lies between two consecutive batches of one route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gap {
    /// `next` starts exactly where `prev` ended.
    None,
    /// This many units were lost between them.
    Lost(u64),
    /// `next` starts at or before `prev`'s end: a duplicate, a reordered
    /// message or a restarted route; nothing can be said about loss.
    Backwards,
}

/// The [`Gap`] between `prev` and the batch after it, `next`, from their
/// `first_index` and `count`.
pub const fn gap(prev: &BatchHeader, next: &BatchHeader) -> Gap {
    let expected = prev.next_index();
    if next.first_index == expected {
        Gap::None
    } else if next.first_index > expected {
        Gap::Lost(next.first_index - expected)
    } else {
        Gap::Backwards
    }
}

/// The G.711 µ-law byte of a 16-bit linear sample, in the formulation of
/// the reference `g711.c` (`linear2ulaw`): the sample is reduced to
/// G.711's 14-bit range by an arithmetic shift by two, its magnitude
/// clipped to 8159 and biased by 33, and the code is the sign, segment
/// (0..=7) and four mantissa bits, all inverted. 0 encodes to 0xFF, the
/// positive extreme to 0x80, the negative to 0x00. The RT1050's microphone
/// route and the runtime's µ-law adapter both use this one.
pub const fn mulaw_encode(sample: i16) -> u8 {
    const BIAS: i16 = 0x84 >> 2;
    const CLIP: i16 = 8159;
    let mut v = sample >> 2; // −8192..=8191
    let mask: u8 = if v < 0 {
        v = -v; // ≤ 8192, no overflow
        0x7F
    } else {
        0xFF
    };
    if v > CLIP {
        v = CLIP;
    }
    let v = (v + BIAS) as u16; // 33..=8192
    // Segment: how many times v reaches 0x40, 0x80, …, 0x2000.
    let mut seg: u16 = 0;
    while seg < 8 && v >= (0x40 << seg) {
        seg += 1;
    }
    if seg >= 8 {
        return 0x7F ^ mask;
    }
    let code = ((seg << 4) | ((v >> (seg + 1)) & 0x0F)) as u8;
    code ^ mask
}

/// The linear value of a µ-law byte (the G.711 decoder; the inverse of
/// [`mulaw_encode`] up to its quantisation).
pub const fn mulaw_decode(byte: u8) -> i16 {
    let u = !byte;
    let exp = ((u >> 4) & 0x07) as i32;
    let mantissa = (u & 0x0f) as i32;
    let mag = (((mantissa << 3) + 0x84) << exp) - 0x84;
    if u & 0x80 != 0 { -mag as i16 } else { mag as i16 }
}

/// The level of one block of samples: `(rms, peak)` in `i16` full scale
/// (0..=32768), the RMS rounded down. An empty block is `(0, 0)`.
pub fn block_levels(block: &[i16]) -> (u16, u16) {
    let mut sum: u64 = 0;
    let mut peak: u16 = 0;
    for &s in block {
        let m = s.unsigned_abs();
        sum = sum.saturating_add(u64::from(m) * u64::from(m));
        peak = peak.max(m);
    }
    let n = block.len() as u64;
    let rms = if n == 0 { 0 } else { (sum / n).isqrt() };
    (u16::try_from(rms).unwrap_or(u16::MAX), peak)
}

/// µg per LSB of an `i16` accelerometer channel with a full scale of
/// ±`range_g` g: `range_g * 1e6 / 32768`, rounded down (61 at ±2 g).
pub const fn accel_scale_ug(range_g: u16) -> u32 {
    (range_g as u32 * 1_000_000) / 32_768
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMU: BatchHeader = BatchHeader {
        kind: SensorKind::Imu,
        format: SampleFormat::I16,
        channels: 3,
        rate_hz: 100,
        first_index: 0x0102_0304_0506_0708,
        timestamp_us: 1_000_000,
        count: 2,
        range: 2,
        scale: 61,
    };

    const MIC: BatchHeader = BatchHeader {
        kind: SensorKind::Microphone,
        format: SampleFormat::MuLaw,
        channels: 1,
        rate_hz: 16_000,
        first_index: 0,
        timestamp_us: 0,
        count: 4,
        range: 0,
        scale: 0,
    };

    #[test]
    fn sc1_header_layout_is_little_endian() {
        let h = IMU.encode().unwrap();
        assert_eq!(h.len(), HEADER_LEN);
        assert_eq!(&h[..4], &[VERSION, 0, 0, 3]);
        assert_eq!(&h[4..8], &100u32.to_le_bytes());
        assert_eq!(&h[8..16], &[8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(&h[16..24], &1_000_000u64.to_le_bytes());
        assert_eq!(&h[24..26], &[2, 0]);
        assert_eq!(&h[26..28], &[2, 0]);
        assert_eq!(&h[28..32], &61u32.to_le_bytes());
    }

    #[test]
    fn sc1_i16_round_trip() {
        let values = [1i16, -2, 16_384, -32_768, 32_767, 0];
        let mut out = [0u8; MAX_BATCH];
        let n = encode_i16(&IMU, &values, &mut out).unwrap();
        assert_eq!(n, HEADER_LEN + 12);
        assert_eq!(&out[HEADER_LEN..HEADER_LEN + 4], &[1, 0, 0xfe, 0xff]);
        let b = decode(&out[..n]).unwrap();
        assert_eq!(b.header, IMU);
        assert!(b.i16_values().eq(values));
        assert_eq!(b.levels().count(), 0);
    }

    #[test]
    fn sc1_mulaw_and_levels_round_trip() {
        let mut out = [0u8; MAX_BATCH];
        let n = encode(&MIC, &[0xff, 0x7f, 0x00, 0x80], &mut out).unwrap();
        let b = decode(&out[..n]).unwrap();
        assert_eq!((b.header, b.data), (MIC, &[0xff, 0x7f, 0x00, 0x80][..]));
        assert_eq!(b.i16_values().count(), 0);

        let lv = BatchHeader {
            format: SampleFormat::Levels,
            count: 2,
            scale: 256,
            ..MIC
        };
        let n = encode(&lv, &[10, 0, 20, 0, 0, 0x80, 0xff, 0x7f], &mut out).unwrap();
        let b = decode(&out[..n]).unwrap();
        assert!(b.levels().eq([(10, 20), (32_768, 32_767)]));
    }

    #[test]
    fn sc1_truncated_version_and_unknown_codes_are_refused() {
        let mut out = [0u8; MAX_BATCH];
        let n = encode_i16(&IMU, &[0; 6], &mut out).unwrap();
        assert_eq!(decode(&out[..HEADER_LEN - 1]), Err(BatchError::Truncated));
        assert_eq!(decode(&[]), Err(BatchError::Truncated));
        let mut v = out;
        v[0] = 2;
        assert_eq!(decode(&v[..n]), Err(BatchError::Version));
        let mut k = out;
        k[1] = 9;
        assert_eq!(decode(&k[..n]), Err(BatchError::Unknown));
        let mut f = out;
        f[2] = 3;
        assert_eq!(decode(&f[..n]), Err(BatchError::Unknown));
    }

    #[test]
    fn sc1_count_past_the_message_and_trailing_bytes_are_refused() {
        let mut out = [0u8; MAX_BATCH];
        let n = encode_i16(&IMU, &[0; 6], &mut out).unwrap();
        assert_eq!(decode(&out[..n - 1]), Err(BatchError::CountPastEnd));
        assert_eq!(decode(&out[..n + 1]), Err(BatchError::Length));
        let mut c = out;
        c[24] = 3;
        assert_eq!(decode(&c[..n]), Err(BatchError::CountPastEnd));
        c[24] = 0;
        assert_eq!(decode(&c[..n]), Err(BatchError::CountPastEnd), "count 0");
        assert_eq!(
            encode_i16(&IMU, &[0; 5], &mut out),
            Err(BatchError::CountPastEnd)
        );
        assert_eq!(encode_i16(&IMU, &[0; 7], &mut out), Err(BatchError::Length));
        assert_eq!(
            encode_i16(&IMU, &[0; 6], &mut [0u8; HEADER_LEN]),
            Err(BatchError::BufferTooSmall)
        );
        let big = BatchHeader {
            count: u16::MAX,
            ..IMU
        };
        assert_eq!(big.check(), Err(BatchError::Length));
        let most = BatchHeader {
            count: (MAX_BATCH_DATA / 6) as u16,
            ..IMU
        };
        assert_eq!(most.check(), Ok(()));
        assert_eq!(most.batch_len(), MAX_BATCH);
    }

    #[test]
    fn sc1_format_and_channel_mismatches_are_refused() {
        let bad: Result<(), BatchError> = Err(BatchError::Mismatch);
        let cases = [
            BatchHeader { channels: 4, ..IMU },
            BatchHeader { channels: 0, ..IMU },
            BatchHeader { format: SampleFormat::MuLaw, ..IMU },
            BatchHeader { format: SampleFormat::Levels, ..IMU },
            BatchHeader { range: 0, ..IMU },
            BatchHeader { scale: 0, ..IMU },
            BatchHeader { rate_hz: 0, ..IMU },
            BatchHeader { channels: 3, ..MIC },
            BatchHeader { scale: 5, ..MIC },
            BatchHeader { format: SampleFormat::Levels, scale: 0, ..MIC },
        ];
        for h in cases {
            assert_eq!(h.check(), bad, "{h:?}");
        }
        assert_eq!(BatchHeader { channels: 6, ..IMU }.check(), Ok(()));
        assert_eq!(BatchHeader { channels: 2, ..MIC }.check(), Ok(()));
        // A mismatch on the wire is refused the same way.
        let mut out = [0u8; MAX_BATCH];
        let n = encode_i16(&IMU, &[0; 6], &mut out).unwrap();
        out[2] = SampleFormat::MuLaw as u8;
        assert_eq!(decode(&out[..n]), Err(BatchError::Mismatch));
        assert_eq!(encode_i16(&MIC, &[0; 4], &mut out), Err(BatchError::Mismatch));
    }

    #[test]
    fn sc1_gap_reports_lost_samples() {
        let a = IMU;
        let next = BatchHeader {
            first_index: a.first_index + 2,
            ..a
        };
        assert_eq!(gap(&a, &next), Gap::None);
        let later = BatchHeader {
            first_index: a.first_index + 2 + 40,
            ..a
        };
        assert_eq!(gap(&a, &later), Gap::Lost(40));
        assert_eq!(gap(&a, &a), Gap::Backwards);
        let restarted = BatchHeader { first_index: 0, ..a };
        assert_eq!(gap(&a, &restarted), Gap::Backwards);
        assert_eq!(accel_scale_ug(2), 61);
        assert_eq!(accel_scale_ug(8), 244);
    }

    #[test]
    fn sc1_mulaw_matches_g711_and_levels_are_fixed_point() {
        // G.711 reference points (Sun `g711.c`; its arithmetic shift
        // rounds a negative sample toward −∞, so −1 is the smallest
        // negative code, 0x7e, not "−0").
        let table = [
            (0i16, 0xffu8),
            (-1, 0x7e),
            (8, 0xfe),
            (100, 0xf2),
            (-100, 0x72),
            (1000, 0xce),
            (8031, 0xa0),
            (32_767, 0x80),
            (-32_768, 0x00),
        ];
        for (s, b) in table {
            assert_eq!(mulaw_encode(s), b, "{s}");
        }
        for s in [0i16, 50, -300, 1234, -20_000, 32_000] {
            let back = mulaw_decode(mulaw_encode(s));
            let err = (i32::from(back) - i32::from(s)).abs();
            assert!(err <= (i32::from(s).abs() / 16).max(8), "{s} -> {back}");
        }
        assert_eq!(block_levels(&[]), (0, 0));
        assert_eq!(block_levels(&[3, -4, 3, -4]), (3, 4));
        assert_eq!(block_levels(&[i16::MIN; 4]), (32_768, 32_768));
        assert_eq!(block_levels(&[1000, -1000]), (1000, 1000));
    }
}
