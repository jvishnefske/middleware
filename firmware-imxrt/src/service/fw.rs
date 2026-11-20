//! `sys/fw/*` on the RT1050: [`Updater`], `middleware::fw_ota`'s
//! `FwService::<RtImage>` over [`RtPlatform`], with the A/B state that
//! outlives a request ([`AbState`]).
//!
//! * [`Updater::boot`] runs `FwService::<RtImage>::boot_for` once at
//!   startup, from the bootloader's [`Handoff`] (which slot runs, on trial
//!   or not). Without a handoff (the firmware was loaded by a debugger,
//!   not booted by the bootloader) or when the boot step fails (the update
//!   record cannot be read), the updater stays down and every `sys/fw/*`
//!   request is answered `Unavailable`; the reason is in the boot facts.
//! * [`Updater::handle`] builds an [`RtPlatform`] over the raw flash for
//!   the request and runs it, then takes what the platform asked of the
//!   shell: a reset ([`ResetRequest`], after `sys/fw/reboot` or an abort
//!   of a trial) and the release of the trial watchdog (after
//!   `sys/fw/confirm`). They are returned as values; nothing here resets
//!   the chip or touches the watchdog.
//!
//! Fail closed, as on the RP base firmware (RP V13): with the trust record
//! unreadable ([`FwContext::trust`] `None`) `begin`, `chunk` and `commit`,
//! the requests that put an image into a slot, are `Unavailable` before
//! the updater sees them ([`needs_trust`]); `info`, `abort`, `reboot` (of an
//! image committed while the record was readable) and `confirm` still go
//! through, and `confirm` is held back by the self-test, which needs the
//! trust record ([`SelfTest`]).

use crate::ab::platform::{AbError, ResetRequest};
use crate::ab::{AbState, Handoff, RtImage, RtPlatform};
use crate::layout::AbMap;
use embedded_storage::nor_flash::NorFlash;
use middleware::Key;
use middleware::fw_ota::service::{Context, FwService};
use middleware::fw_ota::{BootInfo, FwRecord, FwUpdate as FwState, keys};
use middleware::loader::trust::TrustState;
use middleware::transport::{HandlerError, SenderId};

/// Prefix of every firmware-update key.
pub const PREFIX: &str = "sys/fw/";

/// Whether `key` is a firmware-update key (every key under [`PREFIX`]).
pub fn is_fw_key(key: Key<'_>) -> bool {
    key.as_str().starts_with(PREFIX)
}

/// Whether `key` installs firmware and so needs a known root of trust:
/// `begin`, `chunk` and `commit`.
pub fn needs_trust(key: Key<'_>) -> bool {
    key == keys::BEGIN || key == keys::CHUNK || key == keys::COMMIT
}

/// The firmware's own health check, which must pass before a trial image
/// is confirmed (`sys/fw/confirm`): the new firmware reached the state in
/// which it can be updated again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SelfTest {
    /// The shell's protocol link is up (LPUART1 is configured and a frame
    /// has been received on it).
    pub link_ready: bool,
    /// The trust record opened (the root of trust that authorizes the
    /// next update is readable).
    pub trust_open: bool,
}

impl SelfTest {
    /// Every check passed.
    pub const fn ok(&self) -> bool {
        self.link_ready && self.trust_open
    }
}

/// What the updater is given with every request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FwContext {
    /// The node id `sys/info` reports (bound into firmware signatures).
    pub node: SenderId,
    /// The chip `sys/info` reports: a firmware release cert must name it.
    pub chip: &'static str,
    /// The root of trust; `None` when the trust record could not be read.
    pub trust: Option<TrustState>,
    /// The self-test.
    pub self_test: SelfTest,
}

/// Why the updater did not start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FwBootError<E> {
    /// No bootloader handoff: the slot that runs is unknown.
    NoHandoff,
    /// The platform failed (most often: the update record is unreadable).
    Platform(AbError<E>),
}

/// The result of one `sys/fw/*` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FwHandled {
    /// The reply body length in the caller's buffer, or the error.
    pub reply: Result<usize, HandlerError>,
    /// A reset the shell performs once the reply has left.
    pub reset: Option<ResetRequest>,
    /// The trial was confirmed by this request: the shell may stop the
    /// trial watchdog.
    pub release_watchdog: bool,
}

impl FwHandled {
    const fn reply(reply: Result<usize, HandlerError>) -> Self {
        FwHandled {
            reply,
            reset: None,
            release_watchdog: false,
        }
    }
}

/// The A/B updater. See the module docs.
pub struct Updater {
    map: AbMap,
    state: AbState,
    svc: Option<FwService<RtImage>>,
}

impl Updater {
    /// Starts the updater over `flash` (raw, offsets from the start of the
    /// HyperFlash) for the slots and record of `map`, after `handoff`.
    /// The updater is returned either way, with this boot or why it did
    /// not start.
    pub fn boot<F: NorFlash>(
        flash: &mut F,
        map: AbMap,
        handoff: Option<Handoff>,
    ) -> (Self, Result<BootInfo, FwBootError<F::Error>>) {
        let Some(handoff) = handoff else {
            let state = AbState::new(Handoff {
                slot: middleware::fw_ota::Slot::A,
                trial: false,
            });
            return (
                Updater {
                    map,
                    state,
                    svc: None,
                },
                Err(FwBootError::NoHandoff),
            );
        };
        let mut state = AbState::new(handoff);
        let svc = RtPlatform::new(flash, &mut state, map)
            .and_then(|mut p| FwService::<RtImage>::boot_for(&mut p));
        match svc {
            Ok(s) => {
                let info = s.boot_info();
                (
                    Updater {
                        map,
                        state,
                        svc: Some(s),
                    },
                    Ok(info),
                )
            }
            Err(e) => (
                Updater {
                    map,
                    state,
                    svc: None,
                },
                Err(FwBootError::Platform(e)),
            ),
        }
    }

    /// Whether the updater started.
    pub const fn is_up(&self) -> bool {
        self.svc.is_some()
    }

    /// The update state (`None` when the updater did not start).
    pub fn state(&self) -> Option<FwState> {
        self.svc.as_ref().map(FwService::state)
    }

    /// The persisted record as last read or saved by the updater.
    pub fn record(&self) -> Option<FwRecord> {
        self.svc.as_ref().map(FwService::record)
    }

    /// This boot, as the updater saw it.
    pub fn boot_info(&self) -> Option<BootInfo> {
        self.svc.as_ref().map(FwService::boot_info)
    }

    /// The trial was confirmed during this boot.
    pub const fn trial_released(&self) -> bool {
        self.state.trial_released()
    }

    /// Handles one `sys/fw/*` request over the raw `flash`; the reply body
    /// goes into `out`.
    pub fn handle<F: NorFlash>(
        &mut self,
        flash: &mut F,
        ctx: &FwContext,
        key: Key<'_>,
        body: &[u8],
        out: &mut [u8],
    ) -> FwHandled {
        let Some(svc) = self.svc.as_mut() else {
            return FwHandled::reply(Err(HandlerError::Unavailable));
        };
        // With the record unreadable only requests that authorize nothing
        // get past this; the placeholder trust is never consulted by them
        // (the updater checks signatures at commit only).
        let Some(trust) = ctx
            .trust
            .or_else(|| (!needs_trust(key)).then_some(TrustState::Unclaimed))
        else {
            return FwHandled::reply(Err(HandlerError::Unavailable));
        };
        let released_before = self.state.trial_released();
        let reply = match RtPlatform::new(flash, &mut self.state, self.map) {
            Ok(mut p) => {
                let c = Context {
                    node: ctx.node,
                    chip: ctx.chip,
                    trust,
                    self_test_ok: ctx.self_test.ok(),
                };
                svc.handle(&mut p, &c, key, body, out)
                    .unwrap_or(Err(HandlerError::NotFound))
            }
            Err(_) => Err(HandlerError::Internal),
        };
        FwHandled {
            reply,
            reset: self.state.take_reset(),
            release_watchdog: !released_before && self.state.trial_released(),
        }
    }
}
