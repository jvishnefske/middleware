//! KSZ8081RNB PHY decisions: identity, reset, the 50 MHz RMII clock mode,
//! auto-negotiation and the link status.
//!
//! The registers are the IEEE clause-22 ones (BMCR, BMSR, PHY identifier)
//! and two vendor registers: PHY control 1 (0x1E), whose bits 2:0 report
//! the negotiated operation mode, and PHY control 2 (0x1F), whose bit 7
//! selects the 50 MHz RMII reference clock the EVKB's RT1050 drives.
//! A software reset (BMCR bit 15) returns PHY control 2 to its default, so
//! the clock mode is set after the reset, then auto-negotiation started.

/// The PHY's MDIO address on the EVKB (PHYAD straps).
pub const PHY_ADDR: u8 = 2;

/// Basic control register (BMCR).
pub const REG_BMCR: u8 = 0x00;
/// Basic status register (BMSR).
pub const REG_BMSR: u8 = 0x01;
/// PHY identifier 1 (OUI bits 3:18).
pub const REG_PHY_ID1: u8 = 0x02;
/// PHY identifier 2 (OUI bits 19:24, model, revision).
pub const REG_PHY_ID2: u8 = 0x03;
/// PHY control 1 (vendor): operation mode indication in bits 2:0.
pub const REG_PHY_CTRL1: u8 = 0x1E;
/// PHY control 2 (vendor): RMII reference clock select in bit 7.
pub const REG_PHY_CTRL2: u8 = 0x1F;

/// BMCR software reset (self-clearing).
pub const BMCR_RESET: u16 = 1 << 15;
/// BMCR speed select (100 Mbps) when auto-negotiation is off.
pub const BMCR_SPEED_100: u16 = 1 << 13;
/// BMCR auto-negotiation enable.
pub const BMCR_AUTONEG_ENABLE: u16 = 1 << 12;
/// BMCR power down.
pub const BMCR_POWER_DOWN: u16 = 1 << 11;
/// BMCR electrical isolation from RMII.
pub const BMCR_ISOLATE: u16 = 1 << 10;
/// BMCR restart auto-negotiation (self-clearing).
pub const BMCR_RESTART_AUTONEG: u16 = 1 << 9;
/// BMCR full duplex when auto-negotiation is off.
pub const BMCR_FULL_DUPLEX: u16 = 1 << 8;

/// BMSR link status (latches low: a link loss reads 0 once even if the
/// link is back).
pub const BMSR_LINK: u16 = 1 << 2;
/// BMSR auto-negotiation complete.
pub const BMSR_AUTONEG_COMPLETE: u16 = 1 << 5;

/// PHY control 2 bit 7: the RMII reference clock is 50 MHz.
pub const CTRL2_RMII_50MHZ: u16 = 1 << 7;
/// PHY control 1 bits 2:0: the operation mode indication.
pub const CTRL1_OP_MODE_MASK: u16 = 0b111;

/// PHY identifier 1 of the KSZ8081 (Micrel/Microchip OUI).
pub const PHY_ID1: u16 = 0x0022;
/// PHY identifier 2 of the KSZ8081RNB as read on the EVKB (revision 1).
pub const PHY_ID2: u16 = 0x1561;
/// The model bits of PHY identifier 2 (bits 3:0 are the revision).
pub const PHY_ID2_MODEL_MASK: u16 = 0xFFF0;

/// The BMCR value that starts a software reset.
pub const RESET_COMMAND: u16 = BMCR_RESET;
/// BMCR reads the shell makes after [`RESET_COMMAND`] before the reset
/// counts as failed, at least 1 ms apart (the datasheet's reset completes
/// well within that; not exercised by the probe, which only used the
/// hardware reset pin).
pub const RESET_POLL_BOUND: u32 = 500;
/// The BMCR value that enables and (re)starts auto-negotiation, 0x1200 as
/// the probe wrote it (advertising the strapped defaults: 10/100, half and
/// full duplex).
pub const AUTONEG_START: u16 = BMCR_AUTONEG_ENABLE | BMCR_RESTART_AUTONEG;

/// How long the shell holds the PHY's reset pin (GPIO1_IO09) low.
pub const HW_RESET_LOW_MS: u32 = 10;
/// How long the shell waits after releasing the reset pin before MDIO.
pub const HW_RESET_SETTLE_MS: u32 = 100;

/// Why the PHY was not accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhyError {
    /// The identifier read as all ones or all zeros: nothing answers at
    /// the address (MDIO floating or held), or the transfer failed.
    NoPhy,
    /// A PHY answered, but not a KSZ8081.
    WrongPhy {
        /// PHY identifier 1 as read.
        id1: u16,
        /// PHY identifier 2 as read.
        id2: u16,
    },
}

/// Checks the PHY identifier registers; the KSZ8081's silicon revision
/// (bits 3:0 of identifier 2) when they match.
pub const fn identify(id1: u16, id2: u16) -> Result<u8, PhyError> {
    if id1 == 0xFFFF || (id1 == 0 && id2 == 0) {
        return Err(PhyError::NoPhy);
    }
    if id1 == PHY_ID1 && id2 & PHY_ID2_MODEL_MASK == PHY_ID2 & PHY_ID2_MODEL_MASK {
        Ok((id2 & 0xF) as u8)
    } else {
        Err(PhyError::WrongPhy { id1, id2 })
    }
}

/// What to do after a BMCR read during the software reset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetStep {
    /// The reset bit cleared: go on.
    Done,
    /// Still resetting: wait and read BMCR again.
    Poll,
    /// [`RESET_POLL_BOUND`] reads and still resetting, or the read failed.
    Failed,
}

/// Decides the next reset step from the BMCR read `bmcr` (`None` for a
/// failed MDIO transfer) after `reads` BMCR reads, this one included.
pub const fn reset_step(bmcr: Option<u16>, reads: u32) -> ResetStep {
    match bmcr {
        None => ResetStep::Failed,
        Some(v) if v & BMCR_RESET == 0 => ResetStep::Done,
        Some(_) if reads >= RESET_POLL_BOUND => ResetStep::Failed,
        Some(_) => ResetStep::Poll,
    }
}

/// PHY control 2 with the 50 MHz RMII reference clock selected, from its
/// current value `current` (a read-modify-write, as the probe did).
pub const fn ctrl2_rmii_50mhz(current: u16) -> u16 {
    current | CTRL2_RMII_50MHZ
}

/// Whether a PHY control 2 read has the 50 MHz RMII reference selected.
pub const fn is_rmii_50mhz(ctrl2: u16) -> bool {
    ctrl2 & CTRL2_RMII_50MHZ != 0
}

/// Link speed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speed {
    /// 10BASE-T.
    Mbps10,
    /// 100BASE-TX.
    Mbps100,
}

/// Link duplex.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Duplex {
    /// Half duplex.
    Half,
    /// Full duplex.
    Full,
}

/// A link's speed and duplex.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Link {
    /// The speed.
    pub speed: Speed,
    /// The duplex.
    pub duplex: Duplex,
}

impl Link {
    /// 100BASE-TX full duplex, what the EVKB's link came up as on the probe.
    pub const FAST_FULL: Link = Link {
        speed: Speed::Mbps100,
        duplex: Duplex::Full,
    };
}

/// The link state from a BMSR read and a PHY control 1 read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkState {
    /// No link (BMSR link status clear).
    Down,
    /// Link status set but auto-negotiation still running (operation
    /// mode 000).
    Negotiating,
    /// Link up at this speed and duplex.
    Up(Link),
    /// Link status set with a reserved operation mode (the value).
    Unknown(u8),
}

/// Decodes the link from BMSR `bmsr` and PHY control 1 `ctrl1`. BMSR's
/// link bit latches low, so the shell reads BMSR twice and passes the
/// second read when it wants the current state.
pub const fn link_state(bmsr: u16, ctrl1: u16) -> LinkState {
    if bmsr & BMSR_LINK == 0 {
        return LinkState::Down;
    }
    let op = (ctrl1 & CTRL1_OP_MODE_MASK) as u8;
    let (speed, duplex) = match op {
        0b000 => return LinkState::Negotiating,
        0b001 => (Speed::Mbps10, Duplex::Half),
        0b010 => (Speed::Mbps100, Duplex::Half),
        0b101 => (Speed::Mbps10, Duplex::Full),
        0b110 => (Speed::Mbps100, Duplex::Full),
        other => return LinkState::Unknown(other),
    };
    LinkState::Up(Link { speed, duplex })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fl3_phy_id_check() {
        // As read on the EVKB.
        assert_eq!(identify(0x0022, 0x1561), Ok(1));
        assert_eq!(identify(0x0022, 0x1560), Ok(0));
        assert_eq!(identify(0x0022, 0x156F), Ok(15));
        assert_eq!(identify(0xFFFF, 0xFFFF), Err(PhyError::NoPhy));
        assert_eq!(identify(0, 0), Err(PhyError::NoPhy));
        assert_eq!(identify(0xFFFF, 0x1561), Err(PhyError::NoPhy));
        // KSZ8091 / another model, and a different OUI.
        assert_eq!(
            identify(0x0022, 0x1571),
            Err(PhyError::WrongPhy {
                id1: 0x0022,
                id2: 0x1571
            })
        );
        assert_eq!(
            identify(0x0007, 0xC0F1),
            Err(PhyError::WrongPhy {
                id1: 0x0007,
                id2: 0xC0F1
            })
        );
    }

    #[test]
    fn fl3_phy_register_numbers() {
        assert_eq!((PHY_ADDR, REG_PHY_CTRL1, REG_PHY_CTRL2), (2, 0x1E, 0x1F));
        assert_eq!((REG_BMCR, REG_BMSR, REG_PHY_ID1, REG_PHY_ID2), (0, 1, 2, 3));
    }

    #[test]
    fn fl3_phy_reset_sequence() {
        assert_eq!(RESET_COMMAND, 0x8000);
        assert_eq!(reset_step(Some(0x8000), 1), ResetStep::Poll);
        assert_eq!(reset_step(Some(0x3100), 2), ResetStep::Done);
        assert_eq!(
            reset_step(Some(0x8000), RESET_POLL_BOUND - 1),
            ResetStep::Poll
        );
        assert_eq!(
            reset_step(Some(0x8000), RESET_POLL_BOUND),
            ResetStep::Failed
        );
        assert_eq!(reset_step(Some(0x8000), u32::MAX), ResetStep::Failed);
        assert_eq!(reset_step(Some(0x0000), u32::MAX), ResetStep::Done);
        assert_eq!(reset_step(None, 1), ResetStep::Failed);
    }

    #[test]
    fn fl3_phy_rmii_50mhz_clock_mode() {
        assert_eq!(ctrl2_rmii_50mhz(0x0000), 0x0080);
        // KSZ8081 control 2 default (0x8100 region): other bits kept.
        assert_eq!(ctrl2_rmii_50mhz(0x8100), 0x8180);
        assert_eq!(ctrl2_rmii_50mhz(0x0080), 0x0080);
        assert!(is_rmii_50mhz(ctrl2_rmii_50mhz(0x1234)));
        assert!(!is_rmii_50mhz(0xFF7F));
    }

    #[test]
    fn fl3_phy_autoneg_start_is_probe_value() {
        assert_eq!(AUTONEG_START, 0x1200);
    }

    #[test]
    fn fl3_phy_link_decode() {
        assert_eq!(link_state(0x7809, 0x0006), LinkState::Down);
        assert_eq!(
            link_state(0x7809 | BMSR_LINK, 0x0000),
            LinkState::Negotiating
        );
        let up = 0x7829 | BMSR_LINK;
        assert_eq!(link_state(up, 0x0116), LinkState::Up(Link::FAST_FULL));
        assert_eq!(
            link_state(up, 0x0001),
            LinkState::Up(Link {
                speed: Speed::Mbps10,
                duplex: Duplex::Half
            })
        );
        assert_eq!(
            link_state(up, 0x0002),
            LinkState::Up(Link {
                speed: Speed::Mbps100,
                duplex: Duplex::Half
            })
        );
        assert_eq!(
            link_state(up, 0x0005),
            LinkState::Up(Link {
                speed: Speed::Mbps10,
                duplex: Duplex::Full
            })
        );
        for op in [3u16, 4, 7] {
            assert_eq!(link_state(up, op), LinkState::Unknown(op as u8));
        }
        // A failed read (all ones) is never "up".
        assert_eq!(link_state(0xFFFF, 0xFFFF), LinkState::Unknown(7));
    }
}
