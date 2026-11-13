//! [`MeshSysCall`]: the thin I/O shell of [`super::SysCall`], a
//! `MeshClient` for `gw/sys` through a static endpoint (`mw-service`'s
//! `StaticEndpoint`: the dial address, usually the local Envoy, and the
//! gateway's authority).
//!
//! `mwctl`'s operations are synchronous, so the call owns a current-thread
//! tokio runtime and blocks on it: build and use it outside any async
//! context (a plain thread, or `tokio::task::spawn_blocking`).

use std::time::Duration;

use middleware::key::Key;
use middleware::transport::{RequestId, RequestOptions, Requester};
use mw_service::DownstreamClient;
use mw_service::endpoints::StaticEndpoint;

use super::{CallError, SysCall};
use crate::shell::{SYS_ANSWER_MAX, SYS_KIND};

/// Extra time the local wait allows beyond the deadline sent, so the
/// gateway's `DEADLINE_EXCEEDED` arrives before the local timer fires.
const SLACK: Duration = Duration::from_millis(500);

/// `gw/sys` over the mesh (module docs).
pub struct MeshSysCall {
    runtime: tokio::runtime::Runtime,
    client: DownstreamClient,
    pause: Duration,
}

impl std::fmt::Debug for MeshSysCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeshSysCall").finish_non_exhaustive()
    }
}

impl MeshSysCall {
    /// Calls through `endpoint`, pausing `pause` before a resend. The
    /// connection is made on the first call.
    pub fn new(endpoint: &StaticEndpoint, pause: Duration) -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        Ok(MeshSysCall {
            runtime,
            client: endpoint.client(),
            pause,
        })
    }
}

impl SysCall for MeshSysCall {
    fn call(
        &mut self,
        body: &[u8],
        request_id: &str,
        timeout: Duration,
    ) -> Result<Vec<u8>, CallError> {
        let id = RequestId::new(request_id)
            .ok_or_else(|| CallError::Transport(format!("bad request id {request_id:?}")))?;
        let options = RequestOptions::DEFAULT
            .with_timeout(timeout)
            .with_request_id(id);
        let key = Key::from_static(SYS_KIND);
        let client = &mut self.client;
        self.runtime.block_on(async {
            let mut out = vec![0u8; SYS_ANSWER_MAX];
            let call = client.request_with(key, body, &mut out, &options);
            match tokio::time::timeout(timeout + SLACK, call).await {
                Ok(Ok(answer)) => Ok(answer.to_vec()),
                Ok(Err(e)) => Err(match e.handler_error() {
                    Some(status) => CallError::Status(status),
                    None => CallError::Transport(format!("{e:?}")),
                }),
                Err(_) => Err(CallError::Status(
                    middleware::transport::HandlerError::DeadlineExceeded,
                )),
            }
        })
    }

    fn pause(&mut self) {
        std::thread::sleep(self.pause);
    }
}
