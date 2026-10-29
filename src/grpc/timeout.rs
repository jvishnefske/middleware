//! `grpc-timeout` values per the gRPC HTTP/2 spec: [`GrpcTimeout`], an
//! 8-digit value in a [`TimeoutUnit`].
//!
//! The client ([`GrpcTransport`](super::GrpcTransport)) writes one for
//! `RequestOptions::timeout` and the responder
//! ([`ResponderService`](super::ResponderService)) parses the header into
//! `RequestMeta::deadline_ms`. Pure; re-exported by `istio` for
//! interceptors.

use core::time::Duration;

/// Request header carrying the deadline.
pub const GRPC_TIMEOUT_HEADER: &str = "grpc-timeout";

/// Largest `TimeoutValue`: the spec allows at most 8 ASCII digits.
pub const MAX_TIMEOUT_VALUE: u32 = 99_999_999;

/// `TimeoutUnit` of a `grpc-timeout` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutUnit {
    /// `n`
    Nanos,
    /// `u`
    Micros,
    /// `m`
    Millis,
    /// `S`
    Seconds,
    /// `M`
    Minutes,
    /// `H`
    Hours,
}

impl TimeoutUnit {
    /// Finest to coarsest.
    const ALL: [TimeoutUnit; 6] = [
        TimeoutUnit::Nanos,
        TimeoutUnit::Micros,
        TimeoutUnit::Millis,
        TimeoutUnit::Seconds,
        TimeoutUnit::Minutes,
        TimeoutUnit::Hours,
    ];

    /// Length of one unit in nanoseconds.
    pub const fn nanos(self) -> u128 {
        match self {
            TimeoutUnit::Nanos => 1,
            TimeoutUnit::Micros => 1_000,
            TimeoutUnit::Millis => 1_000_000,
            TimeoutUnit::Seconds => 1_000_000_000,
            TimeoutUnit::Minutes => 60_000_000_000,
            TimeoutUnit::Hours => 3_600_000_000_000,
        }
    }

    /// The unit's wire character.
    pub const fn as_char(self) -> char {
        match self {
            TimeoutUnit::Nanos => 'n',
            TimeoutUnit::Micros => 'u',
            TimeoutUnit::Millis => 'm',
            TimeoutUnit::Seconds => 'S',
            TimeoutUnit::Minutes => 'M',
            TimeoutUnit::Hours => 'H',
        }
    }

    /// Inverse of [`TimeoutUnit::as_char`].
    pub const fn from_char(c: char) -> Option<Self> {
        match c {
            'n' => Some(TimeoutUnit::Nanos),
            'u' => Some(TimeoutUnit::Micros),
            'm' => Some(TimeoutUnit::Millis),
            'S' => Some(TimeoutUnit::Seconds),
            'M' => Some(TimeoutUnit::Minutes),
            'H' => Some(TimeoutUnit::Hours),
            _ => None,
        }
    }
}

/// A `grpc-timeout` value: at most [`MAX_TIMEOUT_VALUE`] of a
/// [`TimeoutUnit`]. `Display` renders the wire form, e.g. `250m`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrpcTimeout {
    value: u32,
    unit: TimeoutUnit,
}

impl GrpcTimeout {
    /// Encodes `d` in the finest unit whose value fits in 8 digits. Coarser
    /// units round up so the peer never sees a shorter deadline than asked
    /// for; durations beyond `99999999H` saturate there.
    pub fn from_duration(d: Duration) -> Self {
        let nanos = d.as_nanos();
        for unit in TimeoutUnit::ALL {
            let value = nanos.div_ceil(unit.nanos());
            if let Ok(value) = u32::try_from(value)
                && value <= MAX_TIMEOUT_VALUE
            {
                return Self { value, unit };
            }
        }
        Self {
            value: MAX_TIMEOUT_VALUE,
            unit: TimeoutUnit::Hours,
        }
    }

    /// Parses the wire form; `None` when malformed or over 8 digits.
    pub fn parse(s: &str) -> Option<Self> {
        let unit = TimeoutUnit::from_char(s.chars().last()?)?;
        let digits = s.get(..s.len() - 1)?;
        if digits.is_empty() || digits.len() > 8 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(Self {
            value: digits.parse().ok()?,
            unit,
        })
    }

    /// The numeric part.
    pub const fn value(self) -> u32 {
        self.value
    }

    /// The unit.
    pub const fn unit(self) -> TimeoutUnit {
        self.unit
    }

    /// The deadline this value represents.
    pub fn as_duration(self) -> Duration {
        let nanos = u128::from(self.value) * self.unit.nanos();
        Duration::new(
            u64::try_from(nanos / 1_000_000_000).unwrap_or(u64::MAX),
            // Remainder of a division by 1e9 always fits in u32.
            u32::try_from(nanos % 1_000_000_000).unwrap_or(0),
        )
    }
}

impl core::fmt::Display for GrpcTimeout {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}{}", self.value, self.unit.as_char())
    }
}
