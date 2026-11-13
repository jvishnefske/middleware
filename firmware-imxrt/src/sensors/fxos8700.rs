//! The FXOS8700CQ accelerometer and magnetometer (SC4): register plan,
//! data-ready decision and sample conversion.
//!
//! The device is polled on the route's timer: read [`STATUS`]; when
//! [`data_ready`] says so, burst-read [`Config::burst_len`] bytes from
//! [`STATUS`] (the status byte is read again with the data, so the
//! decision and the samples are one transfer once the route runs) and
//! convert them with [`decode`].
//!
//! Register plan ([`Config::plan`], five single-byte writes, in order):
//!
//! | step | register | value |
//! |------|----------|-------|
//! | 1 | `CTRL_REG1` (0x2A) | 0x00: standby, so the others may be written |
//! | 2 | `XYZ_DATA_CFG` (0x0E) | `FS` = 0/1/2 for ±2/4/8 g, high-pass off |
//! | 3 | `M_CTRL_REG1` (0x5B) | hybrid: `M_OS` = 7, `M_HMS` = 3; else 0 (accelerometer only) |
//! | 4 | `M_CTRL_REG2` (0x5C) | hybrid: `M_HYB_AUTOINC_MODE` (0x20); else 0 |
//! | 5 | `CTRL_REG1` (0x2A) | `DR` code << 3, `LNOISE` for ±2/4 g, `ACTIVE` |
//!
//! The hybrid values are the NXP SDK's (`fsl_fxos.c`). Step 3 and 4 are
//! written also without the magnetometer, so a previous hybrid
//! configuration (registers survive a soft reset of the RT1050) does not
//! linger. `F_READ` stays 0: the accelerometer is read at 14 bits.
//!
//! Samples: the accelerometer's 14 bits are left-justified in a
//! big-endian pair (`OUT_X_MSB`, `OUT_X_LSB`); [`decode`] shifts them down
//! to counts (−8192..=8191), so the `i16` sample times
//! [`Range::ug_per_lsb`] is the acceleration in µg (0.244, 0.488 and
//! 0.976 mg per LSB; exactly 1/[`Range::counts_per_g`] g). The
//! magnetometer's 16-bit big-endian values are taken as they are, at
//! [`MAG_NT_PER_LSB`] (0.1 µT per LSB).

/// `DR_STATUS` (with the FIFO off): data-ready and overwrite bits.
pub const STATUS: u8 = 0x00;
/// `OUT_X_MSB`: the first accelerometer data register.
pub const OUT_X_MSB: u8 = 0x01;
/// `WHO_AM_I`: the device identifier.
pub const WHO_AM_I: u8 = 0x0D;
/// The FXOS8700CQ's `WHO_AM_I` value.
pub const WHO_AM_I_VALUE: u8 = 0xC7;
/// `XYZ_DATA_CFG`: accelerometer full-scale range.
pub const XYZ_DATA_CFG: u8 = 0x0E;
/// `CTRL_REG1`: output data rate, low noise, active.
pub const CTRL_REG1: u8 = 0x2A;
/// `M_CTRL_REG1`: magnetometer oversampling and hybrid mode select.
pub const M_CTRL_REG1: u8 = 0x5B;
/// `M_CTRL_REG2`: hybrid auto-increment (burst reads run on from
/// `OUT_Z_LSB` to `M_OUT_X_MSB`).
pub const M_CTRL_REG2: u8 = 0x5C;

/// `DR_STATUS.ZYXDR`: a new X, Y and Z sample set is ready.
pub const ZYXDR: u8 = 1 << 3;
/// `DR_STATUS.ZYXOW`: a sample set was overwritten before it was read.
pub const ZYXOW: u8 = 1 << 7;
/// `CTRL_REG1.ACTIVE`.
const ACTIVE: u8 = 1 << 0;
/// `CTRL_REG1.LNOISE`: reduced noise, valid for ±2 g and ±4 g only.
const LNOISE: u8 = 1 << 2;
/// `M_CTRL_REG1`: `M_OS` = 7 (maximum oversampling for the ODR) and
/// `M_HMS` = 3 (hybrid, accelerometer and magnetometer).
const M_CTRL_REG1_HYBRID: u8 = (7 << 2) | 0b11;
/// `M_CTRL_REG2.M_HYB_AUTOINC_MODE`.
const M_HYB_AUTOINC: u8 = 1 << 5;

/// Magnetometer scale: nanotesla per LSB (0.1 µT).
pub const MAG_NT_PER_LSB: u16 = 100;

/// Whether a `WHO_AM_I` read names an FXOS8700CQ (RT16's probe).
pub const fn is_fxos8700(who_am_i: u8) -> bool {
    who_am_i == WHO_AM_I_VALUE
}

/// The accelerometer's full-scale range (`XYZ_DATA_CFG.FS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    /// ±2 g, 4096 counts per g (0.244 mg per LSB).
    G2,
    /// ±4 g, 2048 counts per g (0.488 mg per LSB).
    G4,
    /// ±8 g, 1024 counts per g (0.976 mg per LSB).
    G8,
}

impl Range {
    /// Every range.
    pub const ALL: [Range; 3] = [Range::G2, Range::G4, Range::G8];

    /// The range for a route configuration's ±g value, or `None`.
    pub const fn from_g(g: u8) -> Option<Range> {
        match g {
            2 => Some(Range::G2),
            4 => Some(Range::G4),
            8 => Some(Range::G8),
            _ => None,
        }
    }

    /// The full scale in g (2, 4 or 8), the range the stream header
    /// carries.
    pub const fn g(&self) -> u8 {
        match self {
            Range::G2 => 2,
            Range::G4 => 4,
            Range::G8 => 8,
        }
    }

    /// The `XYZ_DATA_CFG.FS` field.
    pub const fn fs_bits(&self) -> u8 {
        match self {
            Range::G2 => 0b00,
            Range::G4 => 0b01,
            Range::G8 => 0b10,
        }
    }

    /// Counts per g of a converted sample: the exact scale.
    pub const fn counts_per_g(&self) -> u16 {
        match self {
            Range::G2 => 4096,
            Range::G4 => 2048,
            Range::G8 => 1024,
        }
    }

    /// Micro-g per LSB of a converted sample, the datasheet's nominal
    /// sensitivity (244, 488, 976; exact is 10⁶ / [`Self::counts_per_g`]).
    pub const fn ug_per_lsb(&self) -> u16 {
        match self {
            Range::G2 => 244,
            Range::G4 => 488,
            Range::G8 => 976,
        }
    }

    /// Whether `LNOISE` may be set (the datasheet limits low-noise mode to
    /// ±4 g full scale).
    const fn low_noise(&self) -> bool {
        !matches!(self, Range::G8)
    }
}

/// The output data rate (`CTRL_REG1.DR`). In hybrid mode the device
/// alternates accelerometer and magnetometer, so each rate is half the
/// accelerometer-only rate of the same code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Odr {
    /// `DR` 0: 800 Hz (hybrid 400 Hz).
    Dr0,
    /// `DR` 1: 400 Hz (hybrid 200 Hz).
    Dr1,
    /// `DR` 2: 200 Hz (hybrid 100 Hz).
    Dr2,
    /// `DR` 3: 100 Hz (hybrid 50 Hz).
    Dr3,
    /// `DR` 4: 50 Hz (hybrid 25 Hz).
    Dr4,
    /// `DR` 5: 12.5 Hz (hybrid 6.25 Hz).
    Dr5,
    /// `DR` 6: 6.25 Hz (hybrid 3.125 Hz).
    Dr6,
    /// `DR` 7: 1.5625 Hz (hybrid 0.78125 Hz).
    Dr7,
}

impl Odr {
    /// Every rate code, fastest first.
    pub const ALL: [Odr; 8] = [
        Odr::Dr0,
        Odr::Dr1,
        Odr::Dr2,
        Odr::Dr3,
        Odr::Dr4,
        Odr::Dr5,
        Odr::Dr6,
        Odr::Dr7,
    ];

    /// The `DR` field (0..=7).
    pub const fn code(&self) -> u8 {
        match self {
            Odr::Dr0 => 0,
            Odr::Dr1 => 1,
            Odr::Dr2 => 2,
            Odr::Dr3 => 3,
            Odr::Dr4 => 4,
            Odr::Dr5 => 5,
            Odr::Dr6 => 6,
            Odr::Dr7 => 7,
        }
    }

    /// The sample rate in millihertz, accelerometer only or hybrid.
    pub const fn millihertz(&self, hybrid: bool) -> u32 {
        let single = match self {
            Odr::Dr0 => 800_000,
            Odr::Dr1 => 400_000,
            Odr::Dr2 => 200_000,
            Odr::Dr3 => 100_000,
            Odr::Dr4 => 50_000,
            Odr::Dr5 => 12_500,
            Odr::Dr6 => 6_250,
            Odr::Dr7 => 1_563, // 1.5625 Hz, rounded
        };
        // Hybrid Dr7 is 0.78125 Hz: 1563 / 2 rounds to 781 as well.
        if hybrid { single / 2 } else { single }
    }

    /// The code for a route configuration's whole-hertz rate, or `None`
    /// for a rate the device does not offer in that mode. The fractional
    /// rates are reachable only through the enum.
    pub const fn from_hz(hz: u32, hybrid: bool) -> Option<Odr> {
        let mut i = 0;
        while i < Odr::ALL.len() {
            let odr = Odr::ALL[i];
            let mhz = odr.millihertz(hybrid);
            if mhz.is_multiple_of(1000) && mhz / 1000 == hz {
                return Some(odr);
            }
            i += 1;
        }
        None
    }
}

/// One register write: `(register, value)`.
pub type RegWrite = (u8, u8);

/// An accelerometer route's device configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Output data rate.
    pub odr: Odr,
    /// Accelerometer full-scale range.
    pub range: Range,
    /// Hybrid mode: the magnetometer too (six `i16` per sample, not three).
    pub magnetometer: bool,
}

impl Config {
    /// The register writes that configure the device and make it active,
    /// in order (the table in the module documentation).
    pub const fn plan(&self) -> [RegWrite; 5] {
        let (m1, m2) = if self.magnetometer {
            (M_CTRL_REG1_HYBRID, M_HYB_AUTOINC)
        } else {
            (0, 0)
        };
        let lnoise = if self.range.low_noise() { LNOISE } else { 0 };
        [
            (CTRL_REG1, 0x00),
            (XYZ_DATA_CFG, self.range.fs_bits()),
            (M_CTRL_REG1, m1),
            (M_CTRL_REG2, m2),
            (CTRL_REG1, (self.odr.code() << 3) | lnoise | ACTIVE),
        ]
    }

    /// The register write that stops the device (standby), when the route
    /// stops.
    pub const fn stop() -> RegWrite {
        (CTRL_REG1, 0x00)
    }

    /// `i16` values per sample: 3 (X, Y, Z) or 6 (and magnetometer X, Y, Z).
    pub const fn channels(&self) -> u8 {
        if self.magnetometer { 6 } else { 3 }
    }

    /// The sample rate in millihertz.
    pub const fn millihertz(&self) -> u32 {
        self.odr.millihertz(self.magnetometer)
    }

    /// Bytes of the burst read from [`STATUS`]: the status byte, six bytes
    /// of accelerometer and, in hybrid mode, six of magnetometer.
    pub const fn burst_len(&self) -> usize {
        1 + 2 * self.channels() as usize
    }
}

/// What the status byte says about the next sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// No new sample since the last read; poll again.
    NotReady,
    /// A new sample is ready.
    Ready,
    /// A new sample is ready, and at least one before it was overwritten
    /// unread (the poll is slower than the rate); the stream's
    /// `first_index` should account for the loss.
    Overrun,
}

/// The data-ready decision from [`STATUS`] (`ZYXDR`, `ZYXOW`).
pub const fn data_ready(status: u8) -> Readiness {
    if status & ZYXDR == 0 {
        Readiness::NotReady
    } else if status & ZYXOW != 0 {
        Readiness::Overrun
    } else {
        Readiness::Ready
    }
}

/// One converted sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sample {
    /// Accelerometer X, Y, Z in counts (× [`Range::ug_per_lsb`] µg).
    pub accel: [i16; 3],
    /// Magnetometer X, Y, Z (× [`MAG_NT_PER_LSB`] nT); zero without the
    /// magnetometer.
    pub mag: [i16; 3],
}

impl Sample {
    /// The sample's values in stream order: `channels` (3 or 6) `i16`
    /// written into `out`; returns how many were written (0 when `out` is
    /// too short).
    pub fn write_to(&self, magnetometer: bool, out: &mut [i16]) -> usize {
        let n = if magnetometer { 6 } else { 3 };
        let Some(dst) = out.get_mut(..n) else {
            return 0;
        };
        for (i, d) in dst.iter_mut().enumerate() {
            *d = if i < 3 {
                self.accel[i]
            } else {
                self.mag[i - 3]
            };
        }
        n
    }
}

/// A 14-bit left-justified big-endian accelerometer value to counts.
pub const fn accel_counts(msb: u8, lsb: u8) -> i16 {
    i16::from_be_bytes([msb, lsb]) >> 2
}

/// A 16-bit big-endian magnetometer value.
pub const fn mag_counts(msb: u8, lsb: u8) -> i16 {
    i16::from_be_bytes([msb, lsb])
}

/// Convert a burst read from [`STATUS`] ([`Config::burst_len`] bytes) to
/// a sample, or `None` when the status says no data is ready or the read
/// is shorter than the configuration needs.
pub fn decode(config: &Config, burst: &[u8]) -> Option<Sample> {
    let bytes = burst.get(..config.burst_len())?;
    let (&status, data) = bytes.split_first()?;
    if matches!(data_ready(status), Readiness::NotReady) {
        return None;
    }
    let mut sample = Sample::default();
    for (i, &[msb, lsb]) in data.as_chunks::<2>().0.iter().enumerate() {
        match i {
            0..3 => sample.accel[i] = accel_counts(msb, lsb),
            3..6 => sample.mag[i - 3] = mag_counts(msb, lsb),
            _ => {}
        }
    }
    Some(sample)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sc4_the_plan_is_standby_range_hybrid_then_rate_and_active() {
        let c = Config {
            odr: Odr::Dr1,
            range: Range::G4,
            magnetometer: true,
        };
        // The NXP SDK's fsl_fxos.c init: 200 Hz hybrid, ±4 g.
        assert_eq!(
            c.plan(),
            [
                (0x2A, 0x00),
                (0x0E, 0x01),
                (0x5B, 0x1F),
                (0x5C, 0x20),
                (0x2A, 0x0D)
            ]
        );
        assert_eq!(c.millihertz(), 200_000);
        assert_eq!(c.channels(), 6);
        assert_eq!(c.burst_len(), 13);
        assert_eq!(Config::stop(), (0x2A, 0x00));
    }

    #[test]
    fn sc4_every_rate_and_range_has_its_plan() {
        for odr in Odr::ALL {
            for range in Range::ALL {
                for magnetometer in [false, true] {
                    let c = Config {
                        odr,
                        range,
                        magnetometer,
                    };
                    let p = c.plan();
                    assert_eq!(p[0], (CTRL_REG1, 0));
                    assert_eq!(p[1], (XYZ_DATA_CFG, range.fs_bits()));
                    assert_eq!(p[2].1 & 0b11, if magnetometer { 0b11 } else { 0 });
                    assert_eq!(p[3].1, if magnetometer { 0x20 } else { 0 });
                    let ctrl = p[4].1;
                    assert_eq!(p[4].0, CTRL_REG1);
                    assert_eq!((ctrl >> 3) & 0b111, odr.code());
                    assert_eq!(ctrl & ACTIVE, ACTIVE);
                    assert_eq!(ctrl & LNOISE != 0, range != Range::G8);
                    assert_eq!(ctrl & 0b10, 0, "F_READ stays clear");
                    assert_eq!(c.burst_len(), if magnetometer { 13 } else { 7 });
                }
            }
        }
        assert_eq!(Range::G2.fs_bits(), 0);
        assert_eq!(Range::G8.fs_bits(), 2);
    }

    #[test]
    fn sc4_rates_and_ranges_from_route_configuration() {
        assert_eq!(Odr::from_hz(800, false), Some(Odr::Dr0));
        assert_eq!(Odr::from_hz(50, false), Some(Odr::Dr4));
        assert_eq!(Odr::from_hz(800, true), None);
        assert_eq!(Odr::from_hz(400, true), Some(Odr::Dr0));
        assert_eq!(Odr::from_hz(25, true), Some(Odr::Dr4));
        assert_eq!(Odr::from_hz(25, false), None);
        assert_eq!(Odr::from_hz(12, false), None);
        assert_eq!(Odr::from_hz(0, false), None);
        assert_eq!(Odr::from_hz(u32::MAX, true), None);
        assert_eq!(Odr::Dr5.millihertz(false), 12_500);
        assert_eq!(Odr::Dr6.millihertz(true), 3_125);
        for g in 0..=u8::MAX {
            match Range::from_g(g) {
                Some(r) => assert_eq!(r.g(), g),
                None => assert!(![2, 4, 8].contains(&g)),
            }
        }
    }

    #[test]
    fn sc4_data_ready_comes_from_zyxdr_and_overwrite_from_zyxow() {
        assert_eq!(data_ready(0x00), Readiness::NotReady);
        assert_eq!(data_ready(0x07), Readiness::NotReady); // X/Y/Z DR only
        assert_eq!(data_ready(0x80), Readiness::NotReady); // OW without DR
        assert_eq!(data_ready(0x08), Readiness::Ready);
        assert_eq!(data_ready(0x0F), Readiness::Ready);
        assert_eq!(data_ready(0x88), Readiness::Overrun);
        assert_eq!(data_ready(0xFF), Readiness::Overrun);
        assert!(is_fxos8700(0xC7));
        assert!(!is_fxos8700(0xC6));
        assert_eq!(WHO_AM_I, 0x0D);
    }

    #[test]
    fn sc4_accelerometer_is_14_bit_left_justified_and_magnetometer_16_bit() {
        // +1 g at ±2 g: 4096 counts = 0x1000 << 2 = 0x4000.
        assert_eq!(accel_counts(0x40, 0x00), 4096);
        // Full scale both ways; the low two bits are ignored.
        assert_eq!(accel_counts(0x7F, 0xFC), 8191);
        assert_eq!(accel_counts(0x7F, 0xFF), 8191);
        assert_eq!(accel_counts(0x80, 0x00), -8192);
        assert_eq!(accel_counts(0xFF, 0xFC), -1);
        assert_eq!(accel_counts(0xC0, 0x00), -4096);
        assert_eq!(accel_counts(0x00, 0x04), 1);
        assert_eq!(mag_counts(0x7F, 0xFF), i16::MAX);
        assert_eq!(mag_counts(0x80, 0x00), i16::MIN);
        assert_eq!(mag_counts(0x01, 0xF4), 500); // 50 µT
        // Scale: counts × µg/LSB ≈ 1 g at full scale / range.
        for r in Range::ALL {
            let ug = r.counts_per_g() as u32 * r.ug_per_lsb() as u32;
            assert!((999_000..=1_000_000).contains(&ug), "{r:?}: {ug}");
            assert_eq!(r.counts_per_g() as u32 * r.g() as u32, 8192);
        }
        assert_eq!(MAG_NT_PER_LSB, 100);
    }

    #[test]
    fn sc4_a_burst_decodes_to_samples_and_short_or_unready_reads_do_not() {
        let hybrid = Config {
            odr: Odr::Dr3,
            range: Range::G2,
            magnetometer: true,
        };
        // Flat on the bench: X 0, Y 0, Z +1 g; mag (100, -200, 300).
        let burst = [
            0x0F, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x64, 0xFF, 0x38, 0x01, 0x2C,
        ];
        let s = decode(&hybrid, &burst).unwrap();
        assert_eq!(s.accel, [0, 0, 4096]);
        assert_eq!(s.mag, [100, -200, 300]);
        let mut out = [0i16; 8];
        assert_eq!(s.write_to(true, &mut out), 6);
        assert_eq!(out[..6], [0, 0, 4096, 100, -200, 300]);
        assert_eq!(s.write_to(true, &mut out[..5]), 0);

        let accel_only = Config {
            magnetometer: false,
            ..hybrid
        };
        let s = decode(&accel_only, &burst[..7]).unwrap();
        assert_eq!(s.accel, [0, 0, 4096]);
        assert_eq!(s.mag, [0, 0, 0]);
        assert_eq!(s.write_to(false, &mut out), 3);

        assert_eq!(decode(&hybrid, &burst[..12]), None);
        assert_eq!(decode(&accel_only, &burst[..6]), None);
        assert_eq!(decode(&accel_only, &[]), None);
        let mut unready = burst;
        unready[0] = 0x00;
        assert_eq!(decode(&hybrid, &unready), None);
        // Longer reads are fine (the extra bytes are ignored).
        assert!(decode(&accel_only, &burst).is_some());
    }
}
