//! Runs a validated graph. A Redfish event enters at every root and flows
//! depth-first to the sinks. Nothing here needs a runtime: a node is a future
//! and the caller decides how to poll it.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;

use core::future::Future;
use core::pin::Pin;

use serde_json::Value;

use crate::Error;
use crate::graph::{GraphSpec, Validated};
use crate::node::Node;
use crate::plugin::Registry;

/// A payload that reached a sink, tagged with the sink's node id.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub node: String,
    pub payload: Value,
}

pub struct Runner {
    graph: Validated,
    nodes: Vec<Box<dyn Node>>,
}

impl Runner {
    /// Validates first, then builds every node through its plugin. Nothing
    /// runs if either fails.
    pub fn new(spec: &GraphSpec, registry: &Registry) -> Result<Self, Error> {
        let graph = spec.validate(registry)?;
        let nodes = graph
            .spec
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let plugin = registry
                    .get(&n.kind)
                    .ok_or_else(|| Error::Invalid(format!("unknown kind `{}`", n.kind)))?;
                plugin.build(&n.id, &graph.configs[i])
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Runner { graph, nodes })
    }

    pub fn graph(&self) -> &Validated {
        &self.graph
    }

    /// Feeds one event to every root and collects what reaches the sinks.
    pub fn run<'a>(&'a self, event: Value) -> Pin<Box<dyn Future<Output = Result<Vec<Outcome>, Error>> + Send + 'a>> {
        Box::pin(async move {
            let mut out = Vec::new();
            for &root in &self.graph.roots {
                out.extend(self.step(root, event.clone()).await?);
            }
            Ok(out)
        })
    }

    fn step<'a>(&'a self, idx: usize, input: Value) -> Pin<Box<dyn Future<Output = Result<Vec<Outcome>, Error>> + Send + 'a>> {
        Box::pin(async move {
            let payloads = self.nodes[idx].process(input).await?;
            let succ = &self.graph.succ[idx];
            if succ.is_empty() {
                let node = self.graph.spec.nodes[idx].id.clone();
                return Ok(payloads
                    .into_iter()
                    .map(|payload| Outcome { node: node.clone(), payload })
                    .collect());
            }
            let mut all = Vec::new();
            for p in payloads {
                for &t in succ {
                    all.extend(self.step(t, p.clone()).await?);
                }
            }
            Ok(all)
        })
    }
}
