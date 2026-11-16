//! Register values and bits of the device controller: what the shell
//! writes to `USB1`'s registers and how it reads what they report.
//!
//! Bit positions assumed (i.MX RT1050 reference manual, USB chapter; NXP
//! SDK `usb_device_ehci.c`), to be checked on hardware:
//!
//! | register (offset) | bits |
//! |-------------------|------|
//! | `USBCMD` (0x140) | `RS` 0, `RST` 1, `SUTW` 13, `ATDTW` 14, `ITC` 23:16 |
//! | `USBSTS` / `USBINTR` (0x144 / 0x148) | `UI` 0, `UEI` 1, `PCI` 2, `SEI` 4, `URI` 6, `SRI` 7, `SLI` 8; `USBSTS` write 1 to clear |
//! | `DEVICEADDR` (0x154) | `USBADR` 31:25, `USBADRA` 24 |
//! | `ENDPTLISTADDR` (0x158) | `EPBASE` 31:11 |
//! | `PORTSC1` (0x184) | `PR` 8, `SUSP` 7, `HSP` 9, `PFSC` 24, `PSPD` 27:26 (0 full, 1 low, 2 high) |
//! | `USBMODE` (0x1A8) | `CM` 1:0 (2 device), `ES` 2, `SLOM` 3, `SDIS` 4 |
//! | `ENDPTSETUPSTAT` (0x1AC) | endpoint `n` bit `n` |
//! | `ENDPTPRIME`, `ENDPTFLUSH`, `ENDPTSTAT`, `ENDPTCOMPLETE` (0x1B0–0x1BC) | OUT `n` bit `n`, IN `n` bit `16 + n` ([`Ep::bit`]) |
//! | `ENDPTCTRLn` (0x1C0 + 4n) | `RXS` 0, `RXT` 3:2, `RXI` 5, `RXR` 6, `RXE` 7; `TXS` 16, `TXT` 19:18, `TXI` 21, `TXR` 22, `TXE` 23 |

use super::{Dir, Ep, Error};

/// `USBCMD`: run/stop.
pub const USBCMD_RS: u32 = 1 << 0;
/// `USBCMD`: controller reset (self-clearing).
pub const USBCMD_RST: u32 = 1 << 1;
/// `USBCMD`: setup tripwire ([`super::control::SetupTripwire`]).
pub const USBCMD_SUTW: u32 = 1 << 13;
/// `USBCMD`: add dTD tripwire (unused: dTDs are only primed idle).
pub const USBCMD_ATDTW: u32 = 1 << 14;
/// `USBCMD`: interrupt threshold control field mask (23:16).
pub const USBCMD_ITC_MASK: u32 = 0xFF << 16;

/// `USBCMD` for running in device mode: `RS` with `ITC` 0 (interrupt at
/// once, not at a microframe threshold), from the register's `current`
/// value.
pub const fn usbcmd_run(current: u32) -> u32 {
    (current & !USBCMD_ITC_MASK) | USBCMD_RS
}

/// `USBCMD` with the run bit cleared (detach), from `current`.
pub const fn usbcmd_stop(current: u32) -> u32 {
    current & !USBCMD_RS
}

/// `USBMODE`: controller mode field mask.
pub const USBMODE_CM_MASK: u32 = 0b11;
/// `USBMODE`: controller mode device.
pub const USBMODE_CM_DEVICE: u32 = 0b10;
/// `USBMODE`: setup lockout off (the tripwire protocol is used instead).
pub const USBMODE_SLOM: u32 = 1 << 3;
/// `USBMODE` written after the controller reset: device mode, setup
/// lockouts off, little-endian, streaming enabled.
pub const USBMODE_DEVICE: u32 = USBMODE_CM_DEVICE | USBMODE_SLOM;

/// Interrupt bits (`USBSTS`, `USBINTR`): USB interrupt (a dTD with `IOC`
/// retired, or a setup packet arrived).
pub const INT_UI: u32 = 1 << 0;
/// Interrupt bits: USB error interrupt (a dTD retired with an error).
pub const INT_UEI: u32 = 1 << 1;
/// Interrupt bits: port change detect (attach, end of reset: speed known).
pub const INT_PCI: u32 = 1 << 2;
/// Interrupt bits: system error.
pub const INT_SEI: u32 = 1 << 4;
/// Interrupt bits: USB reset received.
pub const INT_URI: u32 = 1 << 6;
/// Interrupt bits: start of frame received.
pub const INT_SRI: u32 = 1 << 7;
/// Interrupt bits: device suspend.
pub const INT_SLI: u32 = 1 << 8;
/// `USBINTR` value the driver enables: transfer and error completion,
/// port change, system error, reset and suspend (no start-of-frame).
pub const USBINTR_DEVICE: u32 = INT_UI | INT_UEI | INT_PCI | INT_SEI | INT_URI | INT_SLI;

/// What one read of `USBSTS` reports. The shell writes the same value
/// back to clear it (write 1 to clear) before acting on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    /// A transfer completed or a setup packet arrived: read
    /// `ENDPTSETUPSTAT` and `ENDPTCOMPLETE`.
    pub transfer: bool,
    /// A transfer retired with an error (also in `ENDPTCOMPLETE`).
    pub error: bool,
    /// Port change: the bus reset ended or the port resumed; read the
    /// speed ([`PortSpeed::from_portsc1`]).
    pub port_change: bool,
    /// A system error: the controller's DMA failed.
    pub system_error: bool,
    /// A bus reset began ([`super::control::BUS_RESET`]).
    pub reset: bool,
    /// The bus suspended.
    pub suspend: bool,
}

impl Status {
    /// Decodes `USBSTS`, keeping only the enabled ([`USBINTR_DEVICE`])
    /// bits.
    pub const fn from_usbsts(usbsts: u32) -> Status {
        Status {
            transfer: usbsts & INT_UI != 0,
            error: usbsts & INT_UEI != 0,
            port_change: usbsts & INT_PCI != 0,
            system_error: usbsts & INT_SEI != 0,
            reset: usbsts & INT_URI != 0,
            suspend: usbsts & INT_SLI != 0,
        }
    }
}

/// `DEVICEADDR`: address field shift (31:25).
pub const DEVICEADDR_USBADR_SHIFT: u32 = 25;
/// `DEVICEADDR`: address advance (the address takes effect after the next
/// IN transfer on endpoint 0 completes: the status stage of
/// `SET_ADDRESS`).
pub const DEVICEADDR_USBADRA: u32 = 1 << 24;

/// The `DEVICEADDR` value for USB address `addr`. With `advance`, it is
/// written as soon as `SET_ADDRESS` is decoded, before its status stage,
/// and the controller holds it until that stage's IN completes; without,
/// it takes effect at once (write it after the status stage).
pub const fn deviceaddr(addr: u8, advance: bool) -> Result<u32, Error> {
    if addr > 127 {
        return Err(Error::BadAddress);
    }
    let value = (addr as u32) << DEVICEADDR_USBADR_SHIFT;
    Ok(if advance { value | DEVICEADDR_USBADRA } else { value })
}

/// The `ENDPTLISTADDR` value for a queue head list at bus address
/// `list_addr`, which must be 2048-byte aligned.
pub const fn endpointlistaddr(list_addr: u32) -> Result<u32, Error> {
    if list_addr & (super::dma::QH_LIST_ALIGN - 1) != 0 {
        Err(Error::Misaligned)
    } else {
        Ok(list_addr)
    }
}

/// `PORTSC1`: port reset (high while the host drives the bus reset).
pub const PORTSC1_PR: u32 = 1 << 8;
/// `PORTSC1`: port speed field shift (27:26).
pub const PORTSC1_PSPD_SHIFT: u32 = 26;
/// `PORTSC1`: force full speed (high speed disabled).
pub const PORTSC1_PFSC: u32 = 1 << 24;

/// The speed the port runs at, after the bus reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortSpeed {
    /// Full speed, 12 Mb/s.
    Full,
    /// Low speed, 1.5 Mb/s (not a device speed; reported as read).
    Low,
    /// High speed, 480 Mb/s.
    High,
}

impl PortSpeed {
    /// Decodes `PORTSC1.PSPD`; the reserved value 3 is `None`.
    pub const fn from_portsc1(portsc1: u32) -> Option<PortSpeed> {
        match (portsc1 >> PORTSC1_PSPD_SHIFT) & 0b11 {
            0 => Some(PortSpeed::Full),
            1 => Some(PortSpeed::Low),
            2 => Some(PortSpeed::High),
            _ => None,
        }
    }

    /// The bulk max packet length at this speed: 512 at high speed, 64
    /// otherwise.
    pub const fn bulk_max_packet(self) -> u16 {
        match self {
            PortSpeed::High => super::HS_BULK_MAX_PACKET,
            PortSpeed::Full | PortSpeed::Low => super::FS_BULK_MAX_PACKET,
        }
    }

    /// The packet size a data endpoint described with `described` bytes
    /// runs at this speed: as described at high speed, at most 64 below
    /// (a host enumerating a high-speed configuration at full speed clamps
    /// bulk endpoints to 64, as Linux does).
    pub const fn data_max_packet(self, described: u16) -> u16 {
        match self {
            PortSpeed::High => described,
            PortSpeed::Full | PortSpeed::Low => {
                if described < super::FS_BULK_MAX_PACKET { described } else { super::FS_BULK_MAX_PACKET }
            }
        }
    }
}

/// An endpoint's transfer type, as `ENDPTCTRLn`'s `RXT`/`TXT` encode it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpType {
    /// Control (0).
    Control,
    /// Isochronous (1).
    Isochronous,
    /// Bulk (2).
    Bulk,
    /// Interrupt (3).
    Interrupt,
}

impl EpType {
    /// The 2-bit type field value.
    pub const fn bits(self) -> u32 {
        match self {
            EpType::Control => 0,
            EpType::Isochronous => 1,
            EpType::Bulk => 2,
            EpType::Interrupt => 3,
        }
    }
}

/// `ENDPTCTRLn`: receive (OUT) stall.
pub const CTRL_RXS: u32 = 1 << 0;
/// `ENDPTCTRLn`: receive type field shift (3:2).
pub const CTRL_RXT_SHIFT: u32 = 2;
/// `ENDPTCTRLn`: receive data toggle reset (write 1: next PID DATA0).
pub const CTRL_RXR: u32 = 1 << 6;
/// `ENDPTCTRLn`: receive enable.
pub const CTRL_RXE: u32 = 1 << 7;
/// `ENDPTCTRLn`: transmit (IN) stall.
pub const CTRL_TXS: u32 = 1 << 16;
/// `ENDPTCTRLn`: transmit type field shift (19:18).
pub const CTRL_TXT_SHIFT: u32 = 18;
/// `ENDPTCTRLn`: transmit data toggle reset.
pub const CTRL_TXR: u32 = 1 << 22;
/// `ENDPTCTRLn`: transmit enable.
pub const CTRL_TXE: u32 = 1 << 23;

/// `ENDPTCTRL0`: endpoint 0 is always enabled control in both directions.
pub const ENDPTCTRL0: u32 = CTRL_RXE | CTRL_TXE;

/// The `ENDPTCTRLn` value (n = 1 to 7) for an endpoint whose OUT half has
/// type `out` and IN half type `inn` (`None`: that half disabled). An
/// enabled half gets its type, enable and data toggle reset (so it starts
/// at DATA0, as after `SET_CONFIGURATION`). A disabled half opposite an
/// enabled one is typed bulk: the reference manual says an unused
/// direction must not be left at the control type. Endpoint 0 is fixed
/// ([`ENDPTCTRL0`]) and refused here.
pub const fn endptctrl(number: u8, out: Option<EpType>, inn: Option<EpType>) -> Result<u32, Error> {
    if number == 0 || number >= super::ENDPOINTS {
        return Err(Error::NoSuchEndpoint);
    }
    let unused = if out.is_some() || inn.is_some() { EpType::Bulk.bits() } else { 0 };
    let rx = match out {
        Some(t) => (t.bits() << CTRL_RXT_SHIFT) | CTRL_RXE | CTRL_RXR,
        None => unused << CTRL_RXT_SHIFT,
    };
    let tx = match inn {
        Some(t) => (t.bits() << CTRL_TXT_SHIFT) | CTRL_TXE | CTRL_TXR,
        None => unused << CTRL_TXT_SHIFT,
    };
    Ok(rx | tx)
}

/// The stall bit of `ep`'s direction in `ENDPTCTRLn`.
pub const fn stall_bit(ep: Ep) -> u32 {
    match ep.dir() {
        Dir::Out => CTRL_RXS,
        Dir::In => CTRL_TXS,
    }
}

/// `ENDPTCTRLn` after stalling or unstalling `ep`, from the register's
/// `current` value. Endpoint 0 stalls both directions (the controller
/// clears both on the next setup packet). Unstalling a data endpoint also
/// resets its data toggle, as `CLEAR_FEATURE(ENDPOINT_HALT)` requires.
pub const fn set_stall(current: u32, ep: Ep, stalled: bool) -> u32 {
    let bits = if ep.number() == 0 { CTRL_RXS | CTRL_TXS } else { stall_bit(ep) };
    if stalled {
        current | bits
    } else if ep.number() == 0 {
        current & !bits
    } else {
        let toggle = match ep.dir() {
            Dir::Out => CTRL_RXR,
            Dir::In => CTRL_TXR,
        };
        (current & !bits) | toggle
    }
}

/// Whether `ep` is stalled, from its `ENDPTCTRLn`.
pub const fn is_stalled(endptctrl: u32, ep: Ep) -> bool {
    endptctrl & stall_bit(ep) != 0
}

/// `ENDPTSETUPSTAT`'s bit for control endpoint `number`.
pub const fn setupstat_bit(number: u8) -> Result<u32, Error> {
    if number < super::ENDPOINTS { Ok(1 << number) } else { Err(Error::NoSuchEndpoint) }
}

/// The `ENDPTFLUSH` value that cancels both directions of `number`.
pub const fn flush_both(number: u8) -> Result<u32, Error> {
    if number < super::ENDPOINTS {
        Ok((1 << number) | (1 << (16 + number as u32)))
    } else {
        Err(Error::NoSuchEndpoint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fl2_device_mode_is_cm_device_with_setup_lockouts_off() {
        assert_eq!(USBMODE_DEVICE, 0b1010);
        assert_eq!(usbcmd_run(0x0008_0B00), 0x0000_0B01, "ITC cleared, RS set");
        assert_eq!(usbcmd_stop(0x0000_0B01), 0x0000_0B00);
        assert_eq!(USBINTR_DEVICE, 0b1_0101_0111);
    }

    #[test]
    fn fl2_usbsts_decodes_each_interrupt() {
        let s = Status::from_usbsts(INT_URI | INT_PCI | INT_SRI);
        assert!(s.reset && s.port_change);
        assert!(!s.transfer && !s.error && !s.system_error && !s.suspend);
        let all = Status::from_usbsts(u32::MAX);
        assert!(all.transfer && all.error && all.port_change && all.system_error && all.reset && all.suspend);
    }

    #[test]
    fn fl2_device_address_goes_in_bits_31_to_25_with_advance_in_24() {
        assert_eq!(deviceaddr(5, true), Ok((5 << 25) | (1 << 24)));
        assert_eq!(deviceaddr(127, false), Ok(0xFE00_0000));
        assert_eq!(deviceaddr(0, false), Ok(0));
        assert_eq!(deviceaddr(128, true), Err(Error::BadAddress));
    }

    #[test]
    fn fl2_endpoint_list_must_be_2048_aligned() {
        assert_eq!(endpointlistaddr(0x2000_0800), Ok(0x2000_0800));
        assert_eq!(endpointlistaddr(0x2000_0400), Err(Error::Misaligned));
    }

    #[test]
    fn fl2_port_speed_decodes_from_pspd() {
        assert_eq!(PortSpeed::from_portsc1(2 << 26), Some(PortSpeed::High));
        assert_eq!(PortSpeed::from_portsc1(0), Some(PortSpeed::Full));
        assert_eq!(PortSpeed::from_portsc1((1 << 26) | PORTSC1_PR), Some(PortSpeed::Low));
        assert_eq!(PortSpeed::from_portsc1(3 << 26), None);
        assert_eq!(PortSpeed::High.bulk_max_packet(), 512);
        assert_eq!(PortSpeed::Full.bulk_max_packet(), 64);
        assert_eq!(PortSpeed::High.data_max_packet(512), 512);
        assert_eq!(PortSpeed::Full.data_max_packet(512), 64);
        assert_eq!(PortSpeed::Full.data_max_packet(16), 16);
    }

    #[test]
    fn fl2_endptctrl_enables_types_and_resets_toggles() {
        // CDC data interface: bulk OUT and bulk IN.
        let bulk = endptctrl(2, Some(EpType::Bulk), Some(EpType::Bulk)).unwrap();
        assert_eq!(bulk, (2 << 2) | (1 << 6) | (1 << 7) | (2 << 18) | (1 << 22) | (1 << 23));
        // CDC notification: interrupt IN only; the unused OUT half is bulk.
        let int = endptctrl(1, None, Some(EpType::Interrupt)).unwrap();
        assert_eq!(int, (2 << 2) | (3 << 18) | (1 << 22) | (1 << 23));
        assert_eq!(endptctrl(3, Some(EpType::Control), None), Ok((1 << 6) | (1 << 7) | (2 << 18)));
        assert_eq!(endptctrl(4, None, None), Ok(0));
        assert_eq!(endptctrl(0, None, None), Err(Error::NoSuchEndpoint));
        assert_eq!(endptctrl(8, None, None), Err(Error::NoSuchEndpoint));
        assert_eq!(ENDPTCTRL0, (1 << 7) | (1 << 23));
    }

    #[test]
    fn fl2_stall_sets_one_direction_and_ep0_stalls_both() {
        let in2 = Ep::new(2, Dir::In).unwrap();
        let out2 = Ep::new(2, Dir::Out).unwrap();
        let ctrl = endptctrl(2, Some(EpType::Bulk), Some(EpType::Bulk)).unwrap() & !(CTRL_RXR | CTRL_TXR);
        let stalled = set_stall(ctrl, in2, true);
        assert_eq!(stalled, ctrl | CTRL_TXS);
        assert!(is_stalled(stalled, in2) && !is_stalled(stalled, out2));
        assert_eq!(set_stall(stalled, in2, false), ctrl | CTRL_TXR, "unstall resets the toggle");
        let ep0 = set_stall(ENDPTCTRL0, Ep::EP0_IN, true);
        assert_eq!(ep0, ENDPTCTRL0 | CTRL_RXS | CTRL_TXS);
        assert_eq!(set_stall(ep0, Ep::EP0_OUT, false), ENDPTCTRL0);
    }

    #[test]
    fn fl2_setup_and_flush_bits_per_endpoint() {
        assert_eq!(setupstat_bit(0), Ok(1));
        assert_eq!(setupstat_bit(8), Err(Error::NoSuchEndpoint));
        assert_eq!(flush_both(0), Ok(0x0001_0001));
        assert_eq!(flush_both(7), Ok(0x0080_0080));
        assert_eq!(flush_both(8), Err(Error::NoSuchEndpoint));
    }
}
