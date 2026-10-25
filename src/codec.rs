//! Message serialization, independent of transport.
//!
//! A [`Codec`] encodes a value into a caller-provided buffer and decodes it
//! from a byte slice. Implementations:
//!
//! * [`Raw`] — always available; passes byte arrays through unchanged.
//! * `postcard::Postcard` (feature `postcard`) — serde, `no_std`, no heap.
//!   The usual choice for firmware.
//! * `prost::Prost` (feature `prost`) — Protocol Buffers. Interoperates
//!   with gRPC services; prost messages need `alloc`.

#[cfg(feature = "postcard")]
pub mod postcard;
#[cfg(feature = "prost")]
pub mod prost;

/// Converts `T` to and from bytes.
pub trait Codec<T> {
    /// Encode or decode failure.
    type Error: core::fmt::Debug;

    /// Writes `value` into `buf`, returning the number of bytes used.
    fn encode(&self, value: &T, buf: &mut [u8]) -> Result<usize, Self::Error>;

    /// Reads a value from exactly `bytes`.
    fn decode(&self, bytes: &[u8]) -> Result<T, Self::Error>;
}

/// Identity codec for fixed-size byte arrays.
#[derive(Debug, Clone, Copy, Default)]
pub struct Raw;

/// Failure of the [`Raw`] codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawError {
    /// The output buffer was smaller than the value.
    BufferTooSmall,
    /// The input length did not equal the array length.
    LengthMismatch,
}

impl<const N: usize> Codec<[u8; N]> for Raw {
    type Error = RawError;

    fn encode(&self, value: &[u8; N], buf: &mut [u8]) -> Result<usize, RawError> {
        let out = buf.get_mut(..N).ok_or(RawError::BufferTooSmall)?;
        out.copy_from_slice(value);
        Ok(N)
    }

    fn decode(&self, bytes: &[u8]) -> Result<[u8; N], RawError> {
        bytes.try_into().map_err(|_| RawError::LengthMismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_round_trip_and_errors() {
        let mut buf = [0u8; 4];
        assert_eq!(Raw.encode(&[1u8, 2, 3], &mut buf), Ok(3));
        assert_eq!(Raw.decode(&buf[..3]), Ok([1u8, 2, 3]));
        assert_eq!(
            Codec::<[u8; 3]>::decode(&Raw, &buf),
            Err(RawError::LengthMismatch)
        );
        assert_eq!(
            Raw.encode(&[0u8; 5], &mut buf),
            Err(RawError::BufferTooSmall)
        );
    }
}
