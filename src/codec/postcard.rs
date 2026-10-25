//! [`Postcard`]: serde-based compact binary codec, `no_std` and heap free.

use super::Codec;
use serde::{Serialize, de::DeserializeOwned};

/// Codec for any `Serialize + DeserializeOwned` type using postcard.
#[derive(Debug, Clone, Copy, Default)]
pub struct Postcard;

impl<T: Serialize + DeserializeOwned> Codec<T> for Postcard {
    type Error = postcard::Error;

    fn encode(&self, value: &T, buf: &mut [u8]) -> Result<usize, Self::Error> {
        postcard::to_slice(value, buf).map(|used| used.len())
    }

    fn decode(&self, bytes: &[u8]) -> Result<T, Self::Error> {
        postcard::from_bytes(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Reading {
        millikelvin: u32,
        sensor: u8,
    }

    #[test]
    fn round_trip() {
        let mut buf = [0u8; 16];
        let r = Reading {
            millikelvin: 295_150,
            sensor: 3,
        };
        let n = Postcard.encode(&r, &mut buf).unwrap();
        assert_eq!(Codec::<Reading>::decode(&Postcard, &buf[..n]).unwrap(), r);
    }

    #[test]
    fn small_buffer_is_an_error() {
        let mut buf = [0u8; 1];
        let r = Reading {
            millikelvin: 295_150,
            sensor: 3,
        };
        assert!(Postcard.encode(&r, &mut buf).is_err());
    }
}
