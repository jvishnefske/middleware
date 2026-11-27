//! The node interface. A node takes one JSON payload and returns zero or more
//! payloads for its successors.
//!
//! `process` returns a boxed future, so a node may await I/O (std adapters)
//! or be ready at once (pure nodes). A micro target polls each future once
//! with a no-op waker and rejects any node that is not ready.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use serde_json::Value;

use crate::Error;

/// The future a node returns. `Send` so that adapters on a multi-threaded
/// runtime can spawn graph runs; every built-in node satisfies it.
pub type NodeFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<Value>, Error>> + Send + 'a>>;

pub trait Node: Send + Sync {
    fn process<'a>(&'a self, input: Value) -> NodeFuture<'a>;
}
