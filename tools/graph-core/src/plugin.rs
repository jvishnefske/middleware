//! Plugins: everything a graph node can be. A plugin declares its kind, the
//! payload types it takes and returns, and the config keys it accepts. The
//! validator reads that metadata; nothing about a kind is hard-coded here.
//! Adapter crates register their plugins into a [`Registry`].

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::Error;
use crate::node::Node;

/// Type of one config value in a plugin's schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    String,
    /// Non-negative integer.
    Uint,
    Number,
    Bool,
    Array,
    Object,
    /// Any JSON value.
    Any,
}

impl FieldType {
    pub fn name(self) -> &'static str {
        match self {
            FieldType::String => "a string",
            FieldType::Uint => "a non-negative integer",
            FieldType::Number => "a number",
            FieldType::Bool => "a boolean",
            FieldType::Array => "an array",
            FieldType::Object => "an object",
            FieldType::Any => "any value",
        }
    }

    pub fn accepts(self, v: &Value) -> bool {
        match self {
            FieldType::String => v.is_string(),
            FieldType::Uint => v.is_u64(),
            FieldType::Number => v.is_number(),
            FieldType::Bool => v.is_boolean(),
            FieldType::Array => v.is_array(),
            FieldType::Object => v.is_object(),
            FieldType::Any => true,
        }
    }
}

/// One key of a plugin's config. A `null` value counts as absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigField {
    pub key: String,
    #[serde(rename = "type")]
    pub ty: FieldType,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
}

impl ConfigField {
    pub fn required(key: &str, ty: FieldType) -> Self {
        ConfigField { key: key.into(), ty, required: true, default: None }
    }

    /// Optional with no default: absent from the normalized config.
    pub fn optional(key: &str, ty: FieldType) -> Self {
        ConfigField { key: key.into(), ty, required: false, default: None }
    }

    pub fn with_default(key: &str, ty: FieldType, default: Value) -> Self {
        ConfigField { key: key.into(), ty, required: false, default: Some(default) }
    }
}

pub trait Plugin: Send + Sync {
    /// Node kind as written in graph JSON, e.g. `log.read`.
    fn kind(&self) -> &str;
    /// Payload type accepted on input. `any` accepts everything.
    fn input(&self) -> &str;
    /// Payload type produced. `same` passes the input's type through.
    fn output(&self) -> &str;
    fn config_schema(&self) -> Vec<ConfigField>;
    /// Checks beyond the schema, on the normalized config. Returns every problem.
    fn check(&self, _config: &Value) -> Vec<String> {
        Vec::new()
    }
    /// Builds a node. `config` has passed the schema and has defaults applied.
    /// Config is only read here, never a secret value: plugins name env vars.
    fn build(&self, id: &str, config: &Value) -> Result<Box<dyn Node>, Error>;
}

fn json_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Checks a raw config against a schema and fills in defaults. Every fault is
/// returned together: missing required keys, wrong types, unknown keys.
pub fn normalize_config(schema: &[ConfigField], raw: &Value) -> Result<Value, Vec<String>> {
    let empty = Map::new();
    let map = match raw {
        Value::Null => &empty,
        Value::Object(m) => m,
        other => return Err(alloc::vec![format!("config must be an object, got {}", json_kind(other))]),
    };
    let mut problems = Vec::new();
    let mut out = Map::new();
    for f in schema {
        match map.get(&f.key).filter(|v| !v.is_null()) {
            Some(v) if f.ty.accepts(v) => {
                out.insert(f.key.clone(), v.clone());
            }
            Some(v) => problems.push(format!("`{}` must be {}, got {}", f.key, f.ty.name(), json_kind(v))),
            None if f.required => problems.push(format!("missing required `{}`", f.key)),
            None => {
                if let Some(d) = &f.default {
                    out.insert(f.key.clone(), d.clone());
                }
            }
        }
    }
    for k in map.keys() {
        if !schema.iter().any(|f| &f.key == k) {
            problems.push(format!("unknown config key `{k}`"));
        }
    }
    if problems.is_empty() { Ok(Value::Object(out)) } else { Err(problems) }
}

/// Plugins by kind. Kinds are unique: registering a second one is an error.
#[derive(Default)]
pub struct Registry {
    plugins: BTreeMap<String, Box<dyn Plugin>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, plugin: Box<dyn Plugin>) -> Result<(), Error> {
        let kind = plugin.kind().to_string();
        if self.plugins.contains_key(&kind) {
            return Err(Error::Invalid(format!("plugin kind `{kind}` is already registered")));
        }
        self.plugins.insert(kind, plugin);
        Ok(())
    }

    pub fn get(&self, kind: &str) -> Option<&dyn Plugin> {
        self.plugins.get(kind).map(|p| p.as_ref())
    }

    /// All plugins, sorted by kind.
    pub fn list(&self) -> impl Iterator<Item = &dyn Plugin> {
        self.plugins.values().map(|p| p.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Vec<ConfigField> {
        alloc::vec![
            ConfigField::required("url", FieldType::String),
            ConfigField::with_default("max", FieldType::Uint, json!(5)),
            ConfigField::optional("note", FieldType::String),
        ]
    }

    #[test]
    fn fills_defaults_and_skips_absent_optionals() {
        let cfg = normalize_config(&schema(), &json!({"url": "x"})).unwrap();
        assert_eq!(cfg, json!({"url": "x", "max": 5}));
    }

    #[test]
    fn reports_every_config_fault_at_once() {
        let problems = normalize_config(&schema(), &json!({"max": "many", "bogus": 1})).unwrap_err();
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("missing required `url`")));
        assert!(problems.iter().any(|p| p.contains("`max` must be a non-negative integer")));
        assert!(problems.iter().any(|p| p.contains("unknown config key `bogus`")));
    }

    #[test]
    fn null_counts_as_absent() {
        let cfg = normalize_config(&schema(), &json!({"url": "x", "note": null})).unwrap();
        assert!(cfg.get("note").is_none());
    }
}
