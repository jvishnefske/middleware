//! 7-bit node and broadcast addresses, with the I2C and SMBus reserved
//! ranges refused at construction.
//!
//! An [`Address`] is a validated 7-bit address: a node's own target address
//! (from [`Address::node`]) or the bus's broadcast address (from
//! [`Address::broadcast`], which also accepts the general call address
//! `0x00`). Invalid addresses do not construct, so every address the
//! transport puts on the wire is one the specs leave to devices.
//!
//! # Refused addresses
//!
//! * I2C-bus specification (NXP UM10204 rev. 7, §3.1.12, table 4): `0x00`
//!   general call / START byte, `0x01` CBUS, `0x02` other bus formats,
//!   `0x03` future use, `0x04..=0x07` Hs-mode master codes, `0x78..=0x7B`
//!   10-bit addressing, `0x7C..=0x7F` device ID and future use. `0x00` is
//!   allowed as the broadcast address only.
//! * SMBus (System Management Bus specification 2.0, appendix C "SMBus
//!   address assignments"; unchanged in 3.x): `0x08` SMBus host, `0x0C`
//!   alert response address, `0x28` ACCESS.bus host, `0x37` ACCESS.bus
//!   default address, `0x61` SMBus device default address (ARP).
//!
//! Addresses the SMBus specification assigns to device classes (smart
//! battery `0x0B`, its charger `0x09` and selector `0x0A`, the prototype
//! addresses `0x48..=0x4B`, ...) are accepted: on a bus that carries none
//! of those devices they are free. Avoid them on a shared system bus.
//!
//! # Sender ids
//!
//! The transport names a node by its address as the [`SenderId`]
//! `[b'S', b'M', b'B', address, 0, 0, 0, 0]` ([`Address::sender_id`]), so
//! an SMBus node can never be mistaken for a MAC-derived id from another
//! backend, and [`Address::from_sender_id`] maps a
//! [`Target::Node`](crate::transport::Target::Node) back to the address to
//! write to.

use crate::transport::SenderId;

/// The I2C general call address, the default broadcast address.
pub const GENERAL_CALL: u8 = 0x00;

/// The first bytes of every SMBus node's [`SenderId`].
const SENDER_TAG: [u8; 3] = *b"SMB";

/// Why an address cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressError {
    /// The value has bit 7 set: not a 7-bit address.
    NotSevenBit(u8),
    /// The address is reserved by the I2C or SMBus specification.
    Reserved(u8, Reservation),
}

/// Who reserves an address (see the module docs for the sources).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reservation {
    /// I2C `0x00`: general call (accepted as the broadcast address only).
    GeneralCall,
    /// I2C `0x01..=0x07`: CBUS, other bus formats, future use, Hs-mode.
    I2cLow,
    /// I2C `0x78..=0x7F`: 10-bit addressing, device ID, future use.
    I2cHigh,
    /// SMBus `0x08`: the SMBus host.
    SmbusHost,
    /// SMBus `0x0C`: the alert response address.
    AlertResponse,
    /// SMBus `0x28` and `0x37`: ACCESS.bus host and default address.
    AccessBus,
    /// SMBus `0x61`: the device default address used by ARP.
    DeviceDefault,
}

/// What, if anything, reserves the 7-bit `raw` address (pure).
pub const fn reservation(raw: u8) -> Option<Reservation> {
    match raw {
        0x00 => Some(Reservation::GeneralCall),
        0x01..=0x07 => Some(Reservation::I2cLow),
        0x78..=0x7F => Some(Reservation::I2cHigh),
        0x08 => Some(Reservation::SmbusHost),
        0x0C => Some(Reservation::AlertResponse),
        0x28 | 0x37 => Some(Reservation::AccessBus),
        0x61 => Some(Reservation::DeviceDefault),
        _ => None,
    }
}

/// A validated 7-bit bus address (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Address(u8);

impl Address {
    /// The general call address `0x00`, the default broadcast address.
    pub const GENERAL_CALL: Address = Address(GENERAL_CALL);

    /// A node's own target address: any 7-bit address not reserved.
    pub const fn node(raw: u8) -> Result<Address, AddressError> {
        if raw > 0x7F {
            return Err(AddressError::NotSevenBit(raw));
        }
        match reservation(raw) {
            None => Ok(Address(raw)),
            Some(r) => Err(AddressError::Reserved(raw, r)),
        }
    }

    /// A broadcast address: the general call address, or a node address that
    /// every node's target also answers to.
    pub const fn broadcast(raw: u8) -> Result<Address, AddressError> {
        if raw == GENERAL_CALL {
            Ok(Address::GENERAL_CALL)
        } else {
            Address::node(raw)
        }
    }

    /// The 7-bit value.
    pub const fn get(self) -> u8 {
        self.0
    }

    /// The address byte of a write transaction (`address << 1`, R/W̄ = 0):
    /// the first byte the SMBus PEC covers.
    pub const fn write_byte(self) -> u8 {
        self.0 << 1
    }

    /// The node's [`SenderId`] (see the module docs).
    pub const fn sender_id(self) -> SenderId {
        SenderId([
            SENDER_TAG[0],
            SENDER_TAG[1],
            SENDER_TAG[2],
            self.0,
            0,
            0,
            0,
            0,
        ])
    }

    /// The node address a [`SenderId`] names, if it is an SMBus node's id
    /// ([`Address::sender_id`]) of a valid node address.
    pub const fn from_sender_id(id: SenderId) -> Option<Address> {
        let b = id.0;
        if b[0] != SENDER_TAG[0]
            || b[1] != SENDER_TAG[1]
            || b[2] != SENDER_TAG[2]
            || b[4] != 0
            || b[5] != 0
            || b[6] != 0
            || b[7] != 0
        {
            return None;
        }
        match Address::node(b[3]) {
            Ok(a) => Some(a),
            Err(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m1_reserved_ranges_are_refused() {
        for raw in 0x00..=0x07 {
            assert!(matches!(
                Address::node(raw),
                Err(AddressError::Reserved(r, _)) if r == raw
            ));
        }
        for raw in 0x78..=0x7F {
            assert_eq!(
                Address::node(raw),
                Err(AddressError::Reserved(raw, Reservation::I2cHigh))
            );
        }
        let smbus = [
            (0x08, Reservation::SmbusHost),
            (0x0C, Reservation::AlertResponse),
            (0x28, Reservation::AccessBus),
            (0x37, Reservation::AccessBus),
            (0x61, Reservation::DeviceDefault),
        ];
        for (raw, why) in smbus {
            assert_eq!(Address::node(raw), Err(AddressError::Reserved(raw, why)));
        }
        assert_eq!(Address::node(0x80), Err(AddressError::NotSevenBit(0x80)));
        assert_eq!(Address::node(0xFF), Err(AddressError::NotSevenBit(0xFF)));
    }

    #[test]
    fn m1_every_other_seven_bit_address_is_a_node() {
        let mut nodes = 0;
        for raw in 0u8..=0x7F {
            match Address::node(raw) {
                Ok(a) => {
                    assert_eq!(a.get(), raw);
                    assert_eq!(a.write_byte(), raw << 1);
                    assert_eq!(reservation(raw), None);
                    nodes += 1;
                }
                Err(_) => assert!(reservation(raw).is_some()),
            }
        }
        // 128 - 8 low - 8 high - 5 SMBus.
        assert_eq!(nodes, 107);
    }

    #[test]
    fn m1_broadcast_accepts_general_call_and_node_addresses() {
        assert_eq!(Address::broadcast(0x00), Ok(Address::GENERAL_CALL));
        assert_eq!(Address::GENERAL_CALL.write_byte(), 0x00);
        assert_eq!(Address::broadcast(0x70).map(Address::get), Ok(0x70));
        assert!(Address::broadcast(0x01).is_err());
        assert!(Address::broadcast(0x61).is_err());
    }

    #[test]
    fn m1_sender_ids_round_trip_and_reject_foreign_ids() {
        let a = Address::node(0x2A).unwrap();
        let id = a.sender_id();
        assert_eq!(id.0, [b'S', b'M', b'B', 0x2A, 0, 0, 0, 0]);
        assert_eq!(Address::from_sender_id(id), Some(a));
        assert_eq!(
            Address::from_sender_id(SenderId::from_mac([2, 0, 0, 0, 0, 1])),
            None
        );
        // An SMBus-shaped id of a reserved address, or with trailing bytes.
        assert_eq!(
            Address::from_sender_id(SenderId([b'S', b'M', b'B', 0x08, 0, 0, 0, 0])),
            None
        );
        assert_eq!(
            Address::from_sender_id(SenderId([b'S', b'M', b'B', 0x2A, 0, 0, 0, 1])),
            None
        );
    }
}
