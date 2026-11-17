//! eDMA channels that run as endless receive rings, without interrupts.
//!
//! A [`Ring`] copies one element from a peripheral's data register into a
//! ring of atomics in DTCM per DMAMUX request: the major loop is the ring,
//! after its last element the destination steps back by the ring's length
//! (`DLAST_SGA`) and, with `DREQ` clear, the channel stays enabled, so it
//! never stops. The reader takes what lies between its own index and the
//! DMA's write position ([`write_index`], from `DADDR`). DTCM is not
//! cached, so a write by the DMA engine is like a write by another thread.
//!
//! | channel | user | request |
//! |---------|------|---------|
//! | 0 | LPUART1 receive ([`super::uart`]) | DMAMUX 3 |
//! | 1 | SAI1 receive, the microphone ([`super::sai`]) | DMAMUX 19 |
//!
//! The PAC names each channel's TCD registers separately (`tcd0_*`,
//! `tcd1_*`), all of the same types, so [`Tcd`] gathers one channel's.

use embassy_nxp::pac;
use pac::common::{RW, Reg};
use pac::dma::regs;

/// An eDMA channel this firmware uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// LPUART1 receive.
    Ch0 = 0,
    /// SAI1 receive.
    Ch1 = 1,
}

/// The element the ring holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Width {
    /// One byte per request.
    U8,
    /// One 32-bit word per request.
    U32,
}

impl Width {
    /// Bytes per element.
    pub const fn bytes(self) -> u32 {
        match self {
            Width::U8 => 1,
            Width::U32 => 4,
        }
    }

    const fn size(self) -> pac::dma::vals::TcdAttrSsize {
        match self {
            Width::U8 => pac::dma::vals::TcdAttrSsize::Eight,
            Width::U32 => pac::dma::vals::TcdAttrSsize::ThirtytwoBit,
        }
    }
}

/// A receive ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ring {
    /// The channel.
    pub channel: Channel,
    /// The DMAMUX request source.
    pub source: u8,
    /// The peripheral data register's address.
    pub src: u32,
    /// The ring's address (in DTCM).
    pub base: u32,
    /// Elements in the ring (at most 32767, eDMA's loop count).
    pub len: u16,
    /// The element width.
    pub width: Width,
}

/// One channel's TCD registers.
struct Tcd {
    saddr: Reg<regs::TcdSaddr, RW>,
    soff: Reg<regs::TcdSoff, RW>,
    attr: Reg<regs::TcdAttr, RW>,
    nbytes: Reg<regs::TcdNbytesMlno, RW>,
    slast: Reg<regs::TcdSlast, RW>,
    daddr: Reg<regs::TcdDaddr, RW>,
    doff: Reg<regs::TcdDoff, RW>,
    citer: Reg<regs::TcdCiterElinkno, RW>,
    biter: Reg<regs::TcdBiterElinkno, RW>,
    dlastsga: Reg<regs::TcdDlastsga, RW>,
    csr: Reg<regs::TcdCsr, RW>,
}

macro_rules! tcd {
    ($d:expr, $saddr:ident, $soff:ident, $attr:ident, $nbytes:ident, $slast:ident, $daddr:ident,
     $doff:ident, $citer:ident, $biter:ident, $dlastsga:ident, $csr:ident) => {
        Tcd {
            saddr: $d.$saddr(),
            soff: $d.$soff(),
            attr: $d.$attr(),
            nbytes: $d.$nbytes(),
            slast: $d.$slast(),
            daddr: $d.$daddr(),
            doff: $d.$doff(),
            citer: $d.$citer(),
            biter: $d.$biter(),
            dlastsga: $d.$dlastsga(),
            csr: $d.$csr(),
        }
    };
}

fn tcd(channel: Channel) -> Tcd {
    let d = pac::DMA0;
    match channel {
        Channel::Ch0 => tcd!(
            d,
            tcd0_saddr,
            tcd0_soff,
            tcd0_attr,
            tcd0_nbytes_mlno,
            tcd0_slast,
            tcd0_daddr,
            tcd0_doff,
            tcd0_citer_elinkno,
            tcd0_biter_elinkno,
            tcd0_dlastsga,
            tcd0_csr
        ),
        Channel::Ch1 => tcd!(
            d,
            tcd1_saddr,
            tcd1_soff,
            tcd1_attr,
            tcd1_nbytes_mlno,
            tcd1_slast,
            tcd1_daddr,
            tcd1_doff,
            tcd1_citer_elinkno,
            tcd1_biter_elinkno,
            tcd1_dlastsga,
            tcd1_csr
        ),
    }
}

/// Programs and starts `ring` (the peripheral's DMA request is enabled
/// by the caller afterwards).
pub fn start_ring(ring: &Ring) {
    // eDMA and DMAMUX share one clock gate (CCGR5 CG3).
    pac::CCM.ccgr5().modify(|w| w.set_cg3(0b11));
    let ch = ring.channel as u8;
    let d = pac::DMA0;
    d.cerq().write(|w| w.set_cerq(ch));
    let t = tcd(ring.channel);
    let step = ring.width.bytes();
    t.saddr.write(|w| w.set_saddr(ring.src));
    t.soff.write(|w| w.set_soff(0));
    t.attr.write(|w| {
        w.set_ssize(ring.width.size());
        w.set_dsize(ring.width.size().to_bits());
    });
    t.nbytes.write(|w| w.set_nbytes(step));
    t.slast.write(|w| w.set_slast(0));
    t.daddr.write(|w| w.set_daddr(ring.base));
    t.doff.write(|w| w.set_doff(step as u16));
    t.citer.write(|w| w.set_citer(ring.len));
    t.biter.write(|w| w.set_biter(ring.len));
    t.dlastsga
        .write(|w| w.set_dlastsga((u32::from(ring.len) * step).wrapping_neg()));
    t.csr.write(|w| w.set_dreq(false));
    pac::DMAMUX.chcfg(ring.channel as usize).write(|w| {
        w.set_source(ring.source);
        w.set_enbl(pac::dmamux::vals::Enbl::Enbl1);
    });
    d.serq().write(|w| w.set_serq(ch));
}

/// The ring index the DMA writes next.
pub fn write_index(ring: &Ring) -> usize {
    let daddr = tcd(ring.channel).daddr.read().daddr();
    let bytes = daddr.wrapping_sub(ring.base) / ring.width.bytes();
    bytes as usize % usize::from(ring.len.max(1))
}

/// Stops the channel: its request disabled and its DMAMUX slot off.
pub fn stop(channel: Channel) {
    pac::DMA0.cerq().write(|w| w.set_cerq(channel as u8));
    pac::DMAMUX
        .chcfg(channel as usize)
        .write(|w| w.set_enbl(pac::dmamux::vals::Enbl::Enbl0));
}
