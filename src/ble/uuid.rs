//! Deterministic mapping from [`Key`]s to BLE identifiers.
//!
//! Both peers derive identifiers from the key text, so no discovery table is
//! exchanged. A key's 64-bit FNV-1a hash replaces the low eight bytes of
//! [`BASE_UUID`] to form its GATT characteristic UUID, and is folded to 16
//! bits for the advertising id carried in manufacturer data. The hash is
//! [`fnv1a64`], shared with the other backends.
//!
//! A 16-bit id can collide: two keys may share a [`short_id`]. Receivers that
//! resolve ids back to key text detect this when learning keys
//! ([`ShortIdTable`](super::advert::ShortIdTable)).
//!
//! All functions are `const`, so identifiers for literal keys are computed at
//! compile time.

use crate::dedup::fnv1a64;
use crate::key::Key;

/// A 128-bit UUID in big-endian (textual) byte order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Uuid128(pub [u8; 16]);

impl Uuid128 {
    /// The bytes in little-endian order, as BLE puts them on the air.
    pub const fn to_le_bytes(self) -> [u8; 16] {
        let mut out = [0u8; 16];
        let mut i = 0;
        while i < 16 {
            out[i] = self.0[15 - i];
            i += 1;
        }
        out
    }

    /// Inverse of [`Uuid128::to_le_bytes`].
    pub const fn from_le_bytes(le: [u8; 16]) -> Self {
        let mut out = [0u8; 16];
        let mut i = 0;
        while i < 16 {
            out[i] = le[15 - i];
            i += 1;
        }
        Uuid128(out)
    }
}

/// Namespace for every middleware UUID: `4d57-...` ("MW"), randomly chosen.
pub const BASE_UUID: Uuid128 = Uuid128([
    0x4d, 0x57, 0x9a, 0x3e, 0x1c, 0x52, 0x4b, 0x0f, 0, 0, 0, 0, 0, 0, 0, 0,
]);

/// Primary GATT service that hosts every middleware characteristic, and the
/// UUID advertised for discovery.
pub const SERVICE_UUID: Uuid128 = with_low(BASE_UUID, 0);

/// Characteristic carrying request/response traffic for all keys
/// (the key travels in the frame header).
pub const RPC_CHARACTERISTIC_UUID: Uuid128 = with_low(BASE_UUID, 1);

const fn with_low(base: Uuid128, low: u64) -> Uuid128 {
    let mut out = base.0;
    let low = low.to_be_bytes();
    let mut i = 0;
    while i < 8 {
        out[8 + i] = low[i];
        i += 1;
    }
    Uuid128(out)
}

/// The GATT characteristic UUID for publications on `key`. Values 0 and 1 are
/// reserved for [`SERVICE_UUID`] and [`RPC_CHARACTERISTIC_UUID`]; a key hashing
/// there is moved to 2.
pub const fn characteristic_uuid(key: Key<'_>) -> Uuid128 {
    let h = fnv1a64(key.as_bytes());
    with_low(BASE_UUID, if h < 2 { 2 } else { h })
}

/// 16-bit id identifying `key` inside an advertisement.
pub const fn short_id(key: Key<'_>) -> u16 {
    crate::dedup::short_id(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMP: Key<'static> = Key::from_static("sensor/temperature");
    const HUM: Key<'static> = Key::from_static("sensor/humidity");

    #[test]
    fn ids_are_stable_and_distinct() {
        const C: Uuid128 = characteristic_uuid(TEMP);
        assert_eq!(C, characteristic_uuid(TEMP));
        assert_ne!(C, characteristic_uuid(HUM));
        assert_ne!(short_id(TEMP), short_id(HUM));
        assert_eq!(&C.0[..8], &BASE_UUID.0[..8]);
        assert_ne!(C, SERVICE_UUID);
        assert_ne!(C, RPC_CHARACTERISTIC_UUID);
    }

    #[test]
    fn le_round_trip() {
        let u = characteristic_uuid(TEMP);
        assert_eq!(u.to_le_bytes()[15], u.0[0]);
        assert_eq!(Uuid128::from_le_bytes(u.to_le_bytes()), u);
    }
}
