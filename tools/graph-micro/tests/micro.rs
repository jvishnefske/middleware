//! The micro adapter on the host: the same graph JSON as the std adapter, run
//! with pure plugins only.

use graph_micro::run_once;
use serde_json::json;

const FILTERED_QUERY: &str = r#"{
  "name": "critical-query",
  "nodes": [
    {"id": "event", "kind": "redfish.event"},
    {"id": "critical", "kind": "filter", "config": {"field": "Severity", "equals": "Critical"}},
    {"id": "question", "kind": "template", "config": {"template": "{{Message}} troubleshooting"}}
  ],
  "edges": [["event", "critical"], ["critical", "question"]]
}"#;

fn delivery(severity: &str) -> String {
    json!({"Events": [{
        "MessageId": "Base.1.0.ResourceCreated",
        "Message": "Fan 1 stalled",
        "Severity": severity,
    }]})
    .to_string()
}

#[test]
fn critical_delivery_renders_a_query_on_the_micro_target() {
    let out = run_once(FILTERED_QUERY, &delivery("Critical")).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].node, "question");
    assert_eq!(out[0].payload["text"], "Fan 1 stalled troubleshooting");
}

#[test]
fn filtered_delivery_produces_no_outcome() {
    let out = run_once(FILTERED_QUERY, &delivery("Warning")).unwrap();
    assert!(out.is_empty());
}

#[test]
fn network_kinds_are_rejected_at_validation() {
    let g = r#"{"name":"x","nodes":[{"id":"e","kind":"redfish.event"},{"id":"s","kind":"web.search"}],
                "edges":[["e","s"]]}"#;
    let err = run_once(g, &delivery("Critical")).unwrap_err().to_string();
    assert!(err.contains("unknown kind `web.search`"), "{err}");
}

#[test]
fn bad_event_json_is_reported() {
    let err = run_once(FILTERED_QUERY, "{not json").unwrap_err().to_string();
    assert!(err.contains("event json"), "{err}");
}
