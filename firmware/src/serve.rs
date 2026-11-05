//! Requests on keys the payload serves (ABI 5): which requests go to the
//! payload, which the firmware answers itself, and what is left of a
//! payload's served keys once it no longer runs.
//!
//! A payload registers request keys with `serve` (wasm-host keeps the live
//! table, at most [`MAX_SERVES`] keys, and asks the device through
//! `Device::serve`, which accepts what [`servable`] accepts). For every
//! request the shell asks [`route_request`]:
//!
//! | key | payload | served keys | route |
//! |-----|---------|-------------|-------|
//! | `sys/...` | any | any | [`Route::System`] (`dispatch::handle_request`; a `sys` key can never be served) |
//! | served | `Running` | the payload's live table | [`Route::Payload`] |
//! | served | not running (stopped, trapped, loading) | the last run's ([`ServedKeys`]) | [`Route::Refuse`]`(Unavailable)` |
//! | other | any | — | [`Route::System`] (`NotFound`, as before ABI 5) |
//!
//! [`Route::Payload`] calls `wasm_host::Payload::request` with a reply
//! buffer of [`REPLY_CAPACITY`] bytes and the request's `RequestMeta` (the
//! link's peer as sender, the transport's deadline); [`payload_reply`]
//! turns its result into the answer: the payload's reply or
//! `HandlerError`, and for a trap `Internal` and the payload stops (the
//! shell ends the session, as for any trap).
//!
//! The firmware keeps the keys of the run that ended ([`ServedKeys`],
//! [`ServedKeys::from_run`]) so that a caller of a stopped or trapped
//! payload hears `Unavailable`, which the S88 engine treats as resumable
//! (it holds the batch), rather than `NotFound`, which would abort it as
//! a wrong address. The table is cleared when the image changes
//! ([`served_after`]: a serial `begin`, which stops the payload and erases
//! the slot, and an activated commit); the next run starts with an empty
//! live table and replaces it when it ends.
//!
//! Everything here is pure; the shells (esp-base `shell/controller.rs`,
//! rp-base `rp_base::requests`) perform it.

use heapless::{String, Vec};
use middleware::Key;
use middleware::key::MAX_KEY_LEN;
use middleware::loader::{PayloadStatus, is_system_key};
use middleware::transport::HandlerError;
use middleware::wasm_abi::AbiError;
pub use middleware::wasm_abi::serve::{MAX_REPLY, MAX_SERVES};

use crate::dispatch::Action;

/// Reply bytes the shell offers a payload's `on_request` (the ABI's
/// [`MAX_REPLY`], 512).
pub const REPLY_CAPACITY: usize = MAX_REPLY;

/// Whether a payload may serve `key`: never a `sys/...` key (the
/// firmware's own; they are answered by the dispatcher whatever a payload
/// asks). The device's `serve` answers [`AbiError::InvalidKey`] otherwise.
pub fn servable(key: Key<'_>) -> Result<(), AbiError> {
    if is_system_key(key) { Err(AbiError::InvalidKey) } else { Ok(()) }
}

/// The served keys of the payload run that ended, at most
/// [`MAX_SERVES`], kept so their requests answer `Unavailable`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServedKeys(Vec<String<MAX_KEY_LEN>, MAX_SERVES>);

impl ServedKeys {
    /// No keys.
    pub const fn new() -> Self {
        ServedKeys(Vec::new())
    }

    /// The table of a run that served `keys` (wasm-host's live table, in
    /// slot order). Keys a payload could not have served (`sys/...`,
    /// over-long) and any beyond [`MAX_SERVES`] are dropped; repeats are
    /// kept once.
    pub fn from_run<'s>(keys: impl Iterator<Item = &'s str>) -> Self {
        let mut table = Vec::new();
        for key in keys.take(MAX_SERVES) {
            let ok = Key::new(key).is_ok_and(|k| servable(k).is_ok());
            if !ok || table.iter().any(|k: &String<MAX_KEY_LEN>| k.as_str() == key) {
                continue;
            }
            let mut s = String::new();
            if s.push_str(key).is_ok() {
                // Fewer than MAX_SERVES keys were taken, so it fits.
                let _ = table.push(s);
            }
        }
        ServedKeys(table)
    }

    /// The keys, in slot order.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|s| s.as_str())
    }

    /// Number of keys.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Where one request goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// The firmware's dispatcher (`sys/...`, and keys nothing serves,
    /// which it answers `NotFound`).
    System,
    /// The running payload's `on_request`.
    Payload,
    /// Answer this error without asking anyone (a served key of a payload
    /// that does not run).
    Refuse(HandlerError),
}

/// Routes a request on `key`: `served` is the running payload's live
/// table while `status` is `Running`, otherwise the last run's
/// ([`ServedKeys`]). See the module docs for the table.
pub fn route_request<'s>(key: Key<'_>, status: PayloadStatus, mut served: impl Iterator<Item = &'s str>) -> Route {
    if is_system_key(key) || !served.any(|s| s == key.as_str()) {
        return Route::System;
    }
    match status {
        PayloadStatus::Running => Route::Payload,
        _ => Route::Refuse(HandlerError::Unavailable),
    }
}

/// What the shell answers for a payload's request result, and whether the
/// payload trapped (the shell then marks it trapped and ends the run).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadReply {
    /// The reply length in the reply buffer, or the error to send.
    pub reply: Result<usize, HandlerError>,
    /// The payload trapped (or was already poisoned).
    pub trapped: bool,
}

/// The answer for `Payload::request`'s result: `Ok(r)` is the payload's
/// reply or `HandlerError` as it is; a trap (`Err`) is `Internal` and
/// stops the payload.
pub fn payload_reply<T>(result: Result<Result<usize, HandlerError>, T>) -> PayloadReply {
    match result {
        Ok(reply) => PayloadReply { reply, trapped: false },
        Err(_) => PayloadReply { reply: Err(HandlerError::Internal), trapped: true },
    }
}

/// The served keys after `action` succeeded: none once the image changes
/// (a serial `begin`'s erase, which stops the payload, and an activated
/// commit); otherwise unchanged. A staged `begin` beside the running
/// payload keeps them: the running image still serves its keys.
pub fn served_after(action: &Action<'_>, served: ServedKeys) -> ServedKeys {
    match action {
        Action::Erase { stop: true, .. } | Action::Activate { .. } => ServedKeys::new(),
        _ => served,
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    const PHASE: &str = "unit/e6613852831f2a0b/phase/LED.Blink";

    fn k(s: &'static str) -> Key<'static> {
        Key::from_static(s)
    }

    #[test]
    fn m7_sys_keys_always_go_to_the_dispatcher_and_can_never_be_served() {
        let served = ["sys/info", PHASE];
        for status in [PayloadStatus::Running, PayloadStatus::Stopped, PayloadStatus::Trapped] {
            assert_eq!(route_request(k("sys/info"), status, served.iter().copied()), Route::System);
            assert_eq!(route_request(k("sys/payload/stop"), status, served.iter().copied()), Route::System);
        }
        assert_eq!(servable(k("sys/info")), Err(AbiError::InvalidKey));
        assert_eq!(servable(k("sys")), Err(AbiError::InvalidKey));
        assert_eq!(servable(k(PHASE)), Ok(()));
        assert_eq!(servable(k("system/x")), Ok(()));
        // Even a table claiming a sys key keeps none.
        let t = ServedKeys::from_run(["sys/info", "sys/log"].into_iter());
        assert!(t.is_empty());
    }

    #[test]
    fn m7_served_keys_reach_a_running_payload_and_others_are_not_found() {
        let served = [PHASE];
        let run = |key, status| route_request(k(key), status, served.iter().copied());
        assert_eq!(run(PHASE, PayloadStatus::Running), Route::Payload);
        // Exact match only: keys have no wildcards.
        assert_eq!(run("unit/e6613852831f2a0b/phase/LED.Blink/state", PayloadStatus::Running), Route::System);
        assert_eq!(run("unit/e6613852831f2a0b/phase", PayloadStatus::Running), Route::System);
        assert_eq!(run("app/ping", PayloadStatus::Running), Route::System);
        // Nothing served: the dispatcher answers NotFound, as before.
        assert_eq!(route_request(k(PHASE), PayloadStatus::Running, [].iter().copied()), Route::System);
    }

    #[test]
    fn m7_a_stopped_trapped_or_loading_payload_is_unavailable_on_its_keys() {
        let last = ServedKeys::from_run([PHASE].into_iter());
        for status in [PayloadStatus::Stopped, PayloadStatus::Trapped, PayloadStatus::Loading, PayloadStatus::Empty] {
            assert_eq!(route_request(k(PHASE), status, last.iter()), Route::Refuse(HandlerError::Unavailable), "{status:?}");
            assert_eq!(route_request(k("app/other"), status, last.iter()), Route::System);
        }
    }

    #[test]
    fn m7_a_trap_answers_internal_and_stops_the_payload() {
        struct Trap;
        assert_eq!(payload_reply::<Trap>(Ok(Ok(9))), PayloadReply { reply: Ok(9), trapped: false });
        assert_eq!(
            payload_reply::<Trap>(Ok(Err(HandlerError::InvalidRequest))),
            PayloadReply { reply: Err(HandlerError::InvalidRequest), trapped: false }
        );
        assert_eq!(payload_reply::<Trap>(Err(Trap)), PayloadReply { reply: Err(HandlerError::Internal), trapped: true });
        assert_eq!(REPLY_CAPACITY, 512);
    }

    #[test]
    fn m7_the_table_is_bounded_deduplicated_and_cleared_when_the_image_changes() {
        let many: std::vec::Vec<std::string::String> = (0..12).map(|i| std::format!("app/k{i}")).collect();
        let t = ServedKeys::from_run(many.iter().map(|s| s.as_str()));
        assert_eq!(t.len(), MAX_SERVES);
        assert_eq!(t.iter().next(), Some("app/k0"));
        let t2 = ServedKeys::from_run(["app/a", "app/a", "app/b"].into_iter());
        assert_eq!(t2.iter().collect::<std::vec::Vec<_>>(), ["app/a", "app/b"]);
        // Upload clears; a staged begin, writes, start and stop keep them.
        assert!(served_after(&Action::Erase { len: 4, stop: true }, t2.clone()).is_empty());
        assert!(served_after(&Action::Activate { len: 4, crc: 0, trust: None }, t2.clone()).is_empty());
        for keep in [Action::Erase { len: 4, stop: false }, Action::Write { offset: 0, data: &[1] }, Action::Stop, Action::Start, Action::None] {
            assert_eq!(served_after(&keep, t2.clone()), t2, "{keep:?}");
        }
    }
}
