//! Pure plugins that need no backend, so every target has them:
//!
//! - `redfish.event`: a root source. Parses an EventService delivery (DMTF
//!   format) into the graph event shape `{"event", "origin", "log_lines",
//!   "Message", "Severity"}`. Input already in graph shape passes through.
//! - `log.read`: forwards the last lines of `log_lines`.
//! - `filter`: passes its input on when a field equals a value or contains a
//!   string. Its output type is its input type (`same`).
//! - `template`: renders `{{path}}` placeholders from its input into text.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use serde_json::{Value, json};

use crate::Error;
use crate::graph::{ANY, REDFISH_EVENT, SAME, TEXT};
use crate::node::{Node, NodeFuture};
use crate::plugin::{ConfigField, FieldType, Plugin, Registry};

/// Every plugin in this crate, ready to register.
pub fn core_plugins() -> Vec<Box<dyn Plugin>> {
    vec![Box::new(EventSource), Box::new(LogRead), Box::new(Filter), Box::new(Template)]
}

/// A registry holding only [`core_plugins`].
pub fn core_registry() -> Registry {
    let mut r = Registry::new();
    for p in core_plugins() {
        r.register(p).expect("core plugin kinds are unique");
    }
    r
}

/// Follows a dotted path (`OriginOfCondition.@odata.id`) through JSON objects.
pub fn lookup<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(v, |cur, key| cur.get(key))
}

/// Accepts the standard envelope `{"Events": [...]}` or a single event object.
/// Each event contributes its `Message` as one log line. `Message` and
/// `Severity` of the first event are also copied to the top level, so a
/// `template` or `filter` node can read them.
pub fn to_graph_event(v: &Value) -> Result<Value, Error> {
    let events: Vec<&Value> = match v.get("Events").and_then(Value::as_array) {
        Some(list) => list.iter().collect(),
        None => vec![v],
    };
    let mut lines = Vec::new();
    let mut ids = Vec::new();
    let mut origin = None;
    let mut first: Option<&Value> = None;
    for e in events {
        let msg = e
            .get("Message")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Invalid("Redfish event has no `Message` string".into()))?;
        lines.push(msg.to_string());
        if first.is_none() {
            first = Some(e);
        }
        if let Some(id) = e.get("MessageId").and_then(Value::as_str) {
            ids.push(id.to_string());
        }
        if origin.is_none() {
            origin = e
                .get("OriginOfCondition")
                .and_then(|o| o.get("@odata.id"))
                .and_then(Value::as_str)
                .map(String::from);
        }
    }
    let first_field = |k: &str| first.and_then(|e| e.get(k)).cloned().unwrap_or(Value::Null);
    Ok(json!({
        "event": ids.join(","),
        "origin": origin,
        "log_lines": lines,
        "Message": first_field("Message"),
        "Severity": first_field("Severity"),
    }))
}

// ---- redfish.event --------------------------------------------------------

pub struct EventSource;

impl Plugin for EventSource {
    fn kind(&self) -> &str {
        "redfish.event"
    }
    fn input(&self) -> &str {
        REDFISH_EVENT
    }
    fn output(&self) -> &str {
        REDFISH_EVENT
    }
    fn config_schema(&self) -> Vec<ConfigField> {
        Vec::new()
    }
    fn build(&self, _id: &str, _config: &Value) -> Result<Box<dyn Node>, Error> {
        Ok(Box::new(EventNode))
    }
}

struct EventNode;

impl Node for EventNode {
    fn process<'a>(&'a self, input: Value) -> NodeFuture<'a> {
        Box::pin(async move {
            if input.get("log_lines").is_some() {
                return Ok(vec![input]);
            }
            Ok(vec![to_graph_event(&input)?])
        })
    }
}

// ---- log.read -------------------------------------------------------------

/// Forwards the last `max_lines` of `event.log_lines`.
pub struct LogRead;

impl Plugin for LogRead {
    fn kind(&self) -> &str {
        "log.read"
    }
    fn input(&self) -> &str {
        REDFISH_EVENT
    }
    fn output(&self) -> &str {
        TEXT
    }
    fn config_schema(&self) -> Vec<ConfigField> {
        vec![ConfigField::with_default("max_lines", FieldType::Uint, json!(200))]
    }
    fn build(&self, id: &str, config: &Value) -> Result<Box<dyn Node>, Error> {
        let max_lines = config.get("max_lines").and_then(Value::as_u64).unwrap_or(200) as usize;
        Ok(Box::new(LogNode { id: id.into(), max_lines }))
    }
}

struct LogNode {
    id: String,
    max_lines: usize,
}

impl Node for LogNode {
    fn process<'a>(&'a self, input: Value) -> NodeFuture<'a> {
        Box::pin(async move {
            let id = self.id.as_str();
            let lines = input
                .get("log_lines")
                .and_then(Value::as_array)
                .ok_or_else(|| Error::Invalid(format!("node `{id}`: no `log_lines` array")))?;
            let skip = lines.len().saturating_sub(self.max_lines);
            let text = lines[skip..].iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n");
            Ok(vec![json!({"source": "log", "text": text})])
        })
    }
}

// ---- filter ---------------------------------------------------------------

pub struct Filter;

impl Plugin for Filter {
    fn kind(&self) -> &str {
        "filter"
    }
    fn input(&self) -> &str {
        ANY
    }
    fn output(&self) -> &str {
        SAME
    }
    fn config_schema(&self) -> Vec<ConfigField> {
        vec![
            ConfigField::required("field", FieldType::String),
            ConfigField::optional("equals", FieldType::Any),
            ConfigField::optional("contains", FieldType::String),
        ]
    }
    fn check(&self, config: &Value) -> Vec<String> {
        match (config.get("equals"), config.get("contains")) {
            (Some(_), Some(_)) => vec!["use `equals` or `contains`, not both".into()],
            (None, None) => vec!["needs `equals` or `contains`".into()],
            _ => Vec::new(),
        }
    }
    fn build(&self, id: &str, config: &Value) -> Result<Box<dyn Node>, Error> {
        let field = config.get("field").and_then(Value::as_str).unwrap_or_default().to_string();
        Ok(Box::new(FilterNode {
            id: id.into(),
            field,
            equals: config.get("equals").cloned(),
            contains: config.get("contains").and_then(Value::as_str).map(String::from),
        }))
    }
}

struct FilterNode {
    id: String,
    field: String,
    equals: Option<Value>,
    contains: Option<String>,
}

impl Node for FilterNode {
    fn process<'a>(&'a self, input: Value) -> NodeFuture<'a> {
        Box::pin(async move {
            let value = lookup(&input, &self.field);
            let keep = match (&self.equals, &self.contains) {
                (Some(eq), _) => value == Some(eq),
                (None, Some(needle)) => value.and_then(Value::as_str).is_some_and(|s| s.contains(needle.as_str())),
                (None, None) => {
                    return Err(Error::Invalid(format!("node `{}`: filter has no condition", self.id)));
                }
            };
            Ok(if keep { vec![input] } else { Vec::new() })
        })
    }
}

// ---- template -------------------------------------------------------------

pub struct Template;

impl Plugin for Template {
    fn kind(&self) -> &str {
        "template"
    }
    fn input(&self) -> &str {
        ANY
    }
    fn output(&self) -> &str {
        TEXT
    }
    fn config_schema(&self) -> Vec<ConfigField> {
        vec![ConfigField::required("template", FieldType::String)]
    }
    fn check(&self, config: &Value) -> Vec<String> {
        let t = config.get("template").and_then(Value::as_str).unwrap_or_default();
        if unclosed_placeholder(t) {
            vec!["template has a `{{` with no `}}`".into()]
        } else {
            Vec::new()
        }
    }
    fn build(&self, id: &str, config: &Value) -> Result<Box<dyn Node>, Error> {
        let template = config.get("template").and_then(Value::as_str).unwrap_or_default().to_string();
        Ok(Box::new(TemplateNode { id: id.into(), template }))
    }
}

struct TemplateNode {
    id: String,
    template: String,
}

impl Node for TemplateNode {
    fn process<'a>(&'a self, input: Value) -> NodeFuture<'a> {
        Box::pin(async move {
            let text = render(&self.template, &input, &self.id)?;
            Ok(vec![json!({"source": "template", "text": text})])
        })
    }
}

fn unclosed_placeholder(template: &str) -> bool {
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => rest = &after[end + 2..],
            None => return true,
        }
    }
    false
}

/// Replaces each `{{path}}` with the value at `path` in `input`. Strings go in
/// as they are; other values as their JSON text. A missing or null field is an
/// error, so a query never goes out with a hole in it.
pub fn render(template: &str, input: &Value, node: &str) -> Result<String, Error> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find("}}")
            .ok_or_else(|| Error::Invalid(format!("node `{node}`: template has a `{{{{` with no `}}}}`")))?;
        let path = after[..end].trim();
        let value = lookup(input, path)
            .filter(|v| !v.is_null())
            .ok_or_else(|| Error::Invalid(format!("node `{node}`: payload has no field `{path}`")))?;
        match value {
            Value::String(s) => out.push_str(s),
            other => out.push_str(&other.to_string()),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_nested_fields_and_keeps_literal_text() {
        let input = json!({"Message": "Fan 1 stalled", "Sensor": {"Index": 1}});
        let s = render("{{Message}} troubleshooting ({{Sensor.Index}})", &input, "t").unwrap();
        assert_eq!(s, "Fan 1 stalled troubleshooting (1)");
    }

    #[test]
    fn missing_field_is_an_error() {
        let err = render("{{Nope}}", &json!({}), "t").unwrap_err().to_string();
        assert!(err.contains("no field `Nope`"), "{err}");
    }

    #[test]
    fn unclosed_placeholder_is_a_config_fault() {
        assert!(unclosed_placeholder("abc {{x"));
        assert!(!unclosed_placeholder("abc {{x}} {{y}}"));
    }

    #[test]
    fn maps_standard_envelope() {
        let ev = json!({"Events": [{
            "MessageId": "Base.1.0.ResourceCreated",
            "Message": "Fan 1 stalled",
            "Severity": "Critical",
            "OriginOfCondition": {"@odata.id": "/redfish/v1/Chassis/1/Thermal"}
        }]});
        let g = to_graph_event(&ev).unwrap();
        assert_eq!(g["log_lines"][0], "Fan 1 stalled");
        assert_eq!(g["origin"], "/redfish/v1/Chassis/1/Thermal");
        assert_eq!(g["Message"], "Fan 1 stalled");
        assert_eq!(g["Severity"], "Critical");
    }

    #[test]
    fn rejects_event_without_message() {
        assert!(to_graph_event(&json!({"Events": [{"MessageId": "x"}]})).is_err());
    }

    #[test]
    fn lookup_follows_dotted_paths() {
        let v = json!({"a": {"b": {"c": 7}}});
        assert_eq!(lookup(&v, "a.b.c"), Some(&json!(7)));
        assert_eq!(lookup(&v, "a.x"), None);
    }
}
