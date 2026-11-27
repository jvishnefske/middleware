//! Runs graphs on a microcontroller. Only the pure plugins from `graph-core`
//! are registered, so a graph that names a network plugin fails validation
//! with "unknown kind" before anything runs.
//!
//! A node must finish on its first poll. There is no executor: each future
//! is polled once with a no-op waker, and a node that is still pending is an
//! error. Every pure node satisfies this.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::vec::Vec;

use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, Waker};

use graph_core::{Error, GraphSpec, Outcome, Registry, Runner};
use serde_json::Value;

/// The plugins available on this target.
pub fn registry() -> Registry {
    graph_core::builtin::core_registry()
}

/// Validates the graph JSON, runs it once on one event, and returns what
/// reached the sinks.
pub fn run_once(graph_json: &str, event_json: &str) -> Result<Vec<Outcome>, Error> {
    let spec = GraphSpec::from_json(graph_json)?;
    let event: Value =
        serde_json::from_str(event_json).map_err(|e| Error::Invalid(format!("event json: {e}")))?;
    let runner = Runner::new(&spec, &registry())?;
    match poll_ready(runner.run(event)) {
        Some(result) => result,
        None => Err(Error::Backend("a node is not ready: micro nodes must finish on their first poll".into())),
    }
}

/// Polls a future once. `Some` if it finished, `None` if it is still pending.
pub fn poll_ready<F: Future>(fut: F) -> Option<F::Output> {
    let mut fut = pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(v) => Some(v),
        Poll::Pending => None,
    }
}
