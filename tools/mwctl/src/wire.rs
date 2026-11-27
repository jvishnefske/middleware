//! `mwctl wire`: a signal wiring from the command line (or a JSON file)
//! to a payload.
//!
//! The wiring itself (its two spellings, the checks, the compiler) is
//! `wire-core`. This module is the command around it, still pure:
//! [`Request`] is what the arguments ask for, [`resolve`] turns it and the
//! JSON file's text into a [`Built`] wiring or the problems that stop it,
//! and [`report`] is the JSON object the web page reads after every edit.
//!
//! The chip decides the pin rules: `--chip` when building a file, the
//! connected board's own `sys/info` when writing to it ([`for_chip`]).

use std::path::PathBuf;

use wire_core::graph::{Graph, MIN_PERIOD_MS};
use wire_core::{Issue, Severity, text};

/// What `--emit` prints on stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emit {
    /// The statements, one per line.
    Statements,
    /// The wiring as JSON, with a position for every node.
    Json,
    /// One JSON object for a page: `ok`, `issues`, `statements`,
    /// `command`, `graph`, `bytes`, `kinds` ([`report`]).
    Report,
}

impl Emit {
    /// Parses the `--emit` value.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "statements" => Some(Emit::Statements),
            "json" => Some(Emit::Json),
            "report" => Some(Emit::Report),
            _ => None,
        }
    }
}

/// The wiring a `wire` command names and what to do with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Statements ([`wire_core::text`]); empty with [`Request::json`].
    pub statements: Vec<String>,
    /// A JSON wiring file instead of statements.
    pub json: Option<PathBuf>,
    /// `--name`; overrides the file's.
    pub name: Option<String>,
    /// `--period` in ms; overrides the file's.
    pub period_ms: Option<u32>,
    /// `--trace`: the channels to publish on `wire/<channel>`; an empty
    /// list is every channel. Adds to the ones a JSON file already traces.
    pub trace: Option<Vec<String>>,
    /// `--chip`: check the pins against this chip's board.
    pub chip: Option<String>,
    /// `-o`: where to write the payload.
    pub out: Option<PathBuf>,
    /// `--emit`.
    pub emit: Option<Emit>,
}

/// A wiring that was read, with its check and, without errors, its payload.
#[derive(Debug, Clone, PartialEq)]
pub struct Built {
    /// The wiring, every node with a position.
    pub graph: Graph,
    /// Errors and warnings.
    pub issues: Vec<Issue>,
    /// The payload; `None` when an error stops the build.
    pub wasm: Option<Vec<u8>>,
}

/// The chips `--chip` takes, as `sys/info` reports them.
pub const CHIPS: &str = "esp32, esp32s3, esp32c3, esp8266, rp2040, rp2350, rt1050";

/// The pin map for `chip`, or why there is none.
pub fn pins_of(chip: &str) -> Result<&'static board_pins::PinMap, String> {
    board_pins::boards::by_chip(chip)
        .ok_or_else(|| format!("no pin map for chip '{chip}' (known: {CHIPS})"))
}

/// Reads the wiring `req` names (`json_text` is the JSON file's contents
/// when it names one), checks it against `req.chip` and builds it. `Err`
/// is a wiring that could not be read at all.
pub fn resolve(req: &Request, json_text: Option<&str>) -> Result<Built, String> {
    let mut graph = match json_text {
        Some(text) => {
            if !req.statements.is_empty() {
                return Err("give statements or --json <FILE>, not both".into());
            }
            Graph::from_json(text)?
        }
        None => {
            if req.statements.is_empty() {
                return Err("wire needs statements such as button=gpio15,pullup \
                            'lit=not(button)' gpio25=lit, or --json <FILE>"
                    .into());
            }
            // With the period the wiring will have, so `@Nms` of that
            // period is the default it is.
            text::parse("wire", req.period_ms.unwrap_or(MIN_PERIOD_MS), &req.statements)?
        }
    };
    if let Some(name) = &req.name {
        graph.name = name.clone();
    }
    if let Some(period) = req.period_ms {
        graph.period_ms = period;
    }
    if let Some(channels) = &req.trace {
        text::set_trace(&mut graph, channels)?;
    }
    graph.layout();
    let pins = req.chip.as_deref().map(pins_of).transpose()?;
    Ok(for_pins(graph, pins))
}

fn for_pins(graph: Graph, pins: Option<&board_pins::PinMap>) -> Built {
    match wire_core::build(&graph, pins) {
        Ok((wasm, issues)) => Built { graph, issues, wasm: Some(wasm) },
        Err(issues) => Built { graph, issues, wasm: None },
    }
}

/// The same wiring checked and built for the connected board's `chip`. A
/// chip without a pin map keeps the wiring's own rules only.
pub fn for_chip(built: Built, chip: &str) -> Built {
    for_pins(built.graph, board_pins::boards::by_chip(chip))
}

/// The issues as lines for a terminal: `error: ...` / `warning: ...`.
pub fn issue_lines(issues: &[Issue]) -> Vec<String> {
    issues
        .iter()
        .map(|i| {
            let level = match i.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
            };
            format!("{level}: {}", i.message)
        })
        .collect()
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One JSON object for a page: whether the wiring builds, every issue with
/// the node or edge it is about, the statements and the full command line
/// that spell the same wiring, the wiring itself, the payload's size, and
/// `kinds`: every node kind with its class, how many channels it reads,
/// whether it keeps state and the settings it takes
/// ([`wire_core::spec::kinds_json`], the table the checks and the compiler
/// use), for the page's palette and property editor.
pub fn report(built: &Built) -> String {
    let issues: Vec<String> = built
        .issues
        .iter()
        .map(|i| {
            let mut o = format!(
                "{{\"severity\":\"{}\"",
                if i.severity == Severity::Error { "error" } else { "warning" }
            );
            if let Some(node) = &i.node {
                o.push_str(&format!(",\"node\":{}", json_string(node)));
            }
            if let Some((from, to)) = &i.edge {
                o.push_str(&format!(",\"edge\":[{},{}]", json_string(from), json_string(to)));
            }
            o.push_str(&format!(",\"message\":{}}}", json_string(&i.message)));
            o
        })
        .collect();
    let statements: Vec<String> = text::format(&built.graph).iter().map(|s| json_string(s)).collect();
    format!(
        "{{\"ok\":{},\"issues\":[{}],\"statements\":[{}],\"command\":{},\"graph\":{},\"bytes\":{},\"kinds\":{}}}",
        built.wasm.is_some(),
        issues.join(","),
        statements.join(","),
        json_string(&text::command_line(&built.graph)),
        built.graph.to_json(),
        built.wasm.as_ref().map_or(0, Vec::len),
        wire_core::spec::kinds_json(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(statements: &[&str]) -> Request {
        Request {
            statements: statements.iter().map(|s| (*s).to_string()).collect(),
            json: None,
            name: None,
            period_ms: None,
            trace: None,
            chip: None,
            out: None,
            emit: None,
        }
    }

    const LAMP: [&str; 3] = ["button=gpio15,pullup,invert", "lit=not(button)", "gpio25=lit"];

    #[test]
    fn wr7_statements_build_a_payload_with_name_and_period() {
        let mut req = request(&LAMP);
        req.name = Some("lamp".into());
        req.period_ms = Some(20);
        let built = resolve(&req, None).unwrap();
        assert_eq!((built.graph.name.as_str(), built.graph.period_ms), ("lamp", 20));
        assert!(built.issues.is_empty());
        assert!(built.wasm.as_ref().is_some_and(|w| w.starts_with(b"\0asm")));
        assert!(built.graph.nodes.iter().all(|n| n.x.is_some() && n.y.is_some()));
    }

    #[test]
    fn wr7_the_json_file_is_the_same_wiring_and_excludes_statements() {
        let from_text = resolve(&request(&LAMP), None).unwrap();
        let json = from_text.graph.to_json();
        let from_json = resolve(&request(&[]), Some(&json)).unwrap();
        assert_eq!(from_json, from_text);
        assert!(resolve(&request(&LAMP), Some(&json)).unwrap_err().contains("not both"));
        assert!(resolve(&request(&[]), None).unwrap_err().contains("wire needs statements"));
        assert!(resolve(&request(&[]), Some("{")).unwrap_err().starts_with("wiring json:"));
        assert!(resolve(&request(&["button"]), None).unwrap_err().contains("not a wiring statement"));
    }

    #[test]
    fn wr7_the_chip_decides_the_pin_rules() {
        let wiring = ["a=gpio4", "gpio34=a"];
        assert!(resolve(&request(&wiring), None).unwrap().wasm.is_some());
        let mut req = request(&wiring);
        req.chip = Some("esp32".into());
        let built = resolve(&req, None).unwrap();
        assert!(built.wasm.is_none());
        assert_eq!(issue_lines(&built.issues), ["error: GPIO 34 is input-only on this board; it cannot be an output"]);
        req.chip = Some("z80".into());
        assert!(resolve(&req, None).unwrap_err().contains("no pin map for chip 'z80'"));
        // The connected board's chip re-checks a wiring built without one.
        let built = for_chip(resolve(&request(&wiring), None).unwrap(), "esp32");
        assert!(built.wasm.is_none());
        let built = for_chip(resolve(&request(&wiring), None).unwrap(), "linux-aarch64");
        assert!(built.wasm.is_some());
    }

    #[test]
    fn wr8_the_report_is_json_a_page_can_read() {
        let mut req = request(&["a=gpio1", "b=gpio1", "gpio2=a", "gpio2=b"]);
        assert!(resolve(&req, None).unwrap_err().contains("an output follows one"));
        req.statements = vec!["a=gpio1".into(), "y=and(a)".into(), "gpio2=y".into()];
        let built = resolve(&req, None).unwrap();
        let text = report(&built);
        assert!(text.starts_with("{\"ok\":false,\"issues\":[{\"severity\":\"error\",\"node\":\"y.fn\",\"message\":\"y.fn (and) reads 1 channel(s); it needs 2 to 8 channels\"}]"), "{text}");
        assert!(text.contains("\"statements\":[\"a=gpio1\",\"y=and(a)\",\"gpio2=y\"]"));
        assert!(text.contains("\"command\":\"mwctl wire a=gpio1 'y=and(a)' gpio2=y\""));
        assert!(text.contains("\"bytes\":0,\"kinds\":[{\"kind\":\"input\","));

        let built = resolve(&request(&LAMP), None).unwrap();
        let text = report(&built);
        assert!(text.starts_with("{\"ok\":true,\"issues\":[],"));
        let size = built.wasm.as_ref().unwrap().len();
        assert!(text.contains(&format!(",\"bytes\":{size},\"kinds\":[")));
        // The embedded graph parses back to the same wiring.
        let at = text.find("\"graph\":").unwrap() + 8;
        let end = text.rfind(",\"bytes\"").unwrap();
        assert_eq!(Graph::from_json(&text[at..end]).unwrap(), built.graph);
        assert_eq!(json_string("a\"b\\c\n\u{1}"), "\"a\\\"b\\\\c\\n\\u0001\"");
    }

    #[test]
    fn wr8_the_report_lists_every_kind_for_the_page() {
        let built = resolve(&request(&LAMP), None).unwrap();
        let text = report(&built);
        // The fields a page already reads come first, in their old order.
        let at = |field: &str| text.find(&format!("\"{field}\":")).unwrap_or_else(|| panic!("{field}"));
        let order = ["ok", "issues", "statements", "command", "graph", "bytes", "kinds"].map(at);
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
        let kinds = &text[at("kinds") + 8..text.len() - 1];
        assert_eq!(kinds, wire_core::spec::kinds_json());
        assert!(kinds.contains(
            "{\"kind\":\"hyst\",\"class\":\"function\",\"inputs\":[1,1],\"outputs\":[1,1],\"stateful\":true,\"params\":[\
             {\"name\":\"lo\",\"type\":\"int\",\"min\":-2147483648,\"max\":2147483647,\"default\":null,\"required\":true},\
             {\"name\":\"hi\",\"type\":\"int\",\"min\":-2147483648,\"max\":2147483647,\"default\":null,\"required\":true},\
             {\"name\":\"period_ms\",\"type\":\"int\",\"min\":10,\"max\":3600000,\"default\":null,\"required\":false}]}"
        ));
        for class in ["source", "function", "sink", "channel"] {
            assert!(kinds.contains(&format!("\"class\":\"{class}\"")), "{class}");
        }
        // A wiring with errors still gets the table.
        let built = resolve(&request(&["a=adc1", "y=lowpass(a)"]), None).unwrap();
        assert!(built.wasm.is_none());
        assert!(report(&built).ends_with(&format!(",\"bytes\":0,\"kinds\":{}}}", wire_core::spec::kinds_json())));
    }

    #[test]
    fn ms1_rates_and_traces_round_trip_through_the_command_line() {
        let statements = ["raw=adc26", "slow=mean(raw,n=10)@100ms", "hot=hyst(slow,lo=1000,hi=1200)@100ms", "gpio25=hot@1000ms", "pwm0.1=slow,freq=2000"];
        let mut req = request(&statements);
        req.period_ms = Some(20);
        req.trace = Some(vec!["slow".into(), "hot".into()]);
        let built = resolve(&req, None).unwrap();
        assert!(built.issues.is_empty(), "{:?}", built.issues);
        assert!(built.wasm.is_some());
        let traced: Vec<&str> =
            built.graph.nodes.iter().filter(|n| n.config.trace).map(|n| n.id.as_str()).collect();
        assert_eq!(traced, ["slow", "hot"]);
        assert_eq!(built.graph.node("slow.fn").unwrap().config.period_ms, Some(100));
        let command = text::command_line(&built.graph);
        assert_eq!(
            command,
            "mwctl wire --period 20 --trace slow,hot raw=adc26 'slow=mean(raw,n=10)@100ms' \
             'hot=hyst(slow,lo=1000,hi=1200)@100ms' gpio25=hot@1000ms pwm0.1=slow,freq=2000"
        );
        assert!(report(&built).contains(&json_string(&command)));
        // The command line parses back to the same request and wiring.
        let words: Vec<String> = command.split(' ').skip(1).map(|w| w.trim_matches('\'').to_string()).collect();
        let Ok(crate::cli::Parsed::Wire(again)) = crate::cli::parse(words.into_iter().chain(["--emit".to_string(), "report".to_string()])) else {
            panic!("the command line parses");
        };
        assert_eq!(again.request.trace, req.trace);
        assert_eq!(resolve(&again.request, None).unwrap(), built);
        // The JSON file carries rates and traces itself; --trace adds to them.
        let mut from_json = request(&[]);
        let json = built.graph.to_json();
        assert_eq!(resolve(&from_json, Some(&json)).unwrap(), built);
        from_json.trace = Some(Vec::new());
        let all = resolve(&from_json, Some(&json)).unwrap();
        assert!(text::command_line(&all.graph).starts_with("mwctl wire --period 20 --trace raw=adc26 "));
        assert_eq!(all.graph.nodes.iter().filter(|n| n.config.trace).count(), 3);

        // A period of the wiring's own is the default, whatever --period is.
        let mut req = request(&["a=gpio1@20ms", "gpio2=a@40ms"]);
        req.period_ms = Some(20);
        let built = resolve(&req, None).unwrap();
        assert_eq!(text::format(&built.graph), ["a=gpio1", "gpio2=a@40ms"]);
        // Without --period the base is 10 ms, and 15 ms is not a multiple of it.
        let built = resolve(&request(&["a=gpio1@15ms", "gpio2=a"]), None).unwrap();
        assert_eq!(issue_lines(&built.issues), ["error: gpio1 updates every 15 ms; that must be a multiple of the scan period (10 ms), at most 3600000 ms"]);

        let mut req = request(&LAMP);
        req.trace = Some(vec!["lit".into(), "lamp".into()]);
        assert_eq!(resolve(&req, None).unwrap_err(), "--trace: the wiring has no channel 'lamp'");
    }

    #[test]
    fn kf1_model_blocks_round_trip_as_statements_and_json() {
        let statements = [
            "z=adc26",
            "push=500",
            "pos,vel=ukf(z,n=2,f=\"x0+dt*x1; x1\",h=\"x0\",q=\"1e-6;1e-4\",r=\"0.01\")@20ms",
            "u,flag=cbf(push,pos,n=1,h=\"2 - x0\",f=\"0\",g=\"1\",alpha=2.5,umax=0.75)",
            "pwm0.0=u",
            "gpio2=flag",
        ];
        let mut req = request(&statements);
        req.trace = Some(vec!["pos".into(), "vel".into()]);
        let built = resolve(&req, None).unwrap();
        assert_eq!(issue_lines(&built.issues), [""; 0]);
        assert!(built.wasm.as_ref().is_some_and(|w| w.len() < 4096));
        // --emit statements: the same statements, which build the same wiring.
        let emitted = text::format(&built.graph);
        assert_eq!(emitted, statements);
        let mut again = request(&emitted.iter().map(String::as_str).collect::<Vec<_>>());
        again.trace = req.trace.clone();
        assert_eq!(resolve(&again, None).unwrap(), built);
        // --emit json: the file builds the same wiring, expressions and all.
        let json = built.graph.to_json();
        assert!(json.contains(r#""kind":"ukf","config":{"n":2,"f":"x0+dt*x1; x1","h":"x0","q":"1e-6;1e-4","r":"0.01","period_ms":20}"#), "{json}");
        assert!(json.contains(r#""kind":"cbf","config":{"n":1,"f":"0","h":"2 - x0","g":"1","alpha":2.5,"umax":0.75}"#), "{json}");
        assert_eq!(resolve(&request(&[]), Some(&json)).unwrap(), built);
        // The report: the command line quotes the statements for a shell,
        // and the embedded wiring is that JSON.
        let text = report(&built);
        let command = text::command_line(&built.graph);
        assert!(command.contains(" --trace pos,vel z=adc26 push=500 'pos,vel=ukf(z,n=2,f=\"x0+dt*x1; x1\",h=\"x0\",q=\"1e-6;1e-4\",r=\"0.01\")@20ms' 'u,flag=cbf("), "{command}");
        assert!(text.contains(&json_string(&command)) && text.contains(&json));
        // A mistake in a model is an issue on its node, in the report too.
        let built = resolve(&request(&["z=adc26", "x=ukf(z,n=1,f=\"x0+y\",h=\"x0\",q=\"0\",r=\"1\")"]), None).unwrap();
        assert!(built.wasm.is_none());
        assert!(report(&built).contains("{\"severity\":\"error\",\"node\":\"x.fn\",\"message\":\"x.fn (ukf) has a mistake in f, expression 1, column 4: 'y' is not a name here (x0, dt, pi)\"}"));
    }

    #[test]
    fn kf2_the_report_describes_the_model_blocks_to_the_page() {
        let kinds = wire_core::spec::kinds_json();
        let parsed: Vec<&str> = kinds.split("{\"kind\":").skip(1).collect();
        assert_eq!(parsed.len(), 41);
        // Every kind says how many values it drives.
        assert!(parsed.iter().all(|k| k.contains("\"outputs\":[")));
        let of = |name: &str| *parsed.iter().find(|k| k.starts_with(&format!("\"{name}\""))).unwrap_or_else(|| panic!("{name}"));
        assert!(of("ukf").starts_with("\"ukf\",\"class\":\"function\",\"inputs\":[1,8],\"outputs\":[1,8],\"stateful\":true,"));
        assert!(of("cbf").starts_with("\"cbf\",\"class\":\"function\",\"inputs\":[2,6],\"outputs\":[1,3],\"stateful\":false,"));
        assert!(of("output").contains("\"outputs\":[0,0]") && of("and").contains("\"outputs\":[1,1]") && of("input").contains("\"outputs\":[1,1]"));
        let names = |kind: &str| -> Vec<&str> {
            of(kind).split("{\"name\":\"").skip(1).map(|p| p.split('"').next().unwrap()).collect()
        };
        assert_eq!(names("ukf"), ["n", "f", "h", "q", "r", "x0", "p0", "alpha", "beta", "kappa", "scale", "period_ms"]);
        assert_eq!(names("cbf"), ["n", "h", "f", "g", "alpha", "umin", "umax", "scale", "period_ms"]);
        assert!(of("ukf").contains("{\"name\":\"f\",\"type\":\"text\",\"min\":null,\"max\":null,\"default\":null,\"required\":true,\"hint\":\"x0+dt*x1; x1\"}"));
        assert!(of("cbf").contains("{\"name\":\"alpha\",\"type\":\"real\",\"min\":0.000000001,\"max\":1000000000,\"default\":1,\"required\":false}"));
        assert!(report(&resolve(&request(&LAMP), None).unwrap()).ends_with(&format!("\"kinds\":{kinds}}}")));
    }
}
