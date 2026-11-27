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
//!
//! Two groups go straight to the controller instead, as network requests,
//! because their bodies do not fit a serial frame (1 KiB): the enrolment
//! keys (`sys/cert/...`, a certificate chain) and, on a node with a Linux
//! A/B root, the root's keys (`sys/os/...`, [`OS_KEYS`]: 64 KiB chunks). The
//! controller applies their own rules: a signed control (`reboot`,
//! `confirm`, `abort`) and the root-signed cert at `commit` on a claimed
//! node (posix-node `os.rs`).

use std::time::Duration;

use crate::shell::{DIRECT_WAIT, DirectHandle};
use middleware::Key;
use middleware::os_ab::wire::{CHUNK_LEN, keys as os_keys};
use middleware::transport::{Handler, HandlerError, Request, Responder};
use middleware::zenoh::{ZenohError, ZenohTransport};
use mw_gateway::SysForward;
use mw_gateway::shell::SYS_ANSWER_MAX;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

/// The most a request may carry: the key and a body.
const REQUEST_MAX: usize = 4096;
/// The most a `sys/os/...` request carries: a chunk, `[offset][data]`.
const OS_REQUEST_MAX: usize = 4 + CHUNK_LEN;
/// The longest a direct request waits for the controller, whatever the
/// caller's deadline: a `commit` reads the written slot back to hash it
/// (a few GB from a USB stick).
const DIRECT_WAIT_MAX: Duration = Duration::from_secs(15 * 60);

/// The Linux A/B root's keys: served per node when the node has a root
/// (`MW_POSIX_NODE_OS_*`), beside the gateway's allowlist.
pub const OS_KEYS: [Key<'static>; 7] = os_keys::ALL;

/// Whether `key` goes straight to the controller rather than through the
/// gateway's serial frame.
fn is_direct(key: &str) -> bool {
    key.starts_with("sys/cert/") || key.starts_with("sys/os/")
}

/// The scratch a responder for `key` needs: the request and the answer.
fn scratch_len(key: &str) -> usize {
    let request = if key.starts_with("sys/os/") {
        OS_REQUEST_MAX
    } else {
        REQUEST_MAX
    };
    request + SYS_ANSWER_MAX
}

/// How long a direct request waits: the caller's deadline when it sent
/// one (at most [`DIRECT_WAIT_MAX`]), else [`DIRECT_WAIT`].
fn direct_wait(deadline_ms: Option<u32>) -> Duration {
    deadline_ms.map_or(DIRECT_WAIT, |ms| {
        Duration::from_millis(u64::from(ms)).min(DIRECT_WAIT_MAX)
    })
}

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

/// Asks the controller directly (no serial frame): the enrolment and root
/// keys, whose bodies are larger than a frame. A network request.
struct Straight {
    direct: DirectHandle,
    rt: Handle,
}

impl Handler for Straight {
    fn handle(&mut self, req: &Request<'_>, response: &mut [u8]) -> Result<usize, HandlerError> {
        let (direct, rt) = (&self.direct, &self.rt);
        let body = req.body.to_vec();
        let wait = direct_wait(req.meta.deadline_ms);
        let out = tokio::task::block_in_place(|| {
            rt.block_on(direct.request_within(req.key.as_str(), body, wait))
        })?;
        let dst = response
            .get_mut(..out.len())
            .ok_or(HandlerError::BufferTooSmall)?;
        dst.copy_from_slice(&out);
        Ok(out.len())
    }
}

/// Serves each of `keys` per node on `session` (opened with the node's own
/// sender id). The enrolment keys (`sys/cert/...`) and the root's
/// (`sys/os/...`) go straight to the controller through `direct`; the rest
/// are forwarded to the device through `sys`, under the gateway's rules. A
/// key listed twice is served once. The tasks end with the session; abort
/// them to stop sooner. Needs the multi-thread runtime.
pub async fn serve(
    session: &ZenohTransport,
    sys: &SysForward,
    direct: &DirectHandle,
    keys: impl IntoIterator<Item = String>,
) -> Result<Vec<JoinHandle<()>>, String> {
    let mut tasks = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for k in keys {
        if !seen.insert(k.clone()) {
            continue;
        }
        // A handful of keys for the life of the process.
        let name: &'static str = Box::leak(k.into_boxed_str());
        let key = Key::new(name).map_err(|e| format!("{name}: {e:?}"))?;
        let mut responder = session
            .responder(key)
            .await
            .map_err(|e| format!("{name}: {e:?}"))?;
        let rt = Handle::current();
        let scratch_len = scratch_len(name);
        if is_direct(name) {
            let mut handler = Straight {
                direct: direct.clone(),
                rt,
            };
            tasks.push(tokio::spawn(async move {
                let mut scratch = vec![0u8; scratch_len];
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
                let mut scratch = vec![0u8; scratch_len];
                while !matches!(
                    responder.serve_one(&mut handler, &mut scratch).await,
                    Err(ZenohError::Closed)
                ) {}
            }));
        }
    }
    Ok(tasks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_root_and_enrolment_keys_go_straight_to_the_controller() {
        for k in OS_KEYS {
            assert!(is_direct(k.as_str()), "{}", k.as_str());
        }
        assert!(is_direct("sys/cert/install"));
        for k in ["sys/info", "sys/log", "sys/manifest", "sys/trust/claim"] {
            assert!(!is_direct(k), "{k}");
        }
    }

    #[test]
    fn a_full_chunk_and_its_answer_fit_the_scratch() {
        let chunk = 4 + CHUNK_LEN;
        assert!(scratch_len(os_keys::CHUNK.as_str()) >= chunk + SYS_ANSWER_MAX);
        // The other keys keep the small scratch.
        assert_eq!(scratch_len("sys/info"), REQUEST_MAX + SYS_ANSWER_MAX);
    }

    #[test]
    fn a_direct_request_waits_for_the_caller_within_a_bound() {
        assert_eq!(direct_wait(None), DIRECT_WAIT);
        assert_eq!(direct_wait(Some(120_000)), Duration::from_secs(120));
        assert_eq!(direct_wait(Some(u32::MAX)), DIRECT_WAIT_MAX);
    }
}
