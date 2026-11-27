//! Plugin-based graph core, `no_std` with `alloc`.
//!
//! A graph is JSON: nodes with a kind and config, and edges between them.
//! Every kind is a [`plugin::Plugin`] in a [`plugin::Registry`]. The
//! [`graph::GraphSpec::validate`] step checks a graph against the registry
//! and reports every fault at once: unknown kinds, config faults, dangling
//! edges, type mismatches, cycles, and roots that cannot take a Redfish
//! event. [`exec::Runner`] then runs validated graphs.
//!
//! This crate has no I/O. Plugins that need a network, a file or a clock
//! live in the adapter crates, which register them next to the ones here.
//! The same graph JSON therefore validates on every target.

#![cfg_attr(not(test), no_std)]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

use alloc::string::String;
use core::fmt;

pub mod builtin;
pub mod exec;
pub mod graph;
pub mod node;
pub mod plugin;

pub use exec::{Outcome, Runner};
pub use graph::{GraphSpec, Validated};
pub use node::{Node, NodeFuture};
pub use plugin::{ConfigField, FieldType, Plugin, Registry};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The graph, its config, or a payload is wrong.
    Invalid(String),
    /// A backend or I/O step failed.
    Backend(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Invalid(m) => write!(f, "invalid graph: {m}"),
            Error::Backend(m) => write!(f, "backend: {m}"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Error {}
