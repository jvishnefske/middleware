//! RMII clocking: the ENET PLL at 50 MHz and the reference clock driven
//! out to the PHY.
//!
//! On the EVKB the RT1050 is the RMII clock source: the ENET PLL makes
//! 50 MHz, `IOMUXC_GPR1.ENET1_TX_CLK_DIR` drives it out of ENET_REF_CLK
//! (GPIO_B1_10, with SION so the MAC also takes it back in) and
//! `ENET1_CLK_SEL = 0` selects that internal clock as the MAC's reference.
//! The PHY is set to take a 50 MHz reference ([`super::ksz8081`]).

/// CCM_CCGR1 CG5 (bits 11:10): the ENET clock gate, on in all modes.
pub const CCGR1_ENET: u32 = 3 << 10;
/// CCM_CCGR1 CG13 (bits 27:26): GPIO1, which drives the PHY's reset pin.
pub const CCGR1_GPIO1: u32 = 3 << 26;

/// `CCM_ANALOG_PLL_ENET.DIV_SELECT` (bits 1:0): ENET reference frequency.
pub const PLL_ENET_DIV_SELECT_MASK: u32 = 0b11;
/// `DIV_SELECT` value for 50 MHz (00 = 25, 01 = 50, 10 = 100, 11 = 125).
pub const PLL_ENET_DIV_50MHZ: u32 = 0b01;
/// `CCM_ANALOG_PLL_ENET.POWERDOWN` (bit 12).
pub const PLL_ENET_POWERDOWN: u32 = 1 << 12;
/// `CCM_ANALOG_PLL_ENET.ENABLE` (bit 13): the ENET reference output.
pub const PLL_ENET_ENABLE: u32 = 1 << 13;
/// `CCM_ANALOG_PLL_ENET.BYPASS` (bit 16).
pub const PLL_ENET_BYPASS: u32 = 1 << 16;
/// `CCM_ANALOG_PLL_ENET.LOCK` (bit 31, read only).
pub const PLL_ENET_LOCK: u32 = 1 << 31;
/// Reads of PLL_ENET the shell makes waiting for [`PLL_ENET_LOCK`] (the
/// probe's bound).
pub const PLL_LOCK_POLL_BOUND: u32 = 1_000_000;

/// `IOMUXC_GPR1.ENET1_CLK_SEL` (bit 13): 1 takes the reference from the
/// pad, 0 from the ENET PLL. Cleared here.
pub const GPR1_ENET1_CLK_SEL: u32 = 1 << 13;
/// `IOMUXC_GPR1.ENET1_TX_CLK_DIR` (bit 17): 1 drives the reference clock
/// out of ENET_REF_CLK. Set here.
pub const GPR1_ENET1_TX_CLK_DIR: u32 = 1 << 17;

/// The PLL_ENET value for a 50 MHz RMII reference from the current value
/// `current`: powered up, not bypassed, `DIV_SELECT` 50 MHz, output
/// enabled; every other bit kept.
pub const fn pll_enet_50mhz(current: u32) -> u32 {
    let cleared = current & !(PLL_ENET_POWERDOWN | PLL_ENET_BYPASS | PLL_ENET_DIV_SELECT_MASK);
    cleared | PLL_ENET_DIV_50MHZ | PLL_ENET_ENABLE
}

/// Whether a PLL_ENET read says the PLL is locked.
pub const fn pll_enet_locked(value: u32) -> bool {
    value & PLL_ENET_LOCK != 0
}

/// The IOMUXC_GPR1 value from the current value `current`: the reference
/// clock driven out (`ENET1_TX_CLK_DIR = 1`) and taken from the PLL
/// (`ENET1_CLK_SEL = 0`); every other bit kept.
pub const fn gpr1_rmii_ref_out(current: u32) -> u32 {
    (current | GPR1_ENET1_TX_CLK_DIR) & !GPR1_ENET1_CLK_SEL
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fl3_clock_gates_are_the_probes() {
        // The probe: CCGR1 |= (3 << 10) | (3 << 26).
        assert_eq!(CCGR1_ENET | CCGR1_GPIO1, 0x0C00_0C00);
    }

    #[test]
    fn fl3_pll_enet_50mhz_from_reset_value() {
        // RT1050 reset value: POWERDOWN, BYPASS, DIV_SELECT 01.
        assert_eq!(pll_enet_50mhz(0x0001_1001), 0x0000_2001);
        // The probe's expression on any value.
        for v in [0u32, 0xFFFF_FFFF, 0x0001_1001, 0x8000_3003, 0x1234_5678] {
            let probe = (v & !((1 << 12) | (1 << 16) | 0b11)) | 1 | (1 << 13);
            assert_eq!(pll_enet_50mhz(v), probe);
        }
    }

    #[test]
    fn fl3_pll_enet_50mhz_keeps_other_bits() {
        let v = pll_enet_50mhz(0xFFFF_FFFF);
        assert_eq!(v & PLL_ENET_DIV_SELECT_MASK, PLL_ENET_DIV_50MHZ);
        assert_eq!(v & (PLL_ENET_POWERDOWN | PLL_ENET_BYPASS), 0);
        assert_ne!(v & PLL_ENET_ENABLE, 0);
        assert_eq!(v | PLL_ENET_POWERDOWN | PLL_ENET_BYPASS | 0b10, 0xFFFF_FFFF);
    }

    #[test]
    fn fl3_pll_lock_bit() {
        assert!(pll_enet_locked(0x8000_2001));
        assert!(!pll_enet_locked(0x0000_2001));
    }

    #[test]
    fn fl3_gpr1_drives_reference_clock_out() {
        for v in [0u32, 0xFFFF_FFFF, 0x0F40_2000] {
            let probe = (v | (1 << 17)) & !(1 << 13);
            assert_eq!(gpr1_rmii_ref_out(v), probe);
        }
        assert_eq!(gpr1_rmii_ref_out(0), 0x0002_0000);
    }
}
