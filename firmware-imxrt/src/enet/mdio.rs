//! MDIO: the ENET MMFR frame of a clause-22 management transfer and the
//! MSCR clock divider.
//!
//! Writing MMFR starts one transfer; the MAC sets `EIR.MII` when it is
//! done and, for a read, the PHY's 16 bits are in MMFR's data field.
//!
//! | bits  | field | value |
//! |-------|-------|-------|
//! | 31:30 | ST    | `01` (clause 22) |
//! | 29:28 | OP    | `10` read, `01` write |
//! | 27:23 | PA    | PHY address |
//! | 22:18 | RA    | register address |
//! | 17:16 | TA    | `10` |
//! | 15:0  | DATA  | data written / read |

/// MMFR ST = 01 (clause 22 frame).
pub const MMFR_ST_CLAUSE22: u32 = 0b01 << 30;
/// MMFR OP for a write.
pub const MMFR_OP_WRITE: u32 = 0b01 << 28;
/// MMFR OP for a read.
pub const MMFR_OP_READ: u32 = 0b10 << 28;
/// MMFR TA = 10.
pub const MMFR_TA: u32 = 0b10 << 16;
/// `EIR.MII` (bit 23): the management transfer completed (write 1 to clear).
pub const EIR_MII: u32 = 1 << 23;
/// Reads of EIR the shell makes waiting for [`EIR_MII`] before the
/// transfer counts as failed (the probe's bound).
pub const POLL_BOUND: u32 = 100_000;

/// The highest MDC frequency the MAC is set for (IEEE 802.3 clause 22).
pub const MDC_MAX_HZ: u32 = 2_500_000;
/// The shortest MDIO hold time after MDC's rising edge the MSCR is set for.
pub const MDIO_HOLD_MIN_NS: u32 = 10;
/// MSCR `MII_SPEED` field shift (bits 6:1).
pub const MSCR_MII_SPEED_SHIFT: u32 = 1;
/// MSCR `MII_SPEED` largest value.
pub const MSCR_MII_SPEED_MAX: u32 = 0x3F;
/// MSCR `HOLDTIME` field shift (bits 10:8).
pub const MSCR_HOLDTIME_SHIFT: u32 = 8;
/// MSCR `HOLDTIME` largest value.
pub const MSCR_HOLDTIME_MAX: u32 = 7;
/// The MSCR the probe ran: `MII_SPEED` 0x3F (MDC = IPG / 128, the slowest)
/// and `HOLDTIME` 2 (three IPG cycles). Below 2.5 MHz for any IPG clock up
/// to 320 MHz.
pub const MSCR_PROBE: u32 =
    (MSCR_MII_SPEED_MAX << MSCR_MII_SPEED_SHIFT) | (2 << MSCR_HOLDTIME_SHIFT);

const fn frame(op: u32, phy: u8, reg: u8, data: u16) -> Option<u32> {
    if phy > 31 || reg > 31 {
        return None;
    }
    Some(
        MMFR_ST_CLAUSE22
            | op
            | ((phy as u32) << 23)
            | ((reg as u32) << 18)
            | MMFR_TA
            | data as u32,
    )
}

/// The MMFR value that reads register `reg` of the PHY at `phy`, or `None`
/// for an address or register above 31.
pub const fn read_frame(phy: u8, reg: u8) -> Option<u32> {
    frame(MMFR_OP_READ, phy, reg, 0)
}

/// The MMFR value that writes `data` to register `reg` of the PHY at
/// `phy`, or `None` for an address or register above 31.
pub const fn write_frame(phy: u8, reg: u8, data: u16) -> Option<u32> {
    frame(MMFR_OP_WRITE, phy, reg, data)
}

/// The data a completed read left in MMFR.
pub const fn read_data(mmfr: u32) -> u16 {
    (mmfr & 0xFFFF) as u16
}

/// The MSCR value for an IPG clock of `ipg_hz`: the smallest `MII_SPEED`
/// with MDC = IPG / ((MII_SPEED + 1) × 2) at most [`MDC_MAX_HZ`], and the
/// smallest `HOLDTIME` with (HOLDTIME + 1) IPG cycles of at least
/// [`MDIO_HOLD_MIN_NS`]. `None` for a zero clock or one too fast for the
/// fields (`MII_SPEED` past 0x3F, above 320 MHz).
pub const fn mscr_for_ipg(ipg_hz: u32) -> Option<u32> {
    if ipg_hz == 0 {
        return None;
    }
    // (s + 1) >= ipg / (2 * MDC_MAX): ceil division.
    let per = 2 * MDC_MAX_HZ;
    let divisor = ipg_hz.div_ceil(per);
    let speed = if divisor == 0 { 0 } else { divisor - 1 };
    // MII_SPEED 0 turns MDC off: never use it.
    let speed = if speed == 0 { 1 } else { speed };
    if speed > MSCR_MII_SPEED_MAX {
        return None;
    }
    // Cycles of hold: ceil(ipg_hz * 10 ns), at least one.
    let cycles = (ipg_hz as u64 * MDIO_HOLD_MIN_NS as u64).div_ceil(1_000_000_000);
    let hold = if cycles == 0 { 0 } else { cycles - 1 };
    let hold = if hold > MSCR_HOLDTIME_MAX as u64 { MSCR_HOLDTIME_MAX } else { hold as u32 };
    Some((speed << MSCR_MII_SPEED_SHIFT) | (hold << MSCR_HOLDTIME_SHIFT))
}

/// The MDC frequency an MSCR value gives at an IPG clock of `ipg_hz`
/// (`None` when `MII_SPEED` is 0, MDC off).
pub const fn mdc_hz(mscr: u32, ipg_hz: u32) -> Option<u32> {
    let speed = (mscr >> MSCR_MII_SPEED_SHIFT) & MSCR_MII_SPEED_MAX;
    if speed == 0 {
        return None;
    }
    Some(ipg_hz / ((speed + 1) * 2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fl3_mdio_read_frame_matches_probe() {
        // Probe: (1 << 30) | (2 << 28) | (PHY << 23) | (reg << 18) | (2 << 16).
        assert_eq!(read_frame(2, 2), Some(0x610A_0000));
        assert_eq!(read_frame(2, 3), Some(0x610E_0000));
        for reg in 0..32u8 {
            let probe = (1 << 30) | (2 << 28) | (2 << 23) | (u32::from(reg) << 18) | (2 << 16);
            assert_eq!(read_frame(2, reg), Some(probe));
        }
    }

    #[test]
    fn fl3_mdio_write_frame_matches_probe() {
        // BMCR <- 0x1200 (auto-negotiation restart) at PHY 2.
        assert_eq!(write_frame(2, 0, 0x1200), Some(0x5102_1200));
        let probe = (1 << 30) | (1 << 28) | (2 << 23) | (0x1F << 18) | (2 << 16) | 0x0080;
        assert_eq!(write_frame(2, 0x1F, 0x0080), Some(probe));
    }

    #[test]
    fn fl3_mdio_fields_out_of_range_refused() {
        assert_eq!(read_frame(32, 0), None);
        assert_eq!(read_frame(0, 32), None);
        assert_eq!(write_frame(255, 255, 0xFFFF), None);
        assert_eq!(write_frame(31, 31, 0xFFFF), Some(0x5FFE_FFFF));
    }

    #[test]
    fn fl3_mdio_read_data_is_low_half() {
        assert_eq!(read_data(0x610A_0022), 0x0022);
        assert_eq!(read_data(0xFFFF_1561), 0x1561);
    }

    #[test]
    fn fl3_mscr_probe_value() {
        assert_eq!(MSCR_PROBE, 0x27E);
        assert_eq!(MSCR_PROBE, (0x3F << 1) | (2 << 8));
        // At 150 MHz IPG the probe's MDC is about 1.17 MHz.
        assert_eq!(mdc_hz(MSCR_PROBE, 150_000_000), Some(1_171_875));
    }

    #[test]
    fn fl3_mscr_for_ipg_never_exceeds_2_5_mhz() {
        let mut hz = 1_000_000u32;
        while hz <= 320_000_000 {
            let m = mscr_for_ipg(hz).unwrap_or(0);
            let mdc = mdc_hz(m, hz).unwrap_or(u32::MAX);
            assert!(mdc <= MDC_MAX_HZ, "{hz} -> {mdc}");
            let hold = (m >> 8) & 7;
            // (hold + 1) cycles cover 10 ns unless the field is full.
            assert!(hold == 7 || u64::from(hold + 1) * 1_000_000_000 >= u64::from(hz) * 10);
            hz += 1_234_567;
        }
    }

    #[test]
    fn fl3_mscr_for_ipg_values() {
        // 150 MHz: MII_SPEED 29 (2.5 MHz), 2 cycles hold (13.3 ns).
        assert_eq!(mscr_for_ipg(150_000_000), Some((29 << 1) | (1 << 8)));
        // 75 MHz: MII_SPEED 14, 1 cycle (13.3 ns).
        assert_eq!(mscr_for_ipg(75_000_000), Some(14 << 1));
        // Slow clock: MII_SPEED never 0 (MDC off).
        assert_eq!(mscr_for_ipg(1_000_000), Some(1 << 1));
        assert_eq!(mscr_for_ipg(0), None);
        assert_eq!(mscr_for_ipg(320_000_000), Some((63 << 1) | (3 << 8)));
        assert_eq!(mscr_for_ipg(320_000_001), None);
        assert_eq!(mscr_for_ipg(u32::MAX), None);
        assert_eq!(mdc_hz(0, 150_000_000), None);
    }
}
