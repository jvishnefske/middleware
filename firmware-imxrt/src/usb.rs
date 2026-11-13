//! The USB device controller (ChipIdea, USB OTG1 — `USB1` in the PAC) as
//! pure data and decisions (FL2): the DMA structures it reads and writes,
//! the register values and bit masks the driver writes, and the order of
//! its multi-step protocols.
//!
//! Nothing here touches a register. The imperative shell places the
//! endpoint queue head list and a pool of transfer descriptors in DTCM as
//! `static [AtomicU32; N]`, hands the slices to these functions and does
//! the register pokes they decide. Atomics are interior-mutable, so a
//! write by the controller's DMA is like a write by another thread; every
//! access here is `SeqCst`, which on the Cortex-M7 puts a `DMB` after each
//! store, so the structure is in memory before the shell's next register
//! write (an `ENDPTPRIME`) reaches the controller. The TCM is not cached,
//! so no cache maintenance is needed (that the controller's DMA reaches
//! DTCM is checked on hardware, FL5).
//!
//! * [`dma`] — the endpoint queue head (dQH, 64 bytes, a list of 2 × N at a
//!   2048-byte aligned address) and the transfer descriptor (dTD,
//!   32 bytes): initialisation, encoding, priming and completion decode.
//! * [`regs`] — register values and bits: `USBMODE`, `USBCMD`, `USBSTS`,
//!   `DEVICEADDR`, `ENDPTLISTADDR`, `PORTSC1` speed, `ENDPTCTRLn` and the
//!   per-endpoint bits of `ENDPTPRIME`, `ENDPTFLUSH`, `ENDPTSTAT`,
//!   `ENDPTCOMPLETE` and `ENDPTSETUPSTAT`.
//! * [`control`] — the setup packet, the setup tripwire (`SUTW`) protocol
//!   for copying it out of the queue head, and the bus reset plan.
//!
//! The transfer model is the one embassy-usb drives: at most one dTD
//! outstanding per endpoint direction, primed while that endpoint is idle
//! (the reference manual's "linked list empty" case), so the add-dTD
//! tripwire (`ATDTW`) is never needed. A dTD carries at most one packet
//! read or write of the class, well under [`dma::DTD_MAX_BYTES`].
//!
//! Zero-length packets: the controller's automatic zero-length termination
//! is disabled in every queue head (`ZLT` set), and the CDC link decides
//! as the RP base does ([`needs_zlp`]): a flush after a full packet sends
//! a zero-length one. High-speed bulk endpoints carry [`HS_BULK_MAX_PACKET`]
//! bytes per packet.
//!
//! Register bit positions follow the i.MX RT1050 reference manual's USB
//! chapter and NXP's SDK driver (`usb_device_ehci.c`); each module lists
//! the positions it assumes, to be checked on hardware.

pub mod control;
pub mod dma;
pub mod regs;

/// Endpoints the RT1050's controller implements (`DCCPARAMS.DEN`): 0 to 7.
pub const ENDPOINTS: u8 = 8;
/// Bulk packet size at high speed.
pub const HS_BULK_MAX_PACKET: u16 = 512;
/// Bulk packet size at full speed.
pub const FS_BULK_MAX_PACKET: u16 = 64;
/// Control endpoint 0 packet size (both speeds).
pub const EP0_MAX_PACKET: u16 = 64;

/// Whether a flush must send a zero-length packet: the last packet written
/// since the previous flush was a full one. The RP base's rule
/// (`rp_base::usb::needs_zlp`), copied because that crate is not a
/// dependency of this one.
pub const fn needs_zlp(last_packet: usize, max_packet: u16) -> bool {
    last_packet == max_packet as usize
}

/// Why a USB structure or value was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// An endpoint number the controller does not implement (8 or more).
    NoSuchEndpoint,
    /// The DMA memory slice is too short for the structure.
    ShortMemory,
    /// An address not aligned as the structure requires (dTD 32 bytes,
    /// queue head list 2048 bytes).
    Misaligned,
    /// More bytes than one dTD can carry from this buffer address, or a
    /// max packet length past the 11-bit field.
    TooLong,
    /// A device address past 127.
    BadAddress,
}

/// Transfer direction, as USB names it (from the host's point of view).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    /// Host to device (the controller's "receive", `RX`).
    Out,
    /// Device to host (the controller's "transmit", `TX`).
    In,
}

/// One direction of one endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ep {
    number: u8,
    dir: Dir,
}

impl Ep {
    /// Endpoint 0 OUT (receives setup packets).
    pub const EP0_OUT: Ep = Ep { number: 0, dir: Dir::Out };
    /// Endpoint 0 IN.
    pub const EP0_IN: Ep = Ep { number: 0, dir: Dir::In };

    /// Endpoint `number` in direction `dir`, if the controller has it.
    pub const fn new(number: u8, dir: Dir) -> Result<Ep, Error> {
        if number < ENDPOINTS {
            Ok(Ep { number, dir })
        } else {
            Err(Error::NoSuchEndpoint)
        }
    }

    /// From a USB endpoint address byte (bit 7 IN, bits 3:0 the number).
    /// Bits 6:4 must be zero.
    pub const fn from_address(addr: u8) -> Result<Ep, Error> {
        if addr & 0x70 != 0 {
            return Err(Error::NoSuchEndpoint);
        }
        let dir = if addr & 0x80 != 0 { Dir::In } else { Dir::Out };
        Ep::new(addr & 0x0F, dir)
    }

    /// The endpoint number, 0 to 7.
    pub const fn number(self) -> u8 {
        self.number
    }

    /// The direction.
    pub const fn dir(self) -> Dir {
        self.dir
    }

    /// The USB endpoint address byte.
    pub const fn address(self) -> u8 {
        match self.dir {
            Dir::Out => self.number,
            Dir::In => self.number | 0x80,
        }
    }

    /// Index of this endpoint's queue head in the list: `2n` for OUT,
    /// `2n + 1` for IN.
    pub const fn qh_index(self) -> usize {
        let base = self.number as usize * 2;
        match self.dir {
            Dir::Out => base,
            Dir::In => base + 1,
        }
    }

    /// This endpoint's bit in `ENDPTPRIME`, `ENDPTFLUSH`, `ENDPTSTAT` and
    /// `ENDPTCOMPLETE`: bit `n` for OUT (receive), bit `16 + n` for IN
    /// (transmit).
    pub const fn bit(self) -> u32 {
        match self.dir {
            Dir::Out => 1 << self.number,
            Dir::In => 1 << (16 + self.number as u32),
        }
    }
}

/// A set of endpoint bits as `ENDPTCOMPLETE` (or `ENDPTPRIME`,
/// `ENDPTFLUSH`, `ENDPTSTAT`) reports them; bits of endpoints the
/// controller does not have are ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpMask(pub u32);

impl EpMask {
    /// The bits of every implemented endpoint, both directions.
    pub const ALL: EpMask = EpMask(0x00FF_00FF);

    /// Whether `ep`'s bit is set.
    pub const fn contains(self, ep: Ep) -> bool {
        self.0 & ep.bit() != 0
    }

    /// The endpoints whose bits are set: OUT 0 to 7, then IN 0 to 7.
    pub const fn iter(self) -> EpIter {
        EpIter { mask: self.0 & EpMask::ALL.0, next: 0 }
    }
}

/// Iterator over an [`EpMask`]'s endpoints.
#[derive(Debug, Clone)]
pub struct EpIter {
    mask: u32,
    next: u8,
}

impl Iterator for EpIter {
    type Item = Ep;

    fn next(&mut self) -> Option<Ep> {
        while self.next < 2 * ENDPOINTS {
            let i = self.next;
            self.next += 1;
            let ep = if i < ENDPOINTS {
                Ep { number: i, dir: Dir::Out }
            } else {
                Ep { number: i - ENDPOINTS, dir: Dir::In }
            };
            if self.mask & ep.bit() != 0 {
                return Some(ep);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fl2_a_frame_ending_on_a_full_packet_gets_a_zero_length_packet() {
        assert!(needs_zlp(512, HS_BULK_MAX_PACKET));
        assert!(!needs_zlp(511, HS_BULK_MAX_PACKET));
        assert!(!needs_zlp(0, HS_BULK_MAX_PACKET), "nothing written since the last flush");
        // The RP base's own cases, at its 64-byte full-speed packet.
        assert!(needs_zlp(64, FS_BULK_MAX_PACKET));
        assert!(!needs_zlp(63, FS_BULK_MAX_PACKET));
        assert!(!needs_zlp(0, FS_BULK_MAX_PACKET));
    }

    #[test]
    fn fl2_endpoint_bits_are_rx_low_and_tx_high() {
        let out2 = Ep::new(2, Dir::Out).unwrap();
        let in2 = Ep::new(2, Dir::In).unwrap();
        assert_eq!(out2.bit(), 1 << 2);
        assert_eq!(in2.bit(), 1 << 18);
        assert_eq!(Ep::EP0_OUT.bit(), 1);
        assert_eq!(Ep::EP0_IN.bit(), 1 << 16);
        assert_eq!(Ep::new(7, Dir::In).unwrap().bit(), 1 << 23);
    }

    #[test]
    fn fl2_queue_heads_are_out_then_in_per_endpoint() {
        assert_eq!(Ep::EP0_OUT.qh_index(), 0);
        assert_eq!(Ep::EP0_IN.qh_index(), 1);
        assert_eq!(Ep::new(3, Dir::Out).unwrap().qh_index(), 6);
        assert_eq!(Ep::new(3, Dir::In).unwrap().qh_index(), 7);
    }

    #[test]
    fn fl2_endpoints_past_seven_and_bad_addresses_are_refused() {
        assert_eq!(Ep::new(8, Dir::Out), Err(Error::NoSuchEndpoint));
        assert_eq!(Ep::from_address(0x88), Err(Error::NoSuchEndpoint));
        assert_eq!(Ep::from_address(0x91), Err(Error::NoSuchEndpoint));
        assert_eq!(Ep::from_address(0x81), Ep::new(1, Dir::In));
        assert_eq!(Ep::from_address(0x02), Ep::new(2, Dir::Out));
        assert_eq!(Ep::new(5, Dir::In).unwrap().address(), 0x85);
        assert_eq!(Ep::new(5, Dir::Out).unwrap().address(), 0x05);
    }

    #[test]
    fn fl2_complete_mask_lists_its_endpoints_and_ignores_unimplemented_bits() {
        let mut it = EpMask((1 << 1) | (1 << 17) | (1 << 8) | (1 << 31)).iter();
        assert_eq!(it.next(), Ep::new(1, Dir::Out).ok());
        assert_eq!(it.next(), Ep::new(1, Dir::In).ok());
        assert_eq!(it.next(), None);
        assert_eq!(EpMask::ALL.iter().count(), 16);
        assert!(EpMask(1 << 16).contains(Ep::EP0_IN));
        assert!(!EpMask(1 << 16).contains(Ep::EP0_OUT));
    }
}
