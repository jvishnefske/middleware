//! The node's own `sys/*` keys over Zenoh.
//!
//! `mwctl --zenoh <endpoint> --node <id>` and the HIL discovery ask the board
//! itself (`sys/info`, addressed to the node id), as they ask an RT1050 that
//! serves those keys natively. A posix-node serves them through its gateway:
//! this module declares one responder per allowed key under the node's own
//! id and forwards each request to the device the way `gw/sys` does, under
//! the gateway's rules (`SysAllow`: only the allowlisted keys, by default
//! `sys/info`, `sys/log` and `sys/manifest`). A Zenoh request carries no authenticated
//! principal, so it is anonymous: signed uploads and control still need an
//! updater (`gw/sys`, MW_GATEWAY_UPDATERS) and are `Denied` here.

use crate::shell::DirectHandle;
use middleware::Key;
use middleware::transport::{Handler, HandlerError, Request, Responder};
use middleware::zenoh::{ZenohError, ZenohTransport};
use mw_gateway::SysForward;
use mw_gateway::shell::SYS_ANSWER_MAX;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

/// The most a request may carry: the key and a body.
const REQUEST_MAX: usize = 4096;

/// Forwards a request on `key` to the device as `gw/sys` does:
/// `[key_len][key][body]`.
struct Forward {
    sys: SysForward,
    rt: Handle,
}

impl Handler for Forward {
    fn handle(&mut self, req: &Request<'_>, response: &mut [u8]) -> Result<usize, HandlerError> {
        let key = req.key.as_str().as_bytes();
        let len = u8::try_from(key.len()).map_err(|_| HandlerError::InvalidRequest)?;
        let mut body = Vec::with_capacity(1 + key.len() + req.body.len());
        body.push(len);
        body.extend_from_slice(key);
        body.extend_from_slice(req.body);
        let (sys, rt, meta) = (&mut self.sys, &self.rt, &req.meta);
        // The responder calls this synchronously from an async task on the
        // multi-thread runtime; the forward waits for the device.
        tokio::task::block_in_place(|| rt.block_on(sys.forward_as(meta, &body, response)))
    }
}

/// Asks the controller directly (no serial frame): the enrolment keys, whose
/// bodies (a certificate chain) are larger than a frame. A network request.
struct Straight {
    direct: DirectHandle,
    rt: Handle,
}

impl Handler for Straight {
    fn handle(&mut self, req: &Request<'_>, response: &mut [u8]) -> Result<usize, HandlerError> {
        let (direct, rt) = (&self.direct, &self.rt);
        let body = req.body.to_vec();
        let out =
            tokio::task::block_in_place(|| rt.block_on(direct.request(req.key.as_str(), body)))?;
        let dst = response
            .get_mut(..out.len())
            .ok_or(HandlerError::BufferTooSmall)?;
        dst.copy_from_slice(&out);
        Ok(out.len())
    }
}

/// Serves each of `keys` per node on `session` (opened with the node's own
/// sender id). The enrolment keys (`sys/cert/...`) go straight to the
/// controller through `direct`; the rest are forwarded to the device through
/// `sys`, under the gateway's rules. The tasks end with the session; abort
/// them to stop sooner. Needs the multi-thread runtime.
pub async fn serve(
    session: &ZenohTransport,
    sys: &SysForward,
    direct: &DirectHandle,
    keys: impl IntoIterator<Item = String>,
) -> Result<Vec<JoinHandle<()>>, String> {
    let mut tasks = Vec::new();
    for k in keys {
        // A handful of keys for the life of the process.
        let name: &'static str = Box::leak(k.into_boxed_str());
        let key = Key::new(name).map_err(|e| format!("{name}: {e:?}"))?;
        let mut responder = session
            .responder(key)
            .await
            .map_err(|e| format!("{name}: {e:?}"))?;
        let rt = Handle::current();
        if name.starts_with("sys/cert/") {
            let mut handler = Straight {
                direct: direct.clone(),
                rt,
            };
            tasks.push(tokio::spawn(async move {
                let mut scratch = vec![0u8; REQUEST_MAX + SYS_ANSWER_MAX];
                while !matches!(
                    responder.serve_one(&mut handler, &mut scratch).await,
                    Err(ZenohError::Closed)
                ) {}
            }));
        } else {
            let mut handler = Forward {
                sys: sys.clone(),
                rt,
            };
            tasks.push(tokio::spawn(async move {
                let mut scratch = vec![0u8; REQUEST_MAX + SYS_ANSWER_MAX];
                while !matches!(
                    responder.serve_one(&mut handler, &mut scratch).await,
                    Err(ZenohError::Closed)
                ) {}
            }));
        }
    }
    Ok(tasks)
}
