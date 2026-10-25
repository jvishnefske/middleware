//! [`Prost`]: Protocol Buffers codec, wire compatible with gRPC services.
//!
//! Encoding checks the encoded length first so an undersized buffer is
//! reported as [`ProstError::BufferTooSmall`] rather than a partial write.

use super::Codec;
use prost::Message;

/// Codec for any `prost::Message`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Prost;

/// Failure of the [`Prost`] codec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProstError {
    /// The encoded message needs this many bytes.
    BufferTooSmall {
        /// Bytes required.
        needed: usize,
    },
    /// The input was not a valid encoding.
    Decode(prost::DecodeError),
}

impl<T: Message + Default> Codec<T> for Prost {
    type Error = ProstError;

    fn encode(&self, value: &T, buf: &mut [u8]) -> Result<usize, ProstError> {
        let needed = value.encoded_len();
        let mut out = buf
            .get_mut(..needed)
            .ok_or(ProstError::BufferTooSmall { needed })?;
        value
            .encode(&mut out)
            .map_err(|_| ProstError::BufferTooSmall { needed })?;
        Ok(needed)
    }

    fn decode(&self, bytes: &[u8]) -> Result<T, ProstError> {
        T::decode(bytes).map_err(ProstError::Decode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, PartialEq, Message)]
    struct SetPoint {
        #[prost(uint32, tag = "1")]
        millikelvin: u32,
    }

    #[test]
    fn round_trip() {
        let mut buf = [0u8; 8];
        let m = SetPoint {
            millikelvin: 300_000,
        };
        let n = Prost.encode(&m, &mut buf).unwrap();
        assert_eq!(Codec::<SetPoint>::decode(&Prost, &buf[..n]).unwrap(), m);
    }

    #[test]
    fn small_buffer_reports_needed_length() {
        let mut buf = [0u8; 1];
        let m = SetPoint {
            millikelvin: 300_000,
        };
        assert!(matches!(
            Prost.encode(&m, &mut buf),
            Err(ProstError::BufferTooSmall { needed }) if needed > 1
        ));
    }
}
