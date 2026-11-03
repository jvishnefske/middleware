//! A/B firmware over-the-air update on the RP2350 (Pico 2): the
//! integration of `middleware::fw_ota` into the RP base firmware.
//!
//! `middleware::fw_ota` holds the protocol (`sys/fw/*`), the pure state
//! machine, the IMAGE_DEF parser, the persisted record and `FwService`, the
//! shell that runs one request against a `FwPlatform`. This module supplies
//! what is RP2350 specific:
//!
//! * [`image`] — the image's boot metadata: IMAGE_DEF (flags `0x9021`,
//!   version) at image offset `0x200` and the sentinel block at `0xF00`,
//!   written into the linker script by `build.rs`;
//! * [`table`] — the partition table making slots A and B an A/B pair;
//! * [`factory`] — the update checks, the TBYB clearing, and the factory
//!   flash image (table plus bought image in slot A);
//! * [`uf2`] — the factory UF2 (absolute addresses) and the update UF2;
//! * [`rom`] — the [`rom::Rom`] seam to the boot ROM and the chip;
//! * [`platform`] — [`platform::RpPlatform`], `FwPlatform` over any NOR
//!   flash and a [`rom::Rom`], for the map of [`crate::layout::RP2350`];
//! * [`AbUpdate`] — the [`FwUpdate`] backend the request service hands
//!   every `sys/fw/*` request to.
//!
//! [`AbUpdate::boot`] runs `FwService::boot` once at startup (reconciling
//! the record with this boot, settling or discarding as needed). When it
//! fails (most often: the board has no partition table, so the boot ROM
//! booted no slot) the backend stays and answers every `sys/fw/*` request
//! `Unavailable`; the failure is in the boot log. A request is answered
//! `Unavailable` as well when the payload store, which lends the flash,
//! could not be opened.
//!
//! The firmware's self-test ([`crate::fw_update::SelfTest`]) gates
//! `sys/fw/confirm`: until it passes, a confirm is `Unavailable` and
//! `mwctl` retries; a trial that is never confirmed is reverted by the next
//! reset (the boot ROM skips the unbought image).

pub mod factory;
pub mod image;
pub mod platform;
pub mod rom;
pub mod table;
pub mod uf2;

#[cfg(test)]
mod tests;

use embedded_storage::nor_flash::NorFlash;
use middleware::Key;
use middleware::fw_ota::service::{Context, FwService};
use middleware::fw_ota::{BootInfo, FwRecord, FwUpdate as FwState};
use middleware::transport::HandlerError;

use crate::fw_update::{FwContext, FwUpdate};
use crate::layout::AbMap;
use platform::{PlatformResult, RpPlatform};
use rom::Rom;

/// The A/B firmware-update backend.
pub struct AbUpdate<R> {
    rom: R,
    ab: AbMap,
    svc: Option<FwService>,
}

impl<R: Rom> AbUpdate<R> {
    /// Starts the updater: `FwService::boot` on the platform over `flash`
    /// and `rom`. The backend is returned either way, with this boot or
    /// why the updater did not start; without it, it answers
    /// `Unavailable`.
    pub fn boot<F: NorFlash>(flash: &mut F, mut rom: R, ab: AbMap) -> (Self, PlatformResult<BootInfo, F, R>) {
        let mut p = RpPlatform::new(flash, &mut rom, ab, None);
        let svc = FwService::boot(&mut p);
        let (svc, result) = match svc {
            Ok(s) => {
                let b = s.boot_info();
                (Some(s), Ok(b))
            }
            Err(e) => (None, Err(e)),
        };
        (AbUpdate { rom, ab, svc }, result)
    }

    /// The update state (`None` when the updater did not start).
    pub fn state(&self) -> Option<FwState> {
        self.svc.as_ref().map(FwService::state)
    }

    /// The persisted record as last read or saved.
    pub fn record(&self) -> Option<FwRecord> {
        self.svc.as_ref().map(FwService::record)
    }

    /// This boot, as the updater saw it.
    pub fn boot_info(&self) -> Option<BootInfo> {
        self.svc.as_ref().map(FwService::boot_info)
    }

    /// The ROM seam.
    pub fn rom(&self) -> &R {
        &self.rom
    }
}

impl<F: NorFlash, R: Rom> FwUpdate<F> for AbUpdate<R> {
    fn handle(
        &mut self,
        flash: Option<&mut F>,
        ctx: &FwContext,
        key: Key<'_>,
        body: &[u8],
        out: &mut [u8],
    ) -> Result<usize, HandlerError> {
        let (Some(svc), Some(flash)) = (self.svc.as_mut(), flash) else {
            return Err(HandlerError::Unavailable);
        };
        let running = Some(svc.boot_info().running);
        let mut p = RpPlatform::new(flash, &mut self.rom, self.ab, running);
        let c = Context { node: ctx.node, trust: ctx.trust, self_test_ok: ctx.self_test.ok() };
        svc.handle(&mut p, &c, key, body, out).unwrap_or(Err(HandlerError::NotFound))
    }
}
