//! One request while a payload may be running: the routing the shell
//! performs, over any `wasm_host::Device`, so it is tested on the host
//! with the real interpreter (feature `wasm`, and in tests).
//!
//! [`handle`] asks `esp_base::serve::route_request` where the request goes
//! (the module table there): `sys/...` and keys nothing serves to the
//! [`Service`]; a key the running payload serves (its live table,
//! `Payload::serves`) to `Payload::request` with a reply buffer of
//! `esp_base::serve::REPLY_CAPACITY` bytes and the request's
//! [`RequestMeta`]; a key the last run served while none runs is
//! `Unavailable` ([`Service::route_idle`]).
//!
//! A payload's trap answers `Internal`: the service marks the payload
//! trapped, keeps its served keys ([`Service::run_ended`]) and the
//! [`Answer`] says the payload must stop, so the shell ends the run as for
//! any other trap. The shell calls [`ended`] whenever a run ends, so the
//! keys answer `Unavailable` until the next run or an upload.
//!
//! The serial link names no deadline (its frames carry none) and no
//! sender unless the link was given its peer, so a request over USB
//! reaches the payload with the meta the link has: the peer, if any, and
//! no deadline.

use embedded_storage::nor_flash::NorFlash;
use esp_base::serve::{REPLY_CAPACITY, Route, ServedKeys, payload_reply, route_request};
use middleware::Key;
use middleware::loader::PayloadStatus;
use middleware::transport::{HandlerError, RequestMeta};
use wasm_host::{Device, Payload, Trap};

use crate::fw_update::FwUpdate;
use crate::service::{Handled, Reply, Service};

/// What one request asks of the shell.
#[derive(Debug)]
pub struct Answer {
    /// The reply and storage effects, as [`Service::handle`] returns them;
    /// `stop` is also set when the payload trapped on the request.
    pub handled: Handled,
    /// The payload trapped while answering (the reply is `Internal`).
    pub trapped: Option<Trap>,
}

impl Answer {
    fn now(reply: Result<usize, HandlerError>) -> Self {
        Answer { handled: Handled { reply: Reply::Now(reply), stop: false, notes: Default::default() }, trapped: None }
    }
}

/// Handles one request; `payload` is the running payload, if any. A reply
/// body is written into `out` (at least `REPLY_CAPACITY` bytes are offered
/// to the payload when `out` has them).
pub fn handle<F: NorFlash, U: FwUpdate<F>, D: Device>(
    svc: &mut Service<F, U>,
    payload: Option<&mut Payload<D>>,
    key: Key<'_>,
    body: &[u8],
    meta: &RequestMeta<'_>,
    out: &mut [u8],
) -> Answer {
    let running = svc.status() == PayloadStatus::Running;
    let (route, payload) = match payload {
        Some(p) if running => (route_request(key, PayloadStatus::Running, p.serves()), Some(p)),
        _ => (svc.route_idle(key), None),
    };
    match (route, payload) {
        (Route::Payload, Some(p)) => {
            let cap = out.len().min(REPLY_CAPACITY);
            let result = p.request(key, body, meta, out.get_mut(..cap).unwrap_or_default());
            let trap = result.as_ref().err().cloned();
            let answered = payload_reply(result);
            if !answered.trapped {
                return Answer::now(answered.reply);
            }
            svc.trapped();
            ended(svc, p);
            let mut a = Answer::now(answered.reply);
            a.handled.stop = true;
            a.trapped = trap;
            a
        }
        (Route::Refuse(e), _) => Answer::now(Err(e)),
        // `Route::Payload` needs a running payload; without one the
        // service answers (NotFound), as for any key nothing serves.
        (Route::System | Route::Payload, _) => Answer { handled: svc.handle(key, body, out), trapped: None },
    }
}

/// A run ended: the service keeps the keys `payload` served.
pub fn ended<F: NorFlash, U: FwUpdate<F>, D: Device>(svc: &mut Service<F, U>, payload: &Payload<D>) {
    svc.run_ended(ServedKeys::from_run(payload.serves()));
}

#[cfg(test)]
mod tests;
