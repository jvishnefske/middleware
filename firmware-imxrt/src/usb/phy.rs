//! Clock and PHY bring-up values for USB OTG1, as NXP's SDK does it
//! (`CLOCK_EnableUsbhs0PhyPllClock`, `CLOCK_EnableUsbhs0Clock`,
//! `USB_EhciPhyInit`): the controller's clock gate, the 480 MHz USB PLL
//! (PLL3, `CCM_ANALOG_PLL_USB1`) and USBPHY1.
//!
//! PLL3 also clocks other blocks (LPUART's 80 MHz root, the FlexSPI PFD),
//! so a PLL that already runs is never bypassed or relocked: only its USB
//! clock output is enabled ([`PllUsb1::Running`]).
//!
//! Bit positions assumed (reference manual CCM_ANALOG and USBPHY chapters,
//! cross-checked with the PAC's field offsets), to be checked on hardware:
//!
//! | register | bits |
//! |----------|------|
//! | `CCM_CCGR6` | `CG0` 1:0 (usboh3, 0b11 = on in run and wait) |
//! | `CCM_ANALOG_PLL_USB1` | `DIV_SELECT` 1 (0 = ×20, 480 MHz), `EN_USB_CLKS` 6, `POWER` 12, `ENABLE` 13, `BYPASS_CLK_SRC` 15:14, `BYPASS` 16, `LOCK` 31 |
//! | `USBPHY1_CTRL` | `ENUTMILEVEL2` 14, `ENUTMILEVEL3` 15, `ENAUTOCLR_CLKGATE` 19, `ENAUTOCLR_PHY_PWD` 20, `CLKGATE` 30, `SFTRST` 31 |
//! | `USBPHY1_TX` | `D_CAL` 3:0, `TXCAL45DN` 11:8, `TXCAL45DP` 19:16 |

/// `CCM_CCGR6.CG0` value: the USB controller clock on (run and wait).
pub const CCGR6_CG0_ON: u8 = 0b11;

/// `PLL_USB1`: loop divider select (0: 480 MHz from the 24 MHz crystal).
pub const PLL_DIV_SELECT: u32 = 1 << 1;
/// `PLL_USB1`: the PLL's 480 MHz output to the USB PHY.
pub const PLL_EN_USB_CLKS: u32 = 1 << 6;
/// `PLL_USB1`: power up.
pub const PLL_POWER: u32 = 1 << 12;
/// `PLL_USB1`: output enable.
pub const PLL_ENABLE: u32 = 1 << 13;
/// `PLL_USB1`: bypass clock source field (15:14; 0 = the 24 MHz oscillator).
pub const PLL_BYPASS_CLK_SRC: u32 = 0b11 << 14;
/// `PLL_USB1`: bypass (the output is the bypass source).
pub const PLL_BYPASS: u32 = 1 << 16;
/// `PLL_USB1`: locked.
pub const PLL_LOCK: u32 = 1 << 31;

/// What to do with PLL3, from `PLL_USB1`'s current value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PllUsb1 {
    /// It runs (powered, enabled, locked, not bypassed): write this value
    /// (the current one with `EN_USB_CLKS`) and nothing else.
    Running(u32),
    /// It is off or bypassed: write `bypass`, then `start`, wait for
    /// `LOCK` ([`PLL_LOCK`], bounded), then write `start` without
    /// [`PLL_BYPASS`] ([`pll_usb1_unbypass`]).
    Start {
        /// Bypassed to the oscillator while it locks.
        bypass: u32,
        /// Powered, enabled, USB output on, 480 MHz, still bypassed.
        start: u32,
    },
}

/// Decides PLL3's bring-up from `PLL_USB1`'s current value.
pub const fn pll_usb1(current: u32) -> PllUsb1 {
    let running = PLL_POWER | PLL_ENABLE | PLL_LOCK;
    if current & running == running && current & PLL_BYPASS == 0 {
        PllUsb1::Running(current | PLL_EN_USB_CLKS)
    } else {
        let bypass = (current & !PLL_BYPASS_CLK_SRC) | PLL_BYPASS;
        let start = (bypass & !PLL_DIV_SELECT) | PLL_ENABLE | PLL_POWER | PLL_EN_USB_CLKS;
        PllUsb1::Start { bypass, start }
    }
}

/// The last `PLL_USB1` write of [`PllUsb1::Start`]: bypass off.
pub const fn pll_usb1_unbypass(start: u32) -> u32 {
    start & !PLL_BYPASS
}

/// `USBPHY_CTRL`: UTMI+ level 2 (low-speed devices behind a hub).
pub const PHY_ENUTMILEVEL2: u32 = 1 << 14;
/// `USBPHY_CTRL`: UTMI+ level 3.
pub const PHY_ENUTMILEVEL3: u32 = 1 << 15;
/// `USBPHY_CTRL`: clear `CLKGATE` automatically on wakeup from suspend.
pub const PHY_ENAUTOCLR_CLKGATE: u32 = 1 << 19;
/// `USBPHY_CTRL`: clear `PWD` automatically on wakeup from suspend.
pub const PHY_ENAUTOCLR_PHY_PWD: u32 = 1 << 20;
/// `USBPHY_CTRL`: gate the PHY's clocks.
pub const PHY_CLKGATE: u32 = 1 << 30;
/// `USBPHY_CTRL`: soft reset.
pub const PHY_SFTRST: u32 = 1 << 31;

/// Written to `USBPHY_CTRL_CLR` first: out of reset, clocks ungated.
pub const PHY_CTRL_CLEAR: u32 = PHY_SFTRST | PHY_CLKGATE;
/// Written to `USBPHY_CTRL_SET` after `PWD` = 0.
pub const PHY_CTRL_SET: u32 = PHY_ENUTMILEVEL2 | PHY_ENUTMILEVEL3 | PHY_ENAUTOCLR_PHY_PWD | PHY_ENAUTOCLR_CLKGATE;
/// `USBPHY_PWD` value: every part of the PHY powered.
pub const PHY_PWD_ON: u32 = 0;

/// The EVKB's high-speed transmitter trim (SDK board file:
/// `BOARD_USB_PHY_D_CAL` 0x0C, `TXCAL45DP` 0x06, `TXCAL45DM` 0x06).
pub const PHY_D_CAL: u32 = 0x0C;
/// See [`PHY_D_CAL`].
pub const PHY_TXCAL45DP: u32 = 0x06;
/// See [`PHY_D_CAL`].
pub const PHY_TXCAL45DN: u32 = 0x06;

/// `USBPHY_TX` with the trim fields replaced, from its current value.
pub const fn phy_tx(current: u32) -> u32 {
    let mask = 0xF | (0xF << 8) | (0xF << 16);
    (current & !mask) | PHY_D_CAL | (PHY_TXCAL45DN << 8) | (PHY_TXCAL45DP << 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fl2_a_running_usb_pll_only_gets_its_usb_output_enabled() {
        // As embassy-nxp and the boot ROM leave it: running, USB output off.
        let cur = PLL_POWER | PLL_ENABLE | PLL_LOCK;
        assert_eq!(pll_usb1(cur), PllUsb1::Running(cur | PLL_EN_USB_CLKS));
    }

    #[test]
    fn fl2_a_stopped_usb_pll_starts_bypassed_at_480_mhz_then_leaves_bypass() {
        let cur = 0x0001_2000 | PLL_DIV_SELECT | (0b01 << 14);
        let PllUsb1::Start { bypass, start } = pll_usb1(cur) else { panic!("not started") };
        assert_eq!(bypass & PLL_BYPASS, PLL_BYPASS);
        assert_eq!(bypass & PLL_BYPASS_CLK_SRC, 0, "bypass to the 24 MHz oscillator");
        assert_eq!(start & (PLL_POWER | PLL_ENABLE | PLL_EN_USB_CLKS), PLL_POWER | PLL_ENABLE | PLL_EN_USB_CLKS);
        assert_eq!(start & PLL_DIV_SELECT, 0, "x20: 480 MHz");
        assert_eq!(pll_usb1_unbypass(start) & PLL_BYPASS, 0);
        // A locked but bypassed PLL is restarted, not trusted.
        assert!(matches!(pll_usb1(PLL_POWER | PLL_ENABLE | PLL_LOCK | PLL_BYPASS), PllUsb1::Start { .. }));
    }

    #[test]
    fn fl2_phy_comes_out_of_reset_ungated_with_utmi_levels_and_trim() {
        assert_eq!(PHY_CTRL_CLEAR, 0xC000_0000);
        assert_eq!(PHY_CTRL_SET, (1 << 14) | (1 << 15) | (1 << 19) | (1 << 20));
        assert_eq!(phy_tx(0x1006_0608), 0x1006_060C);
        assert_eq!(phy_tx(0xFFFF_FFFF), 0xFFF6_F6FC);
    }
}
