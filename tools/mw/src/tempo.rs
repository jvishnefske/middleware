//! Tempo's HTTP API (pure): URLs, response parsing and the span tree.
//!
//! * [`Query::parse`] tells a trace id (32 hex digits) from a request id.
//! * [`trace_url`] is `GET /api/traces/<id>`; [`search_url`] is
//!   `GET /api/search?tags=<attr>="<value>"&start=…&end=…&limit=20`, the
//!   tag query in logfmt, percent-encoded.
//! * [`parse_search`] reads the search response's `traces[].traceID`.
//! * [`parse_trace`] reads a trace in OTLP JSON as Tempo returns it:
//!   `batches` (Tempo 1.x/2.x `/api/traces`) or `resourceSpans`, optionally
//!   wrapped in `trace` (`/api/v2/traces`), with `scopeSpans` or the older
//!   `instrumentationLibrarySpans`. Times may be strings or numbers.
//! * [`format_tree`] prints spans as an indented tree: children under
//!   their parent in start order, spans whose parent is missing as roots.
//!   The walk is iterative and visits each span once, so a malformed
//!   (cyclic) trace cannot loop.

use serde_json::Value;
use std::fmt::Write as _;

/// What `mw trace` was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    /// A W3C trace id, lowercase hex.
    TraceId(String),
    /// An `x-request-id`.
    RequestId(String),
}

impl Query {
    /// 32 hex digits (not all zero) is a trace id; otherwise 1–64 visible
    /// ASCII characters without `"` or `\` is a request id.
    pub fn parse(s: &str) -> Option<Query> {
        let hex = s.len() == 32 && s.bytes().all(|c| c.is_ascii_hexdigit());
        if hex && s.bytes().any(|c| c != b'0') {
            return Some(Query::TraceId(s.to_ascii_lowercase()));
        }
        let ok = (1..=64).contains(&s.len())
            && s.bytes()
                .all(|c| c.is_ascii_graphic() && c != b'"' && c != b'\\');
        ok.then(|| Query::RequestId(s.to_owned()))
    }
}

/// Percent-encodes everything but RFC 3986 unreserved characters.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// `<base>/api/traces/<id>`.
pub fn trace_url(base: &str, id: &str) -> String {
    format!("{base}/api/traces/{id}")
}

/// `<base>/api/search` for spans whose `attr` equals `value`, between
/// `start` and `end` (Unix seconds), at most 20 traces.
pub fn search_url(base: &str, attr: &str, value: &str, start: u64, end: u64) -> String {
    let tags = format!("{attr}=\"{value}\"");
    format!(
        "{base}/api/search?tags={}&start={start}&end={end}&limit=20",
        percent_encode(&tags)
    )
}

/// A response Tempo should not have sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TempoError(pub String);

impl std::fmt::Display for TempoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unexpected Tempo response: {}", self.0)
    }
}

impl std::error::Error for TempoError {}

fn json(text: &str) -> Result<Value, TempoError> {
    serde_json::from_str(text).map_err(|e| TempoError(e.to_string()))
}

/// The trace ids of a search response, in response order. A response
/// without traces gives an empty list.
pub fn parse_search(text: &str) -> Result<Vec<String>, TempoError> {
    let v = json(text)?;
    let traces = match v.get("traces") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(t)) => t,
        Some(_) => return Err(TempoError("'traces' is not an array".into())),
    };
    traces
        .iter()
        .map(|t| {
            t.get("traceID")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && id.bytes().all(|c| c.is_ascii_hexdigit()))
                .map(str::to_owned)
                .ok_or_else(|| TempoError("a search result without a hex traceID".into()))
        })
        .collect()
}

/// One span, as far as the tree needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// Span id, as encoded in the response.
    pub id: String,
    /// Parent span id; `None` for a root.
    pub parent: Option<String>,
    /// `service.name` of its resource.
    pub service: String,
    /// Span name.
    pub name: String,
    /// Kind without the `SPAN_KIND_` prefix, lowercase (`server`).
    pub kind: String,
    /// Start, Unix nanoseconds.
    pub start_ns: u64,
    /// End, Unix nanoseconds.
    pub end_ns: u64,
    /// The status is an error.
    pub error: bool,
    /// HTTP or gRPC status code, when an attribute carries one.
    pub code: Option<String>,
}

fn num(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        _ => 0,
    }
}

fn any_value(v: &Value) -> Option<String> {
    let v = v.get("value")?;
    if let Some(s) = v.get("stringValue").and_then(Value::as_str) {
        return Some(s.to_owned());
    }
    match v.get("intValue")? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn attr(attrs: Option<&Value>, keys: &[&str]) -> Option<String> {
    let attrs = attrs?.as_array()?;
    keys.iter().find_map(|k| {
        attrs
            .iter()
            .find(|a| a.get("key").and_then(Value::as_str) == Some(k))
            .and_then(any_value)
    })
}

fn array<'a>(v: &'a Value, keys: &[&str]) -> &'a [Value] {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_array))
        .map_or(&[], Vec::as_slice)
}

/// The spans of a trace response (module docs for the accepted shapes).
pub fn parse_trace(text: &str) -> Result<Vec<Span>, TempoError> {
    let v = json(text)?;
    let root = v.get("trace").unwrap_or(&v);
    let resources = array(root, &["batches", "resourceSpans"]);
    if resources.is_empty() && root.get("batches").is_none() && root.get("resourceSpans").is_none()
    {
        return Err(TempoError("neither 'batches' nor 'resourceSpans'".into()));
    }
    let mut spans = Vec::new();
    for r in resources {
        let service = attr(
            r.get("resource").and_then(|r| r.get("attributes")),
            &["service.name"],
        )
        .unwrap_or_else(|| "?".to_owned());
        for scope in array(r, &["scopeSpans", "instrumentationLibrarySpans"]) {
            for s in array(scope, &["spans"]) {
                let id = s
                    .get("spanId")
                    .and_then(Value::as_str)
                    .filter(|i| !i.is_empty())
                    .ok_or_else(|| TempoError("a span without spanId".into()))?;
                let kind = s
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("SPAN_KIND_UNSPECIFIED");
                let status = s.get("status").and_then(|st| st.get("code"));
                let error = matches!(status, Some(Value::String(c)) if c == "STATUS_CODE_ERROR")
                    || matches!(status, Some(Value::Number(n)) if n.as_u64() == Some(2));
                spans.push(Span {
                    id: id.to_owned(),
                    parent: s
                        .get("parentSpanId")
                        .and_then(Value::as_str)
                        .filter(|p| !p.is_empty())
                        .map(str::to_owned),
                    service: service.clone(),
                    name: s
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    kind: kind
                        .strip_prefix("SPAN_KIND_")
                        .unwrap_or(kind)
                        .to_ascii_lowercase(),
                    start_ns: num(s.get("startTimeUnixNano")),
                    end_ns: num(s.get("endTimeUnixNano")),
                    error,
                    code: attr(
                        s.get("attributes"),
                        &[
                            "http.status_code",
                            "http.response.status_code",
                            "rpc.grpc.status_code",
                            "grpc.status_code",
                        ],
                    ),
                });
            }
        }
    }
    Ok(spans)
}

/// `ns` as milliseconds with three decimals, integer arithmetic.
fn ms(ns: u64) -> String {
    format!("{}.{:03} ms", ns / 1_000_000, (ns / 1_000) % 1_000)
}

/// Prints `spans` of trace `trace_id` as a tree (module docs).
///
/// ```text
/// trace <id>: <n> spans, <duration>
/// <offset>  <service> <name> [<kind>] <duration>[ code=<c>][ ERROR]
/// ```
///
/// Each level indents two spaces; `+<offset>` is the start relative to
/// the earliest span.
pub fn format_tree(trace_id: &str, spans: &[Span]) -> String {
    let t0 = spans.iter().map(|s| s.start_ns).min().unwrap_or(0);
    let t1 = spans.iter().map(|s| s.end_ns).max().unwrap_or(0);
    let mut out = format!(
        "trace {trace_id}: {} span{}, {}\n",
        spans.len(),
        if spans.len() == 1 { "" } else { "s" },
        ms(t1.saturating_sub(t0))
    );
    let mut order: Vec<usize> = (0..spans.len()).collect();
    order.sort_by_key(|&i| (spans[i].start_ns, spans[i].id.clone()));
    let has = |id: &str| spans.iter().any(|s| s.id == id);
    let roots: Vec<usize> = order
        .iter()
        .copied()
        .filter(|&i| spans[i].parent.as_deref().is_none_or(|p| !has(p)))
        .collect();
    let mut seen = vec![false; spans.len()];
    // Depth-first, explicit stack of (index, depth); pushing children in
    // reverse start order pops them in start order.
    let mut stack: Vec<(usize, usize)> = roots.iter().rev().map(|&i| (i, 0)).collect();
    // Spans left unvisited (a parent cycle) are printed as roots afterwards.
    let mut pending: Vec<usize> = order.clone();
    pending.reverse();
    loop {
        let (i, depth) = match stack.pop() {
            Some(top) => top,
            None => match pending.pop() {
                Some(i) if !seen[i] => (i, 0),
                Some(_) => continue,
                None => break,
            },
        };
        if seen[i] {
            continue;
        }
        seen[i] = true;
        let s = &spans[i];
        let _ = write!(
            out,
            "{:>12}  {}{} {} [{}] {}",
            format!("+{}", ms(s.start_ns.saturating_sub(t0))),
            "  ".repeat(depth),
            s.service,
            s.name,
            s.kind,
            ms(s.end_ns.saturating_sub(s.start_ns))
        );
        if let Some(c) = &s.code {
            let _ = write!(out, " code={c}");
        }
        if s.error {
            out.push_str(" ERROR");
        }
        out.push('\n');
        for &c in order.iter().rev() {
            if !seen[c] && spans[c].parent.as_deref() == Some(s.id.as_str()) {
                stack.push((c, depth + 1));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRACE: &str = include_str!("../tests/fixtures/tempo-trace.json");
    const SEARCH: &str = include_str!("../tests/fixtures/tempo-search.json");

    #[test]
    fn m51_queries_are_told_apart() {
        assert_eq!(
            Query::parse("0AF7651916CD43DD8448EB211C80319C"),
            Some(Query::TraceId("0af7651916cd43dd8448eb211c80319c".into()))
        );
        assert_eq!(
            Query::parse("00000000000000000000000000000000"),
            Some(Query::RequestId("00000000000000000000000000000000".into()))
        );
        assert_eq!(
            Query::parse("6e1b4bd2-35b4-4f3c-9f6e-0c1d2e3f4a5b"),
            Some(Query::RequestId(
                "6e1b4bd2-35b4-4f3c-9f6e-0c1d2e3f4a5b".into()
            ))
        );
        assert_eq!(Query::parse(""), None);
        assert_eq!(Query::parse("a\"b"), None);
        assert_eq!(Query::parse(&"x".repeat(65)), None);
    }

    #[test]
    fn m51_urls() {
        assert_eq!(trace_url("http://t", "ab"), "http://t/api/traces/ab");
        assert_eq!(percent_encode("a b/c:d=\"e\"~"), "a%20b%2Fc%3Ad%3D%22e%22~");
        assert_eq!(
            search_url("http://t", "guid:x-request-id", "r-1", 5, 9),
            "http://t/api/search?tags=guid%3Ax-request-id%3D%22r-1%22&start=5&end=9&limit=20"
        );
    }

    #[test]
    fn m51_search_response_gives_trace_ids() {
        assert_eq!(
            parse_search(SEARCH).unwrap(),
            ["4bf92f3577b34da6a3ce929d0e0e4736"]
        );
        assert_eq!(
            parse_search(r#"{"traces":[]}"#).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            parse_search(r#"{"metrics":{}}"#).unwrap(),
            Vec::<String>::new()
        );
        assert!(parse_search(r#"{"traces":[{"traceID":"zz"}]}"#).is_err());
        assert!(parse_search("not json").is_err());
    }

    #[test]
    fn m51_fixture_trace_renders_as_a_tree() {
        let spans = parse_trace(TRACE).unwrap();
        assert_eq!(spans.len(), 5);
        let tree = format_tree("4bf92f3577b34da6a3ce929d0e0e4736", &spans);
        assert_eq!(
            tree,
            "\
trace 4bf92f3577b34da6a3ce929d0e0e4736: 5 spans, 12.500 ms
   +0.000 ms  demo-caller.apps demo-caller [server] 12.500 ms code=200
   +0.400 ms    demo-caller mw.request echo/ping [internal] 11.000 ms
   +0.600 ms      demo-caller mw.request echo/ping [client] 10.200 ms
   +1.100 ms        demo-echo.apps demo-echo.apps.svc.cluster.local:7000/* [server] 9.000 ms code=0
   +1.500 ms          demo-echo mw.handle echo/ping [server] 7.250 ms ERROR
"
        );
    }

    #[test]
    fn m51_other_tempo_shapes_parse() {
        let v2 = r#"{"trace":{"resourceSpans":[{"resource":{"attributes":[]},
            "instrumentationLibrarySpans":[{"spans":[
              {"spanId":"b","parentSpanId":"a","name":"child","kind":"SPAN_KIND_CLIENT",
               "startTimeUnixNano":2000000,"endTimeUnixNano":3000000,"status":{"code":2}},
              {"spanId":"a","name":"root","startTimeUnixNano":"1000000","endTimeUnixNano":"4000000"}
            ]}]}]}}"#;
        let spans = parse_trace(v2).unwrap();
        assert_eq!(spans[0].service, "?");
        assert!(spans[0].error);
        assert_eq!(spans[0].kind, "client");
        assert_eq!(spans[1].kind, "unspecified");
        let tree = format_tree("t", &spans);
        assert_eq!(
            tree,
            "\
trace t: 2 spans, 3.000 ms
   +0.000 ms  ? root [unspecified] 3.000 ms
   +1.000 ms    ? child [client] 1.000 ms ERROR
"
        );
        assert_eq!(parse_trace(r#"{"batches":[]}"#).unwrap(), Vec::new());
        assert!(parse_trace(r#"{"foo":1}"#).is_err());
        assert!(parse_trace(r#"{"batches":[{"scopeSpans":[{"spans":[{"name":"x"}]}]}]}"#).is_err());
    }

    #[test]
    fn m51_cycles_and_orphans_terminate() {
        let span = |id: &str, parent: Option<&str>, start: u64| Span {
            id: id.into(),
            parent: parent.map(Into::into),
            service: "s".into(),
            name: id.into(),
            kind: "server".into(),
            start_ns: start,
            end_ns: start + 1_000_000,
            error: false,
            code: None,
        };
        // a <-> b cycle, c an orphan.
        let spans = [
            span("a", Some("b"), 0),
            span("b", Some("a"), 1_000),
            span("c", Some("zz"), 2_000),
        ];
        let tree = format_tree("t", &spans);
        assert_eq!(tree.lines().count(), 4, "{tree}");
        for id in ["a", "b", "c"] {
            assert_eq!(tree.matches(&format!(" s {id} ")).count(), 1, "{tree}");
        }
    }
}
