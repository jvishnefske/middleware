//! The seam for firmware updates: the reserved `sys/fw/*` keys, the
//! backend trait, what a backend is given with each request, and the
//! self-test that gates confirming a trial.
//!
//! * Every key under [`PREFIX`] belongs to the backend. The request
//!   service ([`crate::service`]) hands such requests to its [`FwUpdate`]
//!   before the payload dispatcher sees them, so they never touch the
//!   payload loader, the payload store or the payload status.
//! * The backend gets the flash (lent by the payload store, `None` when the
//!   store could not be opened), a [`FwContext`] (node id, the trust
//!   store's root of trust, the [`SelfTest`]), the key, the body and the
//!   reply buffer, and returns the reply length or a [`HandlerError`]. It
//!   is a generic parameter of the service (monomorphised, no `dyn`).
//!
//! Three backends:
//!
//! * [`crate::ota::AbUpdate`] (cargo feature `fw-update`, the RP2350
//!   build): A/B firmware OTA, `middleware::fw_ota` on the RP2350;
//! * [`Unsupported`]: every `sys/fw/*` key is reserved and answered
//!   [`HandlerError::Unavailable`], "exists but cannot serve" (a stub for
//!   tests and boards without the updater);
//! * [`Absent`] (without `fw-update`, the RP2040 build, whose ROM has no
//!   A/B boot): the keys are not reserved and answer
//!   [`HandlerError::NotFound`], as any unknown `sys/` key does.
//!
//! The shell picks the backend in one place, the `Fw` type in
//! `src/shell/board.rs`.

use middleware::Key;
use middleware::SenderId;
use middleware::loader::trust::TrustState;
use middleware::transport::HandlerError;

/// Prefix of every firmware-update key.
pub const PREFIX: &str = "sys/fw/";

/// The keys of the A/B updater (`middleware::fw_ota::keys`).
pub use middleware::fw_ota::keys;

/// Whether `key` is a firmware-update key.
pub fn is_fw_key(key: Key<'_>) -> bool {
    key.as_str().starts_with(PREFIX)
}

/// The firmware's own health check, which must pass before a trial image
/// may be bought (`sys/fw/confirm`): the new firmware reached the state in
/// which it can be updated again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SelfTest {
    /// USB is configured by the host (the CDC port that carries updates).
    pub usb_configured: bool,
    /// The payload store opened (its flash region is readable; it lends
    /// the flash to the updater).
    pub store_open: bool,
    /// The trust record opened (the root of trust that authorizes the
    /// next update is readable).
    pub trust_open: bool,
}

impl SelfTest {
    /// Every check passed.
    pub const fn ok(&self) -> bool {
        self.usb_configured && self.store_open && self.trust_open
    }
}

/// What the backend is given with every request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FwContext {
    /// The node id `sys/info` reports (bound into firmware signatures).
    pub node: SenderId,
    /// The root of trust (the pinned payload key signs firmware too).
    pub trust: TrustState,
    /// The self-test.
    pub self_test: SelfTest,
}

/// A firmware-update backend over flash `F`.
pub trait FwUpdate<F> {
    /// Handles one `sys/fw/*` request; the reply body goes into `out`.
    fn handle(
        &mut self,
        flash: Option<&mut F>,
        ctx: &FwContext,
        key: Key<'_>,
        body: &[u8],
        out: &mut [u8],
    ) -> Result<usize, HandlerError>;
}

/// The stub backend: every `sys/fw/*` key is reserved but not served.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Unsupported;

impl<F> FwUpdate<F> for Unsupported {
    fn handle(&mut self, _: Option<&mut F>, _: &FwContext, _: Key<'_>, _: &[u8], _: &mut [u8]) -> Result<usize, HandlerError> {
        Err(HandlerError::Unavailable)
    }
}

/// No backend: `sys/fw/*` keys are unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Absent;

impl<F> FwUpdate<F> for Absent {
    fn handle(&mut self, _: Option<&mut F>, _: &FwContext, _: Key<'_>, _: &[u8], _: &mut [u8]) -> Result<usize, HandlerError> {
        Err(HandlerError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v6_the_reserved_keys_are_under_the_prefix() {
        for k in keys::ALL {
            assert!(is_fw_key(k), "{}", k.as_str());
            assert!(middleware::loader::is_system_key(k));
        }
        assert!(!is_fw_key(middleware::loader::keys::INFO));
        assert!(!is_fw_key(Key::from_static("sys/fwx")));
        assert!(!is_fw_key(Key::from_static("app/sys/fw/info")));
        let mut out = [0u8; 4];
        let ctx = FwContext { node: SenderId([0; 8]), trust: TrustState::Unclaimed, self_test: SelfTest::default() };
        let r = FwUpdate::<()>::handle(&mut Unsupported, None, &ctx, keys::BEGIN, b"x", &mut out);
        assert_eq!(r, Err(HandlerError::Unavailable));
        let r = FwUpdate::<()>::handle(&mut Absent, None, &ctx, keys::BEGIN, b"x", &mut out);
        assert_eq!(r, Err(HandlerError::NotFound));
    }

    #[test]
    fn y12_the_self_test_needs_usb_the_store_and_the_trust_record() {
        let all = SelfTest { usb_configured: true, store_open: true, trust_open: true };
        assert!(all.ok());
        assert!(!SelfTest::default().ok());
        for broken in [
            SelfTest { usb_configured: false, ..all },
            SelfTest { store_open: false, ..all },
            SelfTest { trust_open: false, ..all },
        ] {
            assert!(!broken.ok(), "{broken:?}");
        }
    }
}
