//! Distributed-trace context carried with a request: the W3C
//! [`TraceParent`] and the Envoy/Istio [`RequestId`].
//!
//! Both are plain values with no heap, so the `no_std` core carries them in
//! [`RequestMeta`](super::RequestMeta) (what a handler sees) and
//! [`RequestOptions`](super::RequestOptions) (what a caller sends). A
//! backend that can carry them puts them on the wire (gRPC as the
//! `traceparent` and `x-request-id` headers, Zenoh in the query attachment,
//! the memory backend in process); one that cannot (the serial link, BLE,
//! ESP-NOW) leaves them `None` at the responder.
//!
//! # `traceparent`
//!
//! [W3C Trace Context](https://www.w3.org/TR/trace-context/) level 1:
//! `version "-" trace-id "-" parent-id "-" trace-flags`, lowercase hex,
//! 2-32-16-2 digits. [`TraceParent::parse`] follows the spec's receiving
//! rules:
//!
//! * version `00` must be exactly [`TraceParent::LEN`] characters;
//! * version `ff` is invalid;
//! * a higher (future) version is parsed as version `00` when it is at least
//!   [`TraceParent::LEN`] characters and, if longer, continues with `-`;
//!   whatever follows is ignored, and the value is re-emitted as version
//!   `00` by [`TraceParent::format`];
//! * an all-zero trace id or parent id is invalid, as is any uppercase or
//!   non-hex digit.
//!
//! Invalid input gives `None`: the caller then starts a new trace, as the
//! spec requires, rather than propagating garbage. The fields are private
//! so an all-zero id cannot be constructed.
//!
//! # `x-request-id`
//!
//! [`RequestId`] is the request identifier Envoy generates and Istio asks
//! applications to propagate. Accepted values are 1 to
//! [`MAX_REQUEST_ID_LEN`] bytes of visible ASCII (`!` to `~`); anything else
//! is dropped (`None`) rather than truncated or forwarded.

use core::fmt;

/// The longest [`RequestId`] carried, in bytes.
pub const MAX_REQUEST_ID_LEN: usize = 64;

/// A W3C `traceparent`: trace id, parent (span) id and trace flags. Both
/// ids are non-zero by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TraceParent {
    trace_id: [u8; 16],
    span_id: [u8; 8],
    flags: u8,
}

/// The `sampled` bit of [`TraceParent::flags`].
pub const FLAG_SAMPLED: u8 = 0x01;

const HEX: &[u8; 16] = b"0123456789abcdef";

/// The value of one lowercase hex digit.
const fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

/// Decodes `2 * N` lowercase hex digits of `s` starting at `at`.
fn hex_bytes<const N: usize>(s: &[u8], at: usize) -> Option<[u8; N]> {
    let digits = s.get(at..at.checked_add(2 * N)?)?;
    let mut out = [0u8; N];
    for (byte, pair) in out.iter_mut().zip(digits.chunks_exact(2)) {
        let hi = hex_digit(*pair.first()?)?;
        let lo = hex_digit(*pair.get(1)?)?;
        *byte = (hi << 4) | lo;
    }
    Some(out)
}

/// Writes `bytes` as lowercase hex into `out` (which is twice as long).
fn write_hex(out: &mut [u8], bytes: &[u8]) {
    for (pair, b) in out.chunks_exact_mut(2).zip(bytes) {
        if let [hi, lo] = pair {
            *hi = HEX[usize::from(b >> 4)];
            *lo = HEX[usize::from(b & 0x0f)];
        }
    }
}

impl TraceParent {
    /// Length of a formatted version-00 `traceparent`.
    pub const LEN: usize = 55;

    /// A trace parent from its parts; `None` if either id is all zero.
    pub const fn new(trace_id: [u8; 16], span_id: [u8; 8], flags: u8) -> Option<Self> {
        if is_zero(&trace_id) || is_zero(&span_id) {
            return None;
        }
        Some(TraceParent {
            trace_id,
            span_id,
            flags,
        })
    }

    /// Parses a `traceparent` header value (see the module docs for the
    /// rules). Pure.
    pub fn parse(s: &str) -> Option<Self> {
        let b = s.as_bytes();
        let version: [u8; 1] = hex_bytes(b, 0)?;
        match (version[0], b.len()) {
            (0xff, _) => return None,
            (0x00, Self::LEN) => {}
            (0x00, _) => return None,
            (_, Self::LEN) => {}
            (_, n) if n > Self::LEN && b.get(Self::LEN) == Some(&b'-') => {}
            _ => return None,
        }
        if b.get(2) != Some(&b'-') || b.get(35) != Some(&b'-') || b.get(52) != Some(&b'-') {
            return None;
        }
        let trace_id = hex_bytes(b, 3)?;
        let span_id = hex_bytes(b, 36)?;
        let [flags] = hex_bytes::<1>(b, 53)?;
        Self::new(trace_id, span_id, flags)
    }

    /// Formats this value as a version-00 `traceparent` into `buf` and
    /// returns the text.
    pub fn format<'b>(&self, buf: &'b mut [u8; Self::LEN]) -> &'b str {
        buf[0] = b'0';
        buf[1] = b'0';
        buf[2] = b'-';
        write_hex(&mut buf[3..35], &self.trace_id);
        buf[35] = b'-';
        write_hex(&mut buf[36..52], &self.span_id);
        buf[52] = b'-';
        write_hex(&mut buf[53..55], &[self.flags]);
        // Every byte written above is ASCII.
        core::str::from_utf8(buf).unwrap_or("")
    }

    /// The 16-byte trace id, shared by every span of the trace.
    pub const fn trace_id(&self) -> [u8; 16] {
        self.trace_id
    }

    /// The 8-byte id of the span that made the request (W3C `parent-id`).
    pub const fn span_id(&self) -> [u8; 8] {
        self.span_id
    }

    /// The trace flags byte.
    pub const fn flags(&self) -> u8 {
        self.flags
    }

    /// Whether the caller recorded this trace (the `sampled` flag).
    pub const fn is_sampled(&self) -> bool {
        self.flags & FLAG_SAMPLED != 0
    }

    /// The same value with the `sampled` flag set or cleared.
    pub const fn with_sampled(self, sampled: bool) -> Self {
        let flags = if sampled {
            self.flags | FLAG_SAMPLED
        } else {
            self.flags & !FLAG_SAMPLED
        };
        TraceParent { flags, ..self }
    }

    /// The trace parent of a child span `span_id` of this one: same trace
    /// id and flags, the new span as parent. `None` if `span_id` is all
    /// zero.
    pub const fn child(&self, span_id: [u8; 8]) -> Option<Self> {
        Self::new(self.trace_id, span_id, self.flags)
    }
}

impl fmt::Display for TraceParent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buf = [0u8; Self::LEN];
        f.write_str(self.format(&mut buf))
    }
}

const fn is_zero(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0 {
            return false;
        }
        i += 1;
    }
    true
}

/// An `x-request-id`: 1 to [`MAX_REQUEST_ID_LEN`] bytes of visible ASCII,
/// borrowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId<'a>(&'a str);

impl<'a> RequestId<'a> {
    /// `s` as a request id, or `None` if it is empty, longer than
    /// [`MAX_REQUEST_ID_LEN`] or not visible ASCII.
    pub const fn new(s: &'a str) -> Option<Self> {
        if is_valid_request_id(s) {
            Some(RequestId(s))
        } else {
            None
        }
    }

    /// `bytes` as a request id (see [`RequestId::new`]).
    pub fn from_bytes(bytes: &'a [u8]) -> Option<Self> {
        core::str::from_utf8(bytes).ok().and_then(Self::new)
    }

    /// The id text.
    pub const fn as_str(&self) -> &'a str {
        self.0
    }
}

impl fmt::Display for RequestId<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// Whether `s` is an acceptable [`RequestId`]. Pure.
pub const fn is_valid_request_id(s: &str) -> bool {
    let b = s.as_bytes();
    if b.is_empty() || b.len() > MAX_REQUEST_ID_LEN {
        return false;
    }
    let mut i = 0;
    while i < b.len() {
        if !matches!(b[i], b'!'..=b'~') {
            return false;
        }
        i += 1;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example of the W3C Trace Context recommendation, section 3.2.
    const SPEC: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    const SPEC_TRACE: [u8; 16] = [
        0x4b, 0xf9, 0x2f, 0x35, 0x77, 0xb3, 0x4d, 0xa6, 0xa3, 0xce, 0x92, 0x9d, 0x0e, 0x0e, 0x47,
        0x36,
    ];
    const SPEC_SPAN: [u8; 8] = [0x00, 0xf0, 0x67, 0xaa, 0x0b, 0xa9, 0x02, 0xb7];

    #[test]
    fn k17_parses_and_formats_the_spec_examples() {
        let t = TraceParent::parse(SPEC).expect("spec example");
        assert_eq!(t.trace_id(), SPEC_TRACE);
        assert_eq!(t.span_id(), SPEC_SPAN);
        assert_eq!(t.flags(), 0x01);
        assert!(t.is_sampled());
        let mut buf = [0u8; TraceParent::LEN];
        assert_eq!(t.format(&mut buf), SPEC);
        let unsampled = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00";
        let u = TraceParent::parse(unsampled).expect("not sampled");
        assert!(!u.is_sampled());
        assert_eq!(u.format(&mut buf), unsampled);
        assert_eq!(t.with_sampled(false), u);
        assert_eq!(u.with_sampled(true), t);
        // Other flag bits are kept as received.
        let other = TraceParent::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-09")
            .expect("flags 09");
        assert_eq!((other.flags(), other.is_sampled()), (0x09, true));
        assert_eq!(other.with_sampled(false).flags(), 0x08);
    }

    #[test]
    fn k17_rejects_invalid_values() {
        for bad in [
            "",
            "00",
            // all-zero trace id / parent id
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            // version ff is forbidden
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            // uppercase hex
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00F067AA0BA902B7-01",
            "0A-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            // non-hex digits
            "00-4bf92f3577b34da6a3ce929d0e0e473g-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-0x",
            // wrong separators / lengths
            "00_4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736_00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7_01",
            "00-4bf92f3577b34da6a3ce929d0e0e473-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-1",
            // version 00 must not carry more fields
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-abc",
            // a future version continues with '-' only
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01x",
            " 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        ] {
            assert_eq!(TraceParent::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn k17_tolerates_future_versions() {
        let t = TraceParent::parse(SPEC).unwrap();
        for future in [
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "cc-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-what-the-future-holds",
            "fe-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-",
        ] {
            assert_eq!(TraceParent::parse(future), Some(t), "{future:?}");
        }
        // Re-emitted as version 00.
        let mut buf = [0u8; TraceParent::LEN];
        let parsed =
            TraceParent::parse("cc-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-x")
                .unwrap();
        assert_eq!(parsed.format(&mut buf), SPEC);
    }

    #[test]
    fn k17_new_and_child_refuse_zero_ids() {
        assert_eq!(TraceParent::new([0; 16], [1; 8], 0), None);
        assert_eq!(TraceParent::new([1; 16], [0; 8], 0), None);
        let t = TraceParent::new(SPEC_TRACE, SPEC_SPAN, 1).unwrap();
        assert_eq!(Some(t), TraceParent::parse(SPEC));
        let c = t.child([7; 8]).expect("child");
        assert_eq!(
            (c.trace_id(), c.span_id(), c.flags()),
            (SPEC_TRACE, [7; 8], 1)
        );
        assert_eq!(t.child([0; 8]), None);
    }

    #[test]
    fn k17_format_round_trips_every_byte_value() {
        let mut buf = [0u8; TraceParent::LEN];
        for b in 1..=255u8 {
            let t = TraceParent::new([b; 16], [b; 8], b).unwrap();
            assert_eq!(TraceParent::parse(t.format(&mut buf)), Some(t), "{b}");
        }
    }

    #[test]
    fn k17_request_ids_are_visible_ascii_up_to_64_bytes() {
        let ok = "8a1f0c4e-9a3b-4c55-b3c8-2a8f7d0b6e11";
        assert_eq!(RequestId::new(ok).map(|r| r.as_str()), Some(ok));
        let xs = [b'x'; MAX_REQUEST_ID_LEN + 1];
        assert!(RequestId::from_bytes(&xs[..MAX_REQUEST_ID_LEN]).is_some());
        assert!(RequestId::from_bytes(&xs).is_none());
        for bad in ["", "has space", "tab\t", "caf\u{e9}", "nl\n", "\u{7f}"] {
            assert!(RequestId::new(bad).is_none(), "{bad:?}");
        }
        assert_eq!(
            RequestId::from_bytes(b"abc").map(|r| r.as_str()),
            Some("abc")
        );
        assert!(RequestId::from_bytes(&[0xff, 0x41]).is_none());
    }
}
