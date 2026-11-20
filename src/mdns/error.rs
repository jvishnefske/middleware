//! [`MdnsError`], the failure of the mDNS shells, generic over the error of
//! their [`Datagram`](crate::datagram::Datagram) seam.

use super::DefaultIo;
use super::announce::AnnounceError;
use super::wire::WireError;
use crate::discovery::RecordError;

/// Failure of an [`MdnsSocket`](super::socket::MdnsSocket) or
/// [`MdnsAnnouncer`](super::announce::MdnsAnnouncer); `E` is the error of
/// their socket (`std::io::Error` for the tokio shells).
#[derive(Debug)]
#[non_exhaustive]
pub enum MdnsError<E = DefaultIo> {
    /// The socket failed.
    Io(E),
    /// A received datagram is not a valid DNS message, or a query could not
    /// be encoded.
    Wire(WireError),
    /// The response needs `needed` bytes but the buffer holds only
    /// `capacity`. The response is kept for
    /// [`MAX_SHORT_BUFFER_RETRIES`](super::socket::MAX_SHORT_BUFFER_RETRIES)
    /// more call.
    BufferTooSmall {
        /// Bytes required.
        needed: usize,
        /// Bytes available.
        capacity: usize,
    },
    /// [`MAX_IGNORED_DATAGRAMS`](super::socket::MAX_IGNORED_DATAGRAMS)
    /// datagrams passed without a response (or, for
    /// [`Browse::next`](crate::discovery::Browse::next), without an event;
    /// for an announcer,
    /// [`MAX_DATAGRAMS_PER_CALL`](super::announce::MAX_DATAGRAMS_PER_CALL)).
    IgnoredLimit,
    /// [`Browse::browse`](crate::discovery::Browse::browse) with a prefix
    /// filter: DNS-SD browses one type.
    UnsupportedFilter,
    /// The kind has no DNS-SD type, or an event does not fit the caller's
    /// buffer (kept as for `BufferTooSmall`).
    Record(RecordError),
    /// The announcer refused a service or lost every rename to conflicts.
    Announce(AnnounceError),
}

impl<E: core::fmt::Display> core::fmt::Display for MdnsError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MdnsError::Io(e) => write!(f, "mdns socket error: {e}"),
            MdnsError::Wire(e) => write!(f, "malformed mdns message: {e:?}"),
            MdnsError::BufferTooSmall { needed, capacity } => {
                write!(f, "buffer too small: need {needed} bytes, have {capacity}")
            }
            MdnsError::IgnoredLimit => f.write_str("too many datagrams without a result"),
            MdnsError::UnsupportedFilter => f.write_str("mDNS browses an exact kind, not a prefix"),
            MdnsError::Record(e) => write!(f, "discovery record: {e:?}"),
            MdnsError::Announce(e) => write!(f, "mdns announce: {e:?}"),
        }
    }
}

impl<E: core::error::Error + 'static> core::error::Error for MdnsError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            MdnsError::Io(e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(feature = "std")]
impl From<std::io::Error> for MdnsError<std::io::Error> {
    fn from(e: std::io::Error) -> Self {
        MdnsError::Io(e)
    }
}

impl<E> From<WireError> for MdnsError<E> {
    fn from(e: WireError) -> Self {
        MdnsError::Wire(e)
    }
}

impl<E> From<AnnounceError> for MdnsError<E> {
    fn from(e: AnnounceError) -> Self {
        MdnsError::Announce(e)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::datagram::fake::FakeErr;
    use core::error::Error;
    use std::string::ToString;

    #[test]
    fn io_carries_the_seam_error_with_its_text_and_source() {
        let e: MdnsError<FakeErr> = MdnsError::Io(FakeErr);
        assert_eq!(e.to_string(), "mdns socket error: fake socket error");
        let source = e.source().expect("source");
        assert_eq!(source.to_string(), "fake socket error");
        let wire: MdnsError<FakeErr> = MdnsError::Wire(WireError::Truncated);
        assert!(wire.source().is_none());
    }

    #[test]
    fn wire_and_announce_errors_convert_for_any_seam() {
        let w: MdnsError<FakeErr> = WireError::Truncated.into();
        assert!(matches!(w, MdnsError::Wire(WireError::Truncated)));
        let a: MdnsError<FakeErr> = AnnounceError::Conflict.into();
        assert!(matches!(a, MdnsError::Announce(AnnounceError::Conflict)));
        assert_eq!(
            a.to_string(),
            std::format!("mdns announce: {:?}", AnnounceError::Conflict)
        );
    }
}
