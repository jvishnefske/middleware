//! ENET MAC and KSZ8081RNB PHY core (FL3): the decisions and register
//! values the Ethernet shell applies, with no hardware access.
//!
//! The EVKB's PHY is a KSZ8081RNB on RMII, at MDIO address 2, clocked by a
//! 50 MHz reference the RT1050 drives out of ENET_REF_CLK (GPIO_B1_10).
//! The values here are the ones the RAM-loaded probe ran on hardware on
//! 2026-11-13 (PHY found, link up at 100BASE-TX full duplex, DHCP OFFER
//! received; FL5, FL7), and the tests assert them exactly.
//!
//! * [`clock`] — the ENET PLL at 50 MHz (`CCM_ANALOG_PLL_ENET`), the
//!   reference clock direction (`IOMUXC_GPR1`) and the clock gates.
//! * [`mdio`] — the MMFR frame of a clause-22 read and write and the MSCR
//!   MDC divider for an IPG clock.
//! * [`ksz8081`] — the PHY's identity check, software reset poll, 50 MHz
//!   RMII clock mode, auto-negotiation start and link-status decode.
//! * [`mac`] — the MAC registers: RCR and TCR for a link, ECR, the station
//!   address, the receive buffer size, the descriptor-active triggers.
//! * [`desc`] — the legacy 8-byte buffer descriptors and the receive and
//!   transmit rings over `AtomicU32` DMA memory, with the ownership
//!   hand-off between the CPU and the ENET DMA.
//!
//! # Bring-up order (the shell)
//!
//! 1. Clocks: set [`clock::CCGR1_ENET`] (and [`clock::CCGR1_GPIO1`] for the
//!    PHY reset pin) in CCM_CCGR1; write
//!    [`clock::pll_enet_50mhz`]`(PLL_ENET)` and poll
//!    [`clock::pll_enet_locked`] at most [`clock::PLL_LOCK_POLL_BOUND`]
//!    times; write [`clock::gpr1_rmii_ref_out`]`(GPR1)`.
//! 2. Pads (the board's pin table) and the PHY's hardware reset: GPIO1_IO09
//!    low [`ksz8081::HW_RESET_LOW_MS`], high, then wait
//!    [`ksz8081::HW_RESET_SETTLE_MS`].
//! 3. MAC reset: write [`mac::ECR_RESET`] to ECR and poll until it reads
//!    clear (at most [`mac::RESET_POLL_BOUND`] reads); write MSCR with
//!    [`mdio::mscr_for_ipg`] (or [`mdio::MSCR_PROBE`]).
//! 4. PHY over MDIO (each transfer: clear [`mdio::EIR_MII`], write the
//!    frame from [`mdio::read_frame`] / [`mdio::write_frame`], poll EIR for
//!    `EIR_MII` at most [`mdio::POLL_BOUND`] times, take
//!    [`mdio::read_data`]):
//!    [`ksz8081::identify`] on registers 2 and 3;
//!    write [`ksz8081::RESET_COMMAND`] to BMCR, then read BMCR and decide
//!    with [`ksz8081::reset_step`] until `Done`;
//!    read PHY control 2 and write [`ksz8081::ctrl2_rmii_50mhz`] of it;
//!    write [`ksz8081::AUTONEG_START`] to BMCR.
//! 5. Rings: [`desc::RxRing::init`] and [`desc::TxRing::init`] with the
//!    DTCM buffer addresses.
//! 6. MAC: EIMR 0, EIR [`mac::EIR_ALL`], IAUR/IALR/GAUR/GALR 0, PALR/PAUR
//!    from [`mac::station_address`], RCR [`mac::rcr`], TCR [`mac::tcr`],
//!    TFWR [`mac::TFWR_STORE_AND_FORWARD`], MRBR [`mac::MRBR`], RDSR/TDSR
//!    the ring bases (checked by [`desc::ring_base_ok`]), ECR
//!    [`mac::ECR_ENABLE`], RDAR [`mac::DESCRIPTOR_ACTIVE`].
//! 7. Link: read BMSR and PHY control 1 periodically and decide with
//!    [`ksz8081::link_state`]; when the link comes up with a speed or
//!    duplex other than the one RCR/TCR were set for, write ECR 0 (MAC
//!    off), rewrite RCR and TCR for the new [`ksz8081::Link`], and enable
//!    again (rings re-initialised).
//! 8. Traffic: [`desc::RxRing::peek`] / [`desc::copy_frame`] /
//!    [`desc::RxRing::release`] then RDAR; [`desc::TxRing::free`] /
//!    [`desc::fill_frame`] / [`desc::TxRing::submit`] then TDAR.

pub mod clock;
pub mod desc;
pub mod ksz8081;
pub mod mac;
pub mod mdio;

pub use mac::mac_from_uid;
