//! The replay cache: a resent update request runs once.
//!
//! The update client sends every mutating request with an `x-request-id`
//! and resends a request that timed out (or met a busy gateway) with the
//! **same** id, as `middleware::rpc` requesters do over the air (O3). The
//! gateway keys each forwarded request by (principal, request id) and
//! remembers a fingerprint of it (key, body length, body CRC-32):
//!
//! * a repeat while the first is still at the device is
//!   [`Lookup::InFlight`] (the shell answers `Unavailable`, retriable);
//! * a repeat after the device answered is [`Lookup::Replay`]: the cached
//!   answer is returned and nothing reaches the device again;
//! * the same id with a different request is [`Lookup::Mismatch`]
//!   (`InvalidRequest`);
//! * anything else is [`Lookup::Fresh`].
//!
//! The principal is part of the key, so one caller can never be answered
//! with another's response. A request id alone is the caller's claim and
//! is never used for policy; here it only names the caller's own retry.
//!
//! Bounds: at most [`REPLAY_SLOTS`] entries, each kept [`REPLAY_TTL_MS`]
//! after it was recorded or answered; a full cache drops its oldest
//! answered entry (never one in flight: at most `MAX_PENDING` requests are
//! at the device, fewer than the slots). Answers longer than
//! [`REPLAY_BODY_MAX`] are not kept (the repeat then reaches the device
//! again; every update answer is far smaller).

use middleware::crc::Crc32;
use middleware::key::Key;
use middleware::transport::HandlerError;

/// Entries kept.
pub const REPLAY_SLOTS: usize = 8;
/// How long an entry is kept, in milliseconds.
pub const REPLAY_TTL_MS: u64 = 120_000;
/// Largest answer kept.
pub const REPLAY_BODY_MAX: usize = 256;

/// What identifies one request of a caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    key: String,
    len: usize,
    crc: u32,
}

impl Fingerprint {
    /// The fingerprint of a request on `key` with `body`.
    pub fn of(key: Key<'_>, body: &[u8]) -> Self {
        Fingerprint {
            key: key.as_str().to_owned(),
            len: body.len(),
            crc: Crc32::checksum(body),
        }
    }
}

/// An entry's state.
#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    /// Forwarded; no answer yet.
    InFlight,
    /// The device's answer.
    Done(Result<Vec<u8>, HandlerError>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    principal: String,
    request_id: String,
    print: Fingerprint,
    state: State,
    at_ms: u64,
}

/// What the cache says about a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// Not seen: decide and forward it.
    Fresh,
    /// The first copy is still at the device.
    InFlight,
    /// Answered before: this is the answer.
    Replay(Result<Vec<u8>, HandlerError>),
    /// The id names a different request of this caller.
    Mismatch,
}

/// The cache (module docs).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Replay(Vec<Entry>);

impl Replay {
    /// Empty.
    pub fn new() -> Self {
        Self::default()
    }

    /// Entries held.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn find(&self, principal: &str, request_id: &str) -> Option<&Entry> {
        self.0
            .iter()
            .find(|e| e.principal == principal && e.request_id == request_id)
    }

    /// Looks a request up (module docs). Pure.
    pub fn lookup(&self, principal: &str, request_id: &str, print: &Fingerprint) -> Lookup {
        match self.find(principal, request_id) {
            None => Lookup::Fresh,
            Some(e) if &e.print != print => Lookup::Mismatch,
            Some(Entry {
                state: State::InFlight,
                ..
            }) => Lookup::InFlight,
            Some(Entry {
                state: State::Done(r),
                ..
            }) => Lookup::Replay(r.clone()),
        }
    }

    /// Drops the entries older than [`REPLAY_TTL_MS`] at `now_ms`.
    pub fn expire(self, now_ms: u64) -> Self {
        let mut v = self.0;
        v.retain(|e| now_ms.saturating_sub(e.at_ms) < REPLAY_TTL_MS);
        Replay(v)
    }

    /// Records a forwarded request as in flight, making room as the
    /// module docs say.
    pub fn record(
        self,
        principal: &str,
        request_id: &str,
        print: Fingerprint,
        now_ms: u64,
    ) -> Self {
        let mut v = self.expire(now_ms).0;
        v.retain(|e| !(e.principal == principal && e.request_id == request_id));
        if v.len() >= REPLAY_SLOTS
            && let Some(oldest) = v
                .iter()
                .enumerate()
                .filter(|(_, e)| matches!(e.state, State::Done(_)))
                .min_by_key(|(_, e)| e.at_ms)
                .map(|(i, _)| i)
        {
            v.remove(oldest);
        }
        if v.len() < REPLAY_SLOTS {
            v.push(Entry {
                principal: principal.to_owned(),
                request_id: request_id.to_owned(),
                print,
                state: State::InFlight,
                at_ms: now_ms,
            });
        }
        Replay(v)
    }

    /// Stores the device's answer for an in-flight request; an answer
    /// over [`REPLAY_BODY_MAX`] drops the entry instead.
    pub fn answer(
        self,
        principal: &str,
        request_id: &str,
        result: Result<&[u8], HandlerError>,
        now_ms: u64,
    ) -> Self {
        let mut v = self.0;
        let at = v
            .iter()
            .position(|e| e.principal == principal && e.request_id == request_id);
        if let Some(i) = at {
            match result {
                Ok(body) if body.len() > REPLAY_BODY_MAX => {
                    v.remove(i);
                }
                r => {
                    if let Some(e) = v.get_mut(i) {
                        e.state = State::Done(r.map(<[u8]>::to_vec));
                        e.at_ms = now_ms;
                    }
                }
            }
        }
        Replay(v)
    }

    /// Forgets a request the device never answered, so the caller's
    /// resend is forwarded again.
    pub fn forget(self, principal: &str, request_id: &str) -> Self {
        let mut v = self.0;
        v.retain(|e| !(e.principal == principal && e.request_id == request_id));
        Replay(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::loader::keys;

    const A: &str = "spiffe://infra.local/ns/ops/sa/updater";
    const B: &str = "spiffe://infra.local/ns/ops/sa/other";

    #[test]
    fn rg21_a_request_runs_once_and_repeats_replay_its_answer() {
        let p = Fingerprint::of(keys::CHUNK, b"\0\0\0\0abc");
        let r = Replay::new();
        assert_eq!(r.lookup(A, "u-1", &p), Lookup::Fresh);
        let r = r.record(A, "u-1", p.clone(), 0);
        assert_eq!(r.lookup(A, "u-1", &p), Lookup::InFlight);
        // Another caller's identical id is a different request.
        assert_eq!(r.lookup(B, "u-1", &p), Lookup::Fresh);
        // The same id with another body is refused.
        let q = Fingerprint::of(keys::CHUNK, b"\0\0\0\0abd");
        assert_eq!(r.lookup(A, "u-1", &q), Lookup::Mismatch);
        let r = r.answer(A, "u-1", Ok(b""), 10);
        assert_eq!(r.lookup(A, "u-1", &p), Lookup::Replay(Ok(Vec::new())));
        let r = r
            .record(A, "u-2", q.clone(), 20)
            .answer(A, "u-2", Err(HandlerError::Denied), 30);
        assert_eq!(
            r.lookup(A, "u-2", &q),
            Lookup::Replay(Err(HandlerError::Denied))
        );
        // Unanswered requests are forgotten, so the resend goes out again.
        let r = r.record(A, "u-3", p.clone(), 40).forget(A, "u-3");
        assert_eq!(r.lookup(A, "u-3", &p), Lookup::Fresh);
        // Large answers are not kept.
        let big = vec![0u8; REPLAY_BODY_MAX + 1];
        let r = r
            .record(A, "u-4", p.clone(), 50)
            .answer(A, "u-4", Ok(&big), 60);
        assert_eq!(r.lookup(A, "u-4", &p), Lookup::Fresh);
    }

    #[test]
    fn rg21_the_cache_is_bounded_and_expires() {
        let p = Fingerprint::of(keys::BEGIN, b"x");
        let mut r = Replay::new();
        for i in 0..REPLAY_SLOTS + 3 {
            let id = format!("u-{i}");
            r = r
                .record(A, &id, p.clone(), i as u64)
                .answer(A, &id, Ok(b""), i as u64);
            assert!(r.len() <= REPLAY_SLOTS);
        }
        // The oldest answered went first.
        assert_eq!(r.lookup(A, "u-0", &p), Lookup::Fresh);
        assert!(matches!(
            r.lookup(A, &format!("u-{}", REPLAY_SLOTS + 2), &p),
            Lookup::Replay(_)
        ));
        // In-flight entries are never displaced.
        let mut f = Replay::new();
        for i in 0..REPLAY_SLOTS {
            f = f.record(A, &format!("f-{i}"), p.clone(), 0);
        }
        let f = f.record(A, "f-x", p.clone(), 1);
        assert_eq!(f.len(), REPLAY_SLOTS);
        assert_eq!(f.lookup(A, "f-0", &p), Lookup::InFlight);
        assert_eq!(f.lookup(A, "f-x", &p), Lookup::Fresh);
        // Entries age out.
        let r = r.expire(REPLAY_SLOTS as u64 + 3 + REPLAY_TTL_MS);
        assert!(r.is_empty());
    }
}
