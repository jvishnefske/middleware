//! Control endpoint and bus protocols: the setup packet, the setup
//! tripwire protocol that copies it out of endpoint 0 OUT's queue head,
//! and the bus reset plan, each as the steps and decisions the shell
//! follows with its register pokes.
//!
//! Setup lockouts are off ([`super::regs::USBMODE_SLOM`]), so the
//! controller may overwrite the queue head's setup buffer with a newer
//! setup packet while software copies it; the tripwire (`USBCMD.SUTW`)
//! tells the copy that happened: the controller clears `SUTW` when it
//! writes the buffer. [`SETUP`] is the sequence (as in the reference
//! manual's "Setup phase" and the SDK's `USB_DeviceEhciFillSetupBuffer`),
//! [`SetupTripwire`] decides each copy.
//!
//! Every wait is bounded ([`wait_step`]): a register that does not settle
//! within [`POLL_LIMIT`] reads is reported, never spun on forever.

use super::dma::QhConfig;
use super::{Dir, EpMask};

/// A setup packet (USB 2.0 §9.3), decoded from the 8 setup buffer bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetupPacket {
    /// `bmRequestType`.
    pub request_type: u8,
    /// `bRequest`.
    pub request: u8,
    /// `wValue`.
    pub value: u16,
    /// `wIndex`.
    pub index: u16,
    /// `wLength`: bytes in the data stage.
    pub length: u16,
}

/// `bRequest` of `SET_ADDRESS`.
pub const SET_ADDRESS: u8 = 5;

impl SetupPacket {
    /// Decodes the setup buffer's bytes (multi-byte fields little-endian).
    pub const fn from_bytes(b: [u8; 8]) -> SetupPacket {
        SetupPacket {
            request_type: b[0],
            request: b[1],
            value: u16::from_le_bytes([b[2], b[3]]),
            index: u16::from_le_bytes([b[4], b[5]]),
            length: u16::from_le_bytes([b[6], b[7]]),
        }
    }

    /// The data stage's direction (`bmRequestType` bit 7); with no data
    /// stage, the status stage is IN.
    pub const fn data_dir(self) -> Dir {
        if self.request_type & 0x80 != 0 { Dir::In } else { Dir::Out }
    }

    /// The address of a standard device `SET_ADDRESS` (write it at once
    /// with `USBADRA`, [`super::regs::deviceaddr`]); `None` for any other
    /// request or an address past 127.
    pub const fn set_address(self) -> Option<u8> {
        if self.request_type == 0x00 && self.request == SET_ADDRESS && self.value <= 127 {
            Some(self.value as u8)
        } else {
            None
        }
    }
}

/// Register reads a bounded wait takes before it gives up.
pub const POLL_LIMIT: u32 = 100_000;

/// One read of a register the shell waits on to read zero (setup status,
/// prime, flush), decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitStep {
    /// The bits read zero: the wait is over.
    Done,
    /// Read again.
    Again,
    /// [`POLL_LIMIT`] reads without settling.
    TimedOut,
}

/// Decides the `polls`-th read (counting from 0) of a waited-on register
/// whose value, masked by the bits waited on, is `masked`.
pub const fn wait_step(masked: u32, polls: u32) -> WaitStep {
    if masked == 0 {
        WaitStep::Done
    } else if polls.saturating_add(1) >= POLL_LIMIT {
        WaitStep::TimedOut
    } else {
        WaitStep::Again
    }
}

/// One step of reading a setup packet on endpoint 0 (`ENDPTSETUPSTAT`
/// bit 0 set, seen after `USBSTS.UI`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupStep {
    /// Write [`super::regs::setupstat_bit`]`(0)` to `ENDPTSETUPSTAT`
    /// (write 1 to clear) and wait ([`wait_step`]) until it reads clear.
    /// It must be clear before the data or status stage is primed.
    ClearSetupStat,
    /// Set `USBCMD.SUTW`.
    SetTripwire,
    /// Copy the setup buffer ([`super::dma::qh_setup_bytes`] on endpoint
    /// 0 OUT's queue head).
    Copy,
    /// Read `USBCMD.SUTW` and pass it with the copy to
    /// [`SetupTripwire::after_copy`]: `Again` goes back to
    /// [`SetupStep::SetTripwire`].
    CheckTripwire,
    /// Clear `USBCMD.SUTW`.
    ClearTripwire,
    /// Cancel what is still primed on endpoint 0 from an earlier control
    /// transfer: write [`super::regs::flush_both`]`(0)` to `ENDPTFLUSH`
    /// and wait until it reads clear. Then handle the packet.
    FlushEp0,
}

/// Reading a setup packet, in order.
pub const SETUP: [SetupStep; 6] = [
    SetupStep::ClearSetupStat,
    SetupStep::SetTripwire,
    SetupStep::Copy,
    SetupStep::CheckTripwire,
    SetupStep::ClearTripwire,
    SetupStep::FlushEp0,
];

/// Copies tried before giving up: setup packets arriving back to back
/// faster than this many copies is a broken host; the newest one sets
/// `ENDPTSETUPSTAT` again and is read on the next interrupt.
pub const TRIPWIRE_ATTEMPTS: u8 = 8;

/// The tripwire's decision after one copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tripwire {
    /// `SUTW` survived the copy: this is the packet.
    Done(SetupPacket),
    /// The controller wrote a newer packet during the copy: set the
    /// tripwire and copy again.
    Again,
    /// [`TRIPWIRE_ATTEMPTS`] copies were all overwritten: clear `SUTW` and
    /// drop this one.
    GaveUp,
}

/// The setup tripwire protocol's state: how many copies were tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SetupTripwire {
    attempts: u8,
}

impl SetupTripwire {
    /// Before the first copy.
    pub const fn new() -> SetupTripwire {
        SetupTripwire { attempts: 0 }
    }

    /// Decides a copy: `sutw_still_set` is `USBCMD.SUTW` read after
    /// copying `bytes`.
    pub fn after_copy(&mut self, sutw_still_set: bool, bytes: [u8; 8]) -> Tripwire {
        self.attempts = self.attempts.saturating_add(1);
        if sutw_still_set {
            Tripwire::Done(SetupPacket::from_bytes(bytes))
        } else if self.attempts >= TRIPWIRE_ATTEMPTS {
            Tripwire::GaveUp
        } else {
            Tripwire::Again
        }
    }
}

/// One step of handling a bus reset (`USBSTS.URI`), as the reference
/// manual's "Bus reset" lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetStep {
    /// Write `ENDPTSETUPSTAT`'s value back to it (clear every setup flag).
    ClearSetupStat,
    /// Write `ENDPTCOMPLETE`'s value back to it (clear every completion).
    ClearComplete,
    /// Wait ([`wait_step`]) until `ENDPTPRIME` reads zero: primes in
    /// progress finish.
    WaitPrimeIdle,
    /// Write [`EpMask::ALL`] to `ENDPTFLUSH` and wait until it reads zero:
    /// every primed endpoint is cancelled.
    FlushAll,
    /// Write 0 to `DEVICEADDR`: the device is at the default address.
    ClearAddress,
    /// Software state: every dTD is free (pending transfers end as
    /// cancelled), every queue head is re-initialised ([`reset_qh_config`]),
    /// `ENDPTCTRL1`..`7` are written 0 (data endpoints disabled until the
    /// host configures the device).
    ResetEndpoints,
    /// Read `PORTSC1` and decide with [`bus_reset_outcome`].
    CheckPortReset,
}

/// Handling a bus reset, in order.
pub const BUS_RESET: [ResetStep; 7] = [
    ResetStep::ClearSetupStat,
    ResetStep::ClearComplete,
    ResetStep::WaitPrimeIdle,
    ResetStep::FlushAll,
    ResetStep::ClearAddress,
    ResetStep::ResetEndpoints,
    ResetStep::CheckPortReset,
];

/// The `ENDPTFLUSH` value of [`ResetStep::FlushAll`].
pub const RESET_FLUSH: u32 = EpMask::ALL.0;

/// What `PORTSC1` says after the bus reset was handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusReset {
    /// `PR` is still set: the reset is in progress and was handled in
    /// time. The speed is read at the port change (`USBSTS.PCI`) that ends
    /// it.
    InProgress,
    /// `PR` already cleared: the handling took longer than the reset, and
    /// the controller must be reset (`USBCMD.RST`) and set up again.
    TooLate,
}

/// Decides [`ResetStep::CheckPortReset`].
pub const fn bus_reset_outcome(portsc1: u32) -> BusReset {
    if portsc1 & super::regs::PORTSC1_PR != 0 { BusReset::InProgress } else { BusReset::TooLate }
}

/// The configuration a queue head is reset to on a bus reset: endpoint
/// 0's control heads, and the data heads at the [`QhConfig::data`] max
/// packet `data_max_packet` (they are set again when the host configures
/// the device). `qh_index` is [`super::Ep::qh_index`].
pub const fn reset_qh_config(qh_index: usize, data_max_packet: u16) -> QhConfig {
    match qh_index {
        0 => QhConfig::CONTROL_OUT,
        1 => QhConfig::CONTROL_IN,
        _ => QhConfig::data(data_max_packet),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usb::dma::{self, QH_LIST_WORDS};
    use crate::usb::{Ep, regs};
    use core::sync::atomic::{AtomicU32, Ordering};

    const GET_DESCRIPTOR: [u8; 8] = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x40, 0x00];

    #[test]
    fn fl2_setup_packet_decodes_little_endian_fields() {
        let p = SetupPacket::from_bytes(GET_DESCRIPTOR);
        assert_eq!(p.request_type, 0x80);
        assert_eq!(p.request, 6);
        assert_eq!(p.value, 0x0100);
        assert_eq!(p.index, 0);
        assert_eq!(p.length, 64);
        assert_eq!(p.data_dir(), Dir::In);
        assert_eq!(p.set_address(), None);
    }

    #[test]
    fn fl2_set_address_is_recognised_and_written_with_advance() {
        let p = SetupPacket::from_bytes([0x00, 0x05, 0x2A, 0x00, 0, 0, 0, 0]);
        assert_eq!(p.set_address(), Some(42));
        assert_eq!(p.data_dir(), Dir::Out);
        let addr = p.set_address().unwrap();
        assert_eq!(regs::deviceaddr(addr, true), Ok((42 << 25) | (1 << 24)));
        assert_eq!(SetupPacket::from_bytes([0x00, 0x05, 0x80, 0x00, 0, 0, 0, 0]).set_address(), None);
        assert_eq!(SetupPacket::from_bytes([0x01, 0x05, 0x01, 0x00, 0, 0, 0, 0]).set_address(), None);
    }

    #[test]
    fn fl2_setup_is_read_under_the_tripwire_cleared_first_and_ep0_flushed_last() {
        assert_eq!(SETUP.first(), Some(&SetupStep::ClearSetupStat));
        assert_eq!(SETUP.last(), Some(&SetupStep::FlushEp0));
        let pos = |s| SETUP.iter().position(|x| *x == s).unwrap();
        assert!(pos(SetupStep::SetTripwire) < pos(SetupStep::Copy));
        assert!(pos(SetupStep::Copy) < pos(SetupStep::CheckTripwire));
        assert!(pos(SetupStep::CheckTripwire) < pos(SetupStep::ClearTripwire));
    }

    #[test]
    fn fl2_tripwire_keeps_a_copy_only_if_sutw_survived_it() {
        // The DMA memory model: the controller writes a newer packet into
        // the queue head between the first copy and the SUTW read.
        let list: [AtomicU32; QH_LIST_WORDS] = core::array::from_fn(|_| AtomicU32::new(0));
        let q = dma::qh(&list, Ep::EP0_OUT).unwrap();
        dma::init_qh(q, dma::QhConfig::CONTROL_OUT).unwrap();
        q.get(10).unwrap().store(0x0000_0500, Ordering::SeqCst);
        let mut tw = SetupTripwire::new();
        let stale = dma::qh_setup_bytes(q).unwrap();
        q.get(10).unwrap().store(0x0100_0680, Ordering::SeqCst);
        q.get(11).unwrap().store(0x0040_0000, Ordering::SeqCst);
        assert_eq!(tw.after_copy(false, stale), Tripwire::Again, "controller cleared SUTW");
        let fresh = dma::qh_setup_bytes(q).unwrap();
        assert_eq!(tw.after_copy(true, fresh), Tripwire::Done(SetupPacket::from_bytes(GET_DESCRIPTOR)));
    }

    #[test]
    fn fl2_tripwire_gives_up_after_its_attempts() {
        let mut tw = SetupTripwire::new();
        for _ in 1..TRIPWIRE_ATTEMPTS {
            assert_eq!(tw.after_copy(false, [0; 8]), Tripwire::Again);
        }
        assert_eq!(tw.after_copy(false, [0; 8]), Tripwire::GaveUp);
        // Further calls stay given up and never overflow.
        for _ in 0..300 {
            assert_eq!(tw.after_copy(false, [0; 8]), Tripwire::GaveUp);
        }
    }

    #[test]
    fn fl2_waits_end_on_zero_or_after_the_poll_limit() {
        assert_eq!(wait_step(0, 0), WaitStep::Done);
        assert_eq!(wait_step(1, 0), WaitStep::Again);
        assert_eq!(wait_step(1, POLL_LIMIT - 2), WaitStep::Again);
        assert_eq!(wait_step(1, POLL_LIMIT - 1), WaitStep::TimedOut);
        assert_eq!(wait_step(0, POLL_LIMIT - 1), WaitStep::Done, "a final zero read counts");
        assert_eq!(wait_step(1, u32::MAX), WaitStep::TimedOut);
    }

    #[test]
    fn fl2_bus_reset_clears_flags_waits_for_primes_then_flushes_all() {
        let pos = |s| BUS_RESET.iter().position(|x| *x == s).unwrap();
        assert!(pos(ResetStep::ClearSetupStat) < pos(ResetStep::WaitPrimeIdle));
        assert!(pos(ResetStep::ClearComplete) < pos(ResetStep::WaitPrimeIdle));
        assert!(pos(ResetStep::WaitPrimeIdle) < pos(ResetStep::FlushAll));
        assert!(pos(ResetStep::FlushAll) < pos(ResetStep::ResetEndpoints));
        assert_eq!(BUS_RESET.last(), Some(&ResetStep::CheckPortReset));
        assert_eq!(RESET_FLUSH, 0x00FF_00FF);
    }

    #[test]
    fn fl2_a_bus_reset_handled_after_pr_cleared_needs_a_controller_reset() {
        assert_eq!(bus_reset_outcome(regs::PORTSC1_PR), BusReset::InProgress);
        assert_eq!(bus_reset_outcome(2 << 26), BusReset::TooLate);
    }

    #[test]
    fn fl2_bus_reset_returns_queue_heads_to_control_and_data_defaults() {
        assert_eq!(reset_qh_config(0, 512), QhConfig::CONTROL_OUT);
        assert_eq!(reset_qh_config(1, 512), QhConfig::CONTROL_IN);
        assert_eq!(reset_qh_config(5, 512), QhConfig::data(512));
        assert!(reset_qh_config(0, 512).ios && !reset_qh_config(1, 512).ios);
    }
}
