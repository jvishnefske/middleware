//! `mw-update`'s command line, parsed purely by [`parse`].

use std::path::PathBuf;
use std::time::Duration;

use mw_service::endpoints::{StaticEndpoint, StaticEndpoints};

use crate::shell::SYS_KIND;

/// Usage text.
pub const HELP: &str = "\
mw-update - signed payload and RP2350 firmware updates through an mw-gateway

USAGE:
    mw-update [--endpoint <ENTRY>] [--timeout <MS>] [--attempts <N>] <COMMAND>

COMMANDS:
    info                       sys/info and sys/trust/info of the gateway's device
    upload <FILE.wasm>         Install a payload (sys/loader begin/chunk/commit)
        --key <KEYFILE>            sign the commit (required: the gateway forwards
                                   only signed commits, to a claimed device)
        --version <N>              signed version, above the device's last one
                                   [default: current Unix time in seconds]
        --start                    then send a signed sys/payload/start
    start | stop               sys/payload/start or stop, signed (required)
        --key <KEYFILE>            the device's pinned key
        --counter <N>              control counter, above the last accepted
                                   [default: current Unix time in milliseconds]
    fw info                    RP2350: firmware update state
    fw update <IMAGE>          RP2350: install a firmware image (UF2 or raw .bin) in
                               the inactive slot, reboot into it on trial and confirm
        --key <KEYFILE>            sign the commit (required)
        --version <MAJOR.MINOR>    expected image version (checked against its IMAGE_DEF)
        --no-reboot                stop after the commit
    fw reboot | fw confirm | fw abort

OPTIONS:
    --endpoint <ENTRY>  The gateway, as an MW_STATIC_ENDPOINTS entry:
                        gw/sys=[http://]<dial host:port>@<authority host:port>
                        [default: the gw/sys entry of MW_STATIC_ENDPOINTS]
    --timeout <MS>      Per-attempt deadline [default: 2000]; begin and confirm
                        wait at least 5000 ms, a firmware commit 15000 ms
    --attempts <N>      Attempts per request, resent with the same request id
                        [default: 3]
    -h, --help          Print this help

The caller must be in the gateway's MW_GATEWAY_UPDATERS (its SPIFFE ID, as
the gateway's sidecar sees it). Exit status: 0 on success, 1 when the
update fails, 2 on a usage error.
";

/// Default per-attempt deadline.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(2000);
/// Default attempts per request.
pub const DEFAULT_ATTEMPTS: u32 = 3;

/// A command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// `info`.
    Info,
    /// `upload <file>`.
    Upload {
        /// The payload image.
        file: PathBuf,
        /// The signing key file.
        key: PathBuf,
        /// The signed version, if given.
        version: Option<u32>,
        /// Start it afterwards.
        start: bool,
        /// The start's control counter, if given.
        counter: Option<u64>,
    },
    /// `start` (true) or `stop` (false).
    Control {
        /// Start, not stop.
        start: bool,
        /// The signing key file.
        key: PathBuf,
        /// The control counter, if given.
        counter: Option<u64>,
    },
    /// `fw info`.
    FwInfo,
    /// `fw update <image>`.
    FwUpdate {
        /// The firmware image.
        file: PathBuf,
        /// The signing key file.
        key: PathBuf,
        /// The expected version (`MAJOR.MINOR`), if given.
        version: Option<String>,
        /// Reboot and confirm after the commit.
        reboot: bool,
    },
    /// `fw reboot`.
    FwReboot,
    /// `fw confirm`.
    FwConfirm,
    /// `fw abort`.
    FwAbort,
}

/// A parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    /// Where `gw/sys` is.
    pub endpoint: StaticEndpoint,
    /// Per-attempt deadline.
    pub timeout: Duration,
    /// Attempts per request.
    pub attempts: u32,
    /// What to do.
    pub cmd: Cmd,
}

/// What the arguments ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    /// Run this.
    Run(Cli),
    /// Print [`HELP`].
    Help,
}

/// Parses the arguments after the program name, with `static_endpoints`
/// the value of `MW_STATIC_ENDPOINTS` (if set). Pure; the error is the
/// message to print before [`HELP`].
pub fn parse<I, S>(args: I, static_endpoints: Option<&str>) -> Result<Parsed, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args: Vec<String> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
    let mut endpoint: Option<String> = None;
    let mut timeout = DEFAULT_TIMEOUT;
    let mut attempts = DEFAULT_ATTEMPTS;
    let mut key: Option<PathBuf> = None;
    let mut version: Option<String> = None;
    let mut counter: Option<u64> = None;
    let mut start = false;
    let mut no_reboot = false;
    let mut words: Vec<String> = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "--endpoint" => endpoint = Some(value("--endpoint")?),
            "--timeout" => {
                let v = value("--timeout")?;
                timeout = v
                    .parse::<u64>()
                    .ok()
                    .filter(|ms| (1..=120_000).contains(ms))
                    .map(Duration::from_millis)
                    .ok_or_else(|| format!("--timeout {v}: not 1..=120000 milliseconds"))?;
            }
            "--attempts" => {
                let v = value("--attempts")?;
                attempts = v
                    .parse::<u32>()
                    .ok()
                    .filter(|n| (1..=10).contains(n))
                    .ok_or_else(|| format!("--attempts {v}: not 1..=10"))?;
            }
            "--key" => key = Some(PathBuf::from(value("--key")?)),
            "--version" => version = Some(value("--version")?),
            "--counter" => {
                let v = value("--counter")?;
                counter = Some(
                    v.parse()
                        .map_err(|_| format!("--counter {v}: not a number"))?,
                );
            }
            "--start" => start = true,
            "--no-reboot" => no_reboot = true,
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            _ => words.push(a),
        }
    }
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    let need_key = |key: &Option<PathBuf>| {
        key.clone().ok_or_else(|| {
            "--key is required: the gateway forwards only signed requests".to_owned()
        })
    };
    let refuse = |what: &str, cmd: &str| Err(format!("{what} is not an option of {cmd}"));
    let cmd = match words.as_slice() {
        ["info"] | ["fw", "info"] | ["fw", "reboot"] | ["fw", "confirm"] | ["fw", "abort"] => {
            if key.is_some() || version.is_some() || counter.is_some() || start || no_reboot {
                return refuse(
                    "--key/--version/--counter/--start/--no-reboot",
                    &words.join(" "),
                );
            }
            match words.as_slice() {
                ["info"] => Cmd::Info,
                ["fw", "info"] => Cmd::FwInfo,
                ["fw", "reboot"] => Cmd::FwReboot,
                ["fw", "confirm"] => Cmd::FwConfirm,
                _ => Cmd::FwAbort,
            }
        }
        ["upload", file] => {
            if no_reboot {
                return refuse("--no-reboot", "upload");
            }
            if counter.is_some() && !start {
                return refuse("--counter without --start", "upload");
            }
            let version = version
                .map(|v| {
                    v.parse::<u32>()
                        .map_err(|_| format!("--version {v}: not a number"))
                })
                .transpose()?;
            Cmd::Upload {
                file: PathBuf::from(file),
                key: need_key(&key)?,
                version,
                start,
                counter,
            }
        }
        [op @ ("start" | "stop")] => {
            if version.is_some() || start || no_reboot {
                return refuse("--version/--start/--no-reboot", op);
            }
            Cmd::Control {
                start: *op == "start",
                key: need_key(&key)?,
                counter,
            }
        }
        ["fw", "update", file] => {
            if start || counter.is_some() {
                return refuse("--start/--counter", "fw update");
            }
            Cmd::FwUpdate {
                file: PathBuf::from(file),
                key: need_key(&key)?,
                version,
                reboot: !no_reboot,
            }
        }
        [] => return Err("no command".into()),
        other => return Err(format!("unknown command {:?}", other.join(" "))),
    };
    let entries = match (endpoint.as_deref(), static_endpoints) {
        (Some(e), _) => e,
        (None, Some(env)) if !env.trim().is_empty() => env,
        _ => return Err("no gateway: give --endpoint or set MW_STATIC_ENDPOINTS".into()),
    };
    let parsed = StaticEndpoints::parse(entries).map_err(|e| e.to_string())?;
    let endpoint = parsed
        .get(SYS_KIND)
        .cloned()
        .ok_or_else(|| format!("no {SYS_KIND} entry in {entries:?}"))?;
    Ok(Parsed::Run(Cli {
        endpoint,
        timeout,
        attempts,
        cmd,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EP: &str = "gw/sys=127.0.0.1:15001@mw-gateway.robots.svc.cluster.local:50051";

    fn run(args: &[&str], env: Option<&str>) -> Result<Cli, String> {
        match parse(args, env)? {
            Parsed::Run(c) => Ok(c),
            Parsed::Help => Err("help".into()),
        }
    }

    #[test]
    fn rg24_command_line_parses_each_command() {
        let c = run(&["--endpoint", EP, "info"], None).unwrap();
        assert_eq!(c.cmd, Cmd::Info);
        assert_eq!((c.timeout, c.attempts), (DEFAULT_TIMEOUT, DEFAULT_ATTEMPTS));
        assert_eq!(c.endpoint.dial(), "127.0.0.1:15001");
        assert_eq!(
            c.endpoint.authority(),
            "mw-gateway.robots.svc.cluster.local:50051"
        );
        // The environment's entry when no --endpoint.
        let env = format!("demo/echo=127.0.0.1:1@a:1,{EP}");
        let c = run(
            &[
                "upload",
                "p.wasm",
                "--key",
                "k",
                "--start",
                "--version",
                "7",
            ],
            Some(&env),
        )
        .unwrap();
        assert_eq!(
            c.cmd,
            Cmd::Upload {
                file: "p.wasm".into(),
                key: "k".into(),
                version: Some(7),
                start: true,
                counter: None
            }
        );
        let c = run(
            &[
                "--endpoint",
                EP,
                "stop",
                "--key",
                "k",
                "--counter",
                "9",
                "--timeout",
                "500",
                "--attempts",
                "5",
            ],
            None,
        )
        .unwrap();
        assert_eq!(
            c.cmd,
            Cmd::Control {
                start: false,
                key: "k".into(),
                counter: Some(9)
            }
        );
        assert_eq!((c.timeout, c.attempts), (Duration::from_millis(500), 5));
        let c = run(
            &[
                "--endpoint",
                EP,
                "fw",
                "update",
                "fw.uf2",
                "--key",
                "k",
                "--version",
                "1.2",
                "--no-reboot",
            ],
            None,
        )
        .unwrap();
        assert_eq!(
            c.cmd,
            Cmd::FwUpdate {
                file: "fw.uf2".into(),
                key: "k".into(),
                version: Some("1.2".into()),
                reboot: false
            }
        );
        for (w, cmd) in [
            ("info", Cmd::FwInfo),
            ("reboot", Cmd::FwReboot),
            ("confirm", Cmd::FwConfirm),
            ("abort", Cmd::FwAbort),
        ] {
            assert_eq!(run(&["--endpoint", EP, "fw", w], None).unwrap().cmd, cmd);
        }
        assert_eq!(parse(["--help"], None), Ok(Parsed::Help));
    }

    #[test]
    fn rg24_command_line_errors() {
        for (args, env) in [
            (&["info"][..], None),
            (
                &["--endpoint", "demo/echo=127.0.0.1:1@a:1", "info"][..],
                None,
            ),
            (&["--endpoint", "gw/sys=nonsense", "info"][..], None),
            (&["--endpoint", EP, "upload", "p.wasm"][..], None),
            (&["--endpoint", EP, "start"][..], None),
            (&["--endpoint", EP, "fw", "update", "x"][..], None),
            (&["--endpoint", EP, "info", "--key", "k"][..], None),
            (
                &["--endpoint", EP, "upload", "p", "--key", "k", "--no-reboot"][..],
                None,
            ),
            (
                &[
                    "--endpoint",
                    EP,
                    "upload",
                    "p",
                    "--key",
                    "k",
                    "--counter",
                    "1",
                ][..],
                None,
            ),
            (
                &[
                    "--endpoint",
                    EP,
                    "upload",
                    "p",
                    "--key",
                    "k",
                    "--version",
                    "x",
                ][..],
                None,
            ),
            (
                &["--endpoint", EP, "start", "--key", "k", "--start"][..],
                None,
            ),
            (&["--endpoint", EP, "--timeout", "0", "info"][..], None),
            (&["--endpoint", EP, "--attempts", "11", "info"][..], None),
            (&["--endpoint", EP, "--bogus", "info"][..], None),
            (&["--endpoint", EP, "flash"][..], None),
            (&["--endpoint", EP][..], None),
            (&["--endpoint"][..], None),
            (&["info"][..], Some("  ")),
        ] {
            assert!(run(args, env).is_err(), "{args:?}");
        }
    }
}
