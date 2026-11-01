//! The seam for A/B firmware updates (RP2350): the reserved `sys/fw/*`
//! keys and the trait an update backend implements.
//!
//! Firmware OTA (writing the inactive firmware slot of
//! [`crate::layout::RP2350`], verifying it and asking the boot ROM to try
//! it) is built separately (branch `fw-ab-ota`). This module fixes only
//! what the base firmware must agree on with it:
//!
//! * every key under [`PREFIX`] belongs to the backend. The request service
//!   ([`crate::service`]) hands such requests to its [`FwUpdate`] before the
//!   payload dispatcher sees them, so they never touch the payload loader,
//!   the payload store or the payload status;
//! * the keys the backend is expected to serve are named here
//!   ([`keys`]); their bodies are the backend's to define;
//! * the backend gets the key, the body and the reply buffer, and returns
//!   the reply length or a [`HandlerError`], like any request handler. It
//!   is a generic parameter of the service (monomorphised, no `dyn`).
//!
//! Two backends ship with the base firmware:
//!
//! * [`Unsupported`] (cargo feature `fw-update`, on for the RP2350 build):
//!   every `sys/fw/*` key is reserved and answered
//!   [`HandlerError::Unavailable`], "exists but cannot serve". The protocol
//!   has no `Unsupported` status (`HandlerError` is a closed wire enum), and
//!   `Unavailable` keeps a reserved key distinguishable from an unknown one;
//! * [`Absent`] (without `fw-update`, the RP2040 build, whose ROM has no
//!   A/B boot): the keys are not reserved and answer
//!   [`HandlerError::NotFound`], as any unknown `sys/` key does.
//!
//! The shell picks the backend in one place, the `Fw` type in
//! `src/shell/board.rs`; the A/B OTA work replaces [`Unsupported`] there
//! with its own [`FwUpdate`] implementation, and nothing else changes.

use middleware::Key;
use middleware::transport::HandlerError;

/// Prefix of every firmware-update key.
pub const PREFIX: &str = "sys/fw/";

/// The keys the A/B backend is expected to serve (reserved now).
pub mod keys {
    use middleware::Key;

    /// Running slot, versions and update state.
    pub const INFO: Key<'static> = Key::from_static("sys/fw/info");
    /// Starts writing an image into the inactive slot.
    pub const BEGIN: Key<'static> = Key::from_static("sys/fw/begin");
    /// One image chunk.
    pub const CHUNK: Key<'static> = Key::from_static("sys/fw/chunk");
    /// Verifies the image and marks the slot to try on the next boot.
    pub const COMMIT: Key<'static> = Key::from_static("sys/fw/commit");
    /// Confirms the running (tried) image so the boot ROM keeps it.
    pub const CONFIRM: Key<'static> = Key::from_static("sys/fw/confirm");

    /// Every reserved key.
    pub const ALL: [Key<'static>; 5] = [INFO, BEGIN, CHUNK, COMMIT, CONFIRM];
}

/// Whether `key` is a firmware-update key.
pub fn is_fw_key(key: Key<'_>) -> bool {
    key.as_str().starts_with(PREFIX)
}

/// A firmware-update backend.
pub trait FwUpdate {
    /// Handles one `sys/fw/*` request; the reply body goes into `out`.
    fn handle(&mut self, key: Key<'_>, body: &[u8], out: &mut [u8]) -> Result<usize, HandlerError>;
}

/// The stub backend: every `sys/fw/*` key is reserved but not served.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Unsupported;

impl FwUpdate for Unsupported {
    fn handle(&mut self, _key: Key<'_>, _body: &[u8], _out: &mut [u8]) -> Result<usize, HandlerError> {
        Err(HandlerError::Unavailable)
    }
}

/// No backend: `sys/fw/*` keys are unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Absent;

impl FwUpdate for Absent {
    fn handle(&mut self, _key: Key<'_>, _body: &[u8], _out: &mut [u8]) -> Result<usize, HandlerError> {
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
        assert_eq!(Unsupported.handle(keys::BEGIN, b"x", &mut out), Err(HandlerError::Unavailable));
        assert_eq!(Absent.handle(keys::BEGIN, b"x", &mut out), Err(HandlerError::NotFound));
    }
}
