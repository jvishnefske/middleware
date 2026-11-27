//! Graph description and validation against a plugin [`Registry`]. Validation
//! does no I/O, so a graph can be checked before any backend is reachable.

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use serde::Deserialize;
use serde_json::Value;

use crate::Error;
use crate::plugin::{Plugin, Registry, normalize_config};

/// Payload type tags carried on edges.
pub const REDFISH_EVENT: &str = "redfish.event";
pub const TEXT: &str = "text";
pub const VECTOR: &str = "vector";
pub const HITS: &str = "hits";
pub const NOTICE: &str = "notice";
/// Wildcard: as an input, accepts any upstream type.
pub const ANY: &str = "any";
/// As an output: the node passes its input's type through unchanged.
pub const SAME: &str = "same";

#[derive(Debug, Clone, Deserialize)]
pub struct NodeSpec {
    pub id: String,
    pub kind: String,
    #[serde(default)]
    pub config: Value,
}

/// On-disk graph format: `{"name", "nodes": [...], "edges": [["from","to"], ...]}`.
#[derive(Debug, Clone, Deserialize)]
pub struct GraphSpec {
    pub name: String,
    pub nodes: Vec<NodeSpec>,
    pub edges: Vec<(String, String)>,
}

impl GraphSpec {
    pub fn from_json(s: &str) -> Result<Self, Error> {
        serde_json::from_str(s).map_err(|e| Error::Invalid(format!("graph json: {e}")))
    }

    /// Checks the graph against the registry and returns its execution order.
    /// Every problem found is reported in one error, so a bad graph shows all
    /// its faults at once.
    pub fn validate(&self, registry: &Registry) -> Result<Validated, Error> {
        let mut problems = Vec::new();
        let n = self.nodes.len();

        // Node ids, kinds and configs.
        let mut index = BTreeMap::new();
        let mut plugins: Vec<Option<&dyn Plugin>> = Vec::with_capacity(n);
        let mut configs = vec![Value::Null; n];
        for (i, node) in self.nodes.iter().enumerate() {
            if index.insert(node.id.as_str(), i).is_some() {
                problems.push(format!("duplicate node id `{}`", node.id));
            }
            match registry.get(&node.kind) {
                None => {
                    problems.push(format!("node `{}`: unknown kind `{}`", node.id, node.kind));
                    plugins.push(None);
                }
                Some(p) => {
                    match normalize_config(&p.config_schema(), &node.config) {
                        Ok(cfg) => {
                            problems.extend(p.check(&cfg).into_iter().map(|m| format!("node `{}`: {m}", node.id)));
                            configs[i] = cfg;
                        }
                        Err(ps) => problems.extend(ps.into_iter().map(|m| format!("node `{}`: {m}", node.id))),
                    }
                    plugins.push(Some(p));
                }
            }
        }
        if n == 0 {
            problems.push("graph has no nodes".into());
        }

        // Edges.
        let mut succ = vec![Vec::new(); n];
        let mut indeg = vec![0usize; n];
        let mut edges = Vec::new();
        let mut seen_edges = BTreeSet::new();
        for (from, to) in &self.edges {
            let (Some(&f), Some(&t)) = (index.get(from.as_str()), index.get(to.as_str())) else {
                problems.push(format!("edge {from} -> {to}: unknown node"));
                continue;
            };
            if f == t {
                problems.push(format!("edge {from} -> {to}: self loop"));
                continue;
            }
            if !seen_edges.insert((f, t)) {
                problems.push(format!("edge {from} -> {to}: duplicate"));
                continue;
            }
            succ[f].push(t);
            indeg[t] += 1;
            edges.push((f, t));
        }

        // Kahn's algorithm: a leftover node means a cycle.
        let mut queue: VecDeque<usize> = (0..n).filter(|&i| indeg[i] == 0).collect();
        let roots: Vec<usize> = queue.iter().copied().collect();
        let mut order = Vec::with_capacity(n);
        let mut deg = indeg.clone();
        while let Some(i) = queue.pop_front() {
            order.push(i);
            for &t in &succ[i] {
                deg[t] -= 1;
                if deg[t] == 0 {
                    queue.push_back(t);
                }
            }
        }
        let acyclic = order.len() == n;
        if !acyclic {
            problems.push("graph has a cycle".into());
        }

        if n > 0 && roots.is_empty() {
            problems.push("graph has no root (every node has an incoming edge)".into());
        }
        for &r in &roots {
            if let Some(p) = plugins[r]
                && p.input() != REDFISH_EVENT
                && p.input() != ANY
            {
                problems.push(format!(
                    "root `{}` takes `{}`, but roots receive a `{REDFISH_EVENT}`",
                    self.nodes[r].id,
                    p.input()
                ));
            }
        }

        // Types, in execution order. A node's output type is its plugin's
        // output, or for `same` the type flowing into it: a root's Redfish
        // event, or the type its first upstream produced.
        if acyclic {
            let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
            for &(f, t) in &edges {
                preds[t].push(f);
            }
            let mut resolved: Vec<Option<String>> = vec![None; n];
            for &i in &order {
                let Some(p) = plugins[i] else { continue };
                for &f in &preds[i] {
                    if let Some(out) = &resolved[f]
                        && p.input() != ANY
                        && out != ANY
                        && out != p.input()
                    {
                        problems.push(format!(
                            "edge {} -> {}: type mismatch, `{out}` into `{}`",
                            self.nodes[f].id,
                            self.nodes[i].id,
                            p.input()
                        ));
                    }
                }
                let flowing = if roots.contains(&i) {
                    REDFISH_EVENT.to_string()
                } else {
                    preds[i].first().and_then(|&f| resolved[f].clone()).unwrap_or_else(|| ANY.into())
                };
                resolved[i] = Some(if p.output() == SAME { flowing } else { p.output().to_string() });
            }
        }

        if !problems.is_empty() {
            return Err(Error::Invalid(problems.join("; ")));
        }
        let sinks: Vec<usize> = (0..n).filter(|&i| succ[i].is_empty()).collect();
        Ok(Validated { spec: self.clone(), configs, order, roots, sinks, succ })
    }
}

/// A graph that passed validation. Indices refer to `spec.nodes`.
#[derive(Debug, Clone)]
pub struct Validated {
    pub spec: GraphSpec,
    /// Each node's config with schema defaults filled in, same index as `spec.nodes`.
    pub configs: Vec<Value>,
    pub order: Vec<usize>,
    pub roots: Vec<usize>,
    pub sinks: Vec<usize>,
    pub succ: Vec<Vec<usize>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{Node, NodeFuture};
    use crate::plugin::{ConfigField, FieldType};
    use alloc::boxed::Box;
    use serde_json::json;

    /// A plugin with fixed metadata, so the validator is tested on its own.
    struct Probe {
        kind: &'static str,
        input: &'static str,
        output: &'static str,
    }

    struct Pass;
    impl Node for Pass {
        fn process<'a>(&'a self, input: Value) -> NodeFuture<'a> {
            Box::pin(async move { Ok(vec![input]) })
        }
    }

    impl Plugin for Probe {
        fn kind(&self) -> &str {
            self.kind
        }
        fn input(&self) -> &str {
            self.input
        }
        fn output(&self) -> &str {
            self.output
        }
        fn config_schema(&self) -> Vec<ConfigField> {
            vec![ConfigField::optional("note", FieldType::String)]
        }
        fn build(&self, _id: &str, _config: &Value) -> Result<Box<dyn Node>, Error> {
            Ok(Box::new(Pass))
        }
    }

    fn registry() -> Registry {
        let mut r = Registry::new();
        let probes = [
            Probe { kind: "event", input: REDFISH_EVENT, output: TEXT },
            Probe { kind: "text.step", input: TEXT, output: TEXT },
            Probe { kind: "vector.step", input: VECTOR, output: HITS },
            Probe { kind: "any.sink", input: ANY, output: NOTICE },
            Probe { kind: "passthrough", input: ANY, output: SAME },
        ];
        for p in probes {
            r.register(Box::new(p)).unwrap();
        }
        r
    }

    fn spec(nodes: &[(&str, &str)], edges: &[(&str, &str)]) -> GraphSpec {
        GraphSpec {
            name: "t".into(),
            nodes: nodes
                .iter()
                .map(|(id, k)| NodeSpec { id: (*id).into(), kind: (*k).into(), config: Value::Null })
                .collect(),
            edges: edges.iter().map(|(a, b)| ((*a).into(), (*b).into())).collect(),
        }
    }

    #[test]
    fn accepts_a_valid_chain() {
        let g = spec(
            &[("ev", "event"), ("t", "text.step"), ("out", "any.sink")],
            &[("ev", "t"), ("t", "out")],
        );
        let v = g.validate(&registry()).unwrap();
        assert_eq!(v.roots, vec![0]);
        assert_eq!(v.sinks, vec![2]);
        assert_eq!(v.order, vec![0, 1, 2]);
    }

    #[test]
    fn rejects_type_mismatch_with_reason() {
        // event emits text; vector.step needs a vector.
        let g = spec(&[("ev", "event"), ("s", "vector.step")], &[("ev", "s")]);
        let err = g.validate(&registry()).unwrap_err().to_string();
        assert!(err.contains("edge ev -> s: type mismatch, `text` into `vector`"), "{err}");
    }

    #[test]
    fn rejects_cycle() {
        let g = spec(
            &[("ev", "any.sink"), ("a", "any.sink"), ("b", "any.sink")],
            &[("ev", "a"), ("a", "b"), ("b", "a")],
        );
        let err = g.validate(&registry()).unwrap_err().to_string();
        assert!(err.contains("cycle"), "{err}");
    }

    #[test]
    fn rejects_unknown_kind_and_dangling_edge_together() {
        let g = spec(&[("ev", "event"), ("x", "teleport")], &[("ev", "nope")]);
        let err = g.validate(&registry()).unwrap_err().to_string();
        assert!(err.contains("unknown kind `teleport`"), "{err}");
        assert!(err.contains("unknown node"), "{err}");
    }

    #[test]
    fn root_must_accept_redfish_event() {
        let g = spec(&[("e", "text.step")], &[]);
        let err = g.validate(&registry()).unwrap_err().to_string();
        assert!(err.contains("roots receive"), "{err}");
    }

    #[test]
    fn same_type_flows_through_a_pass_through_node() {
        // event (text) -> passthrough (text) -> text.step: fine.
        let g = spec(
            &[("ev", "event"), ("p", "passthrough"), ("t", "text.step")],
            &[("ev", "p"), ("p", "t")],
        );
        g.validate(&registry()).unwrap();
    }

    #[test]
    fn same_type_carries_the_wrong_type_into_a_mismatch() {
        // event (text) -> passthrough -> vector.step (wants vector): mismatch.
        let g = spec(
            &[("ev", "event"), ("p", "passthrough"), ("v", "vector.step")],
            &[("ev", "p"), ("p", "v")],
        );
        let err = g.validate(&registry()).unwrap_err().to_string();
        assert!(err.contains("edge p -> v: type mismatch, `text` into `vector`"), "{err}");
    }

    #[test]
    fn every_fault_is_reported_in_one_error() {
        let g = spec(
            &[("ev", "event"), ("x", "teleport"), ("t", "text.step")],
            &[("ev", "t"), ("t", "nowhere")],
        );
        let err = g.validate(&registry()).unwrap_err().to_string();
        assert!(err.contains("unknown kind `teleport`"), "{err}");
        assert!(err.contains("edge t -> nowhere: unknown node"), "{err}");
    }

    #[test]
    fn config_defaults_are_recorded_and_unknown_keys_rejected() {
        let mut g = spec(&[("ev", "event")], &[]);
        g.nodes[0].config = json!({"note": "hi"});
        assert_eq!(g.validate(&registry()).unwrap().configs[0], json!({"note": "hi"}));
        g.nodes[0].config = json!({"bogus": 1});
        let err = g.validate(&registry()).unwrap_err().to_string();
        assert!(err.contains("node `ev`: unknown config key `bogus`"), "{err}");
    }
}
