//! Command-line parsing (pure).
//!
//! [`parse`] turns the arguments after the program name into a [`Cli`]
//! (device commands, which need `--port`), a local [`KeyCmd`] (no port) or
//! a [`Parsed::Help`] request. Options may appear before or after the
//! command; command-specific options are rejected on other commands.
//! [`HELP`] is the usage text printed by `--help` and on usage errors.
//!
//! `--espnow <MAC>` makes `--port` the USB↔ESP-NOW bridge and sends the
//! command over the air to the device with that MAC (default baud
//! [`crate::bridge::BAUD`]); only `info`, `upload`, `start`, `stop` and
//! `trust info|claim` travel over the air (`monitor` and `reset` act on the
//! port's own board). Over the air `upload`, `start` and `stop` need
//! `--key` (a claimed device accepts only signed commits and signed
//! control requests there); `start`/`stop --key` is refused without
//! `--espnow`, where they need no signature. `--counter` overrides the
//! control counter (default: Unix time in milliseconds).

use std::path::PathBuf;
use std::time::Duration;

/// Usage text.
pub const HELP: &str = "\
mwctl - talk to the middleware ESP base firmware over one serial port

USAGE:
    mwctl --port <PATH> [--baud <N>] [--timeout <MS>] <COMMAND>
    mwctl --port <BRIDGE> --espnow <MAC> <COMMAND>    (over the air, see below)
    mwctl key new <KEYFILE> | key pub <KEYFILE>

COMMANDS:
    info                       Print chip, firmware, ABI, capacity and payload status
    upload <FILE.wasm>         Install a payload (sys/loader begin/chunk/commit)
        --key <KEYFILE>            sign the commit (Ed25519); without it the commit is
                                   unsigned, which only an unclaimed device accepts
        --version <N>              signed version, must exceed the device's last one
                                   [default: current Unix time in seconds]
        --start                    then send sys/payload/start (signed with --key
                                   over --espnow)
        --corrupt-chunk <INDEX>    test hook: flip a byte in that chunk's data but
                                   declare the original CRC, so commit must fail
    start                      sys/payload/start
    stop                       sys/payload/stop
        --key <KEYFILE>            (with --espnow, required) sign the request for the
                                   device named by --espnow (Ed25519 control request)
        --counter <N>              control counter, must exceed the last one the device
                                   accepted [default: current Unix time in milliseconds]
    monitor [--seconds <N>]    Print sys/log and sys/payload/status publications
                               (and other publications) until N seconds pass or Ctrl-C
    reset                      Reboot into run mode: RTS asserted for 100 ms with DTR
                               deasserted (EN pulse through the DTR/RTS auto-reset circuit)
    trust info                 Print the root of trust: unclaimed, or the pinned key and
                               the last signed version installed
    trust claim --key <KEYFILE>
                               Pin KEYFILE's public key (sys/trust/claim). Accepted when
                               unclaimed, or within 60 s after BOOT was held for 3 s
    key new <KEYFILE>          Create a signing key: 32 random bytes (the Ed25519 seed),
                               mode 0600, never overwriting; prints the public key
    key pub <KEYFILE>          Print a key file's public key (hex)

OPTIONS:
    --port <PATH>      Serial port to open, e.g. /dev/ttyUSB0 (required; never scanned)
    --baud <N>         Baud rate [default: 115200]
    --timeout <MS>     Per-attempt response timeout [default: 1000]; each request is
                       retried once on timeout, sys/loader/begin waits at least 5000 ms
    --espnow <MAC>     Talk to the device with this Wi-Fi MAC over ESP-NOW, through the
                       USB-ESP-NOW bridge (tools/espnow-bridge) on --port [baud default
                       460800]. Requests go unicast with retransmission (300 ms per
                       attempt, 5000 ms for begin/commit/start, 10 attempts); uploads use
                       217-byte chunks. The device answers only when claimed, only
                       info, trust info, upload, start and stop; upload, start and stop
                       need --key (signed commit, signed control request). An upload
                       stages beside the running payload, which keeps running until
                       the signed commit. trust claim is always refused over the air.
                       Not for monitor or reset.
    -h, --help         Print this help

The port is opened with DTR and RTS deasserted so opening it neither resets
the board nor selects the ROM bootloader. Exit status is 0 on success, 1 when
the device or port fails, 2 on a usage error.
";

/// Default baud rate of the base firmware's console UART.
pub const DEFAULT_BAUD: u32 = 115_200;
/// Default per-attempt timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(1000);

/// A local key-file command (no device, no port).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyCmd {
    /// `key new <file>`.
    New {
        /// Key file to create.
        file: PathBuf,
    },
    /// `key pub <file>`.
    Pub {
        /// Key file to read.
        file: PathBuf,
    },
}

/// A device subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// `info`.
    Info,
    /// `upload`.
    Upload {
        /// Payload image.
        file: PathBuf,
        /// Start after commit.
        start: bool,
        /// Test hook: chunk to corrupt.
        corrupt_chunk: Option<usize>,
        /// Signing key file; `None` sends an unsigned commit.
        key: Option<PathBuf>,
        /// Signed version; `None` uses the current Unix time.
        version: Option<u32>,
    },
    /// `trust info`.
    TrustInfo,
    /// `trust claim --key <file>`.
    TrustClaim {
        /// Key file whose public key is pinned.
        key: PathBuf,
    },
    /// `start`.
    Start {
        /// Signing key for a control request (`--espnow` only).
        key: Option<PathBuf>,
        /// Control counter; `None` uses the current Unix time in ms.
        counter: Option<u64>,
    },
    /// `stop`.
    Stop {
        /// Signing key for a control request (`--espnow` only).
        key: Option<PathBuf>,
        /// Control counter; `None` uses the current Unix time in ms.
        counter: Option<u64>,
    },
    /// `monitor`; `None` runs until interrupted.
    Monitor {
        /// How long to listen.
        seconds: Option<u64>,
    },
    /// `reset`.
    Reset,
}

/// Parsed arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    /// Serial port path.
    pub port: String,
    /// Baud rate.
    pub baud: u32,
    /// Per-attempt timeout.
    pub timeout: Duration,
    /// Over ESP-NOW to the device with this MAC, through the bridge on
    /// `port`.
    pub espnow: Option<[u8; 6]>,
    /// What to do.
    pub cmd: Cmd,
}

/// Result of a successful parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    /// Run a device command.
    Run(Cli),
    /// Run a local key command.
    Key(KeyCmd),
    /// Print [`HELP`] and exit successfully.
    Help,
}

fn number<T: std::str::FromStr>(flag: &str, v: Option<String>) -> Result<T, String> {
    let v = v.ok_or_else(|| format!("{flag} needs a value"))?;
    v.parse()
        .map_err(|_| format!("{flag}: '{v}' is not a valid number"))
}

/// Parses the arguments (without the program name).
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Parsed, String> {
    let mut args = args.into_iter();
    let mut port = None;
    let mut baud = None;
    let mut espnow = None;
    let mut timeout = DEFAULT_TIMEOUT;
    let mut positional: Vec<String> = Vec::new();
    let mut start = false;
    let mut corrupt_chunk = None;
    let mut seconds = None;
    let mut key: Option<PathBuf> = None;
    let mut version = None;
    let mut counter = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "--port" => port = Some(args.next().ok_or("--port needs a path")?),
            "--baud" => baud = Some(number("--baud", args.next())?),
            "--espnow" => {
                let v = args
                    .next()
                    .ok_or("--espnow needs a MAC such as e4:b0:63:81:d3:24")?;
                espnow = Some(
                    crate::bridge::parse_mac(&v)
                        .ok_or_else(|| format!("--espnow: '{v}' is not a MAC"))?,
                );
            }
            "--timeout" => timeout = Duration::from_millis(number("--timeout", args.next())?),
            "--start" => start = true,
            "--corrupt-chunk" => corrupt_chunk = Some(number("--corrupt-chunk", args.next())?),
            "--seconds" => seconds = Some(number("--seconds", args.next())?),
            "--key" => key = Some(PathBuf::from(args.next().ok_or("--key needs a key file")?)),
            "--version" => version = Some(number("--version", args.next())?),
            "--counter" => counter = Some(number("--counter", args.next())?),
            s if s.starts_with('-') => return Err(format!("unknown option '{s}'")),
            _ => positional.push(a),
        }
    }
    let baud = baud.unwrap_or(if espnow.is_some() {
        crate::bridge::BAUD
    } else {
        DEFAULT_BAUD
    });
    if baud == 0 {
        return Err("--baud must be positive".into());
    }
    if timeout.is_zero() {
        return Err("--timeout must be positive".into());
    }
    let mut pos = positional.into_iter();
    let first = pos.next().ok_or("no command given")?;
    // Two-word commands are named "trust info", "key new", ...
    let name = match first.as_str() {
        "trust" | "key" => {
            let sub = pos
                .next()
                .ok_or_else(|| format!("'{first}' needs a subcommand"))?;
            format!("{first} {sub}")
        }
        _ => first,
    };
    let only = |flag: &str, set: bool, cmds: &[&str]| {
        if set && !cmds.contains(&name.as_str()) {
            Err(format!("{flag} applies only to '{}'", cmds.join("' or '")))
        } else {
            Ok(())
        }
    };
    only("--start", start, &["upload"])?;
    only("--corrupt-chunk", corrupt_chunk.is_some(), &["upload"])?;
    only("--seconds", seconds.is_some(), &["monitor"])?;
    only(
        "--key",
        key.is_some(),
        &["upload", "trust claim", "start", "stop"],
    )?;
    only("--version", version.is_some(), &["upload"])?;
    only("--counter", counter.is_some(), &["start", "stop"])?;
    if version.is_some() && key.is_none() {
        return Err("--version needs --key (only signed commits carry a version)".into());
    }
    if counter.is_some() && key.is_none() {
        return Err("--counter needs --key (only signed control requests carry a counter)".into());
    }
    let control = matches!(name.as_str(), "start" | "stop");
    if control && key.is_some() && espnow.is_none() {
        return Err(format!(
            "'{name} --key' signs a control request for --espnow; over serial '{name}' needs no signature"
        ));
    }
    if espnow.is_some() && key.is_none() && (control || name == "upload") {
        return Err(format!(
            "over ESP-NOW a claimed device accepts '{name}' only signed by its pinned key: pass --key <KEYFILE>"
        ));
    }
    let local = match name.as_str() {
        "key new" => Some(KeyCmd::New {
            file: PathBuf::from(pos.next().ok_or("key new needs a key file")?),
        }),
        "key pub" => Some(KeyCmd::Pub {
            file: PathBuf::from(pos.next().ok_or("key pub needs a key file")?),
        }),
        _ => None,
    };
    if let Some(k) = local {
        if let Some(extra) = pos.next() {
            return Err(format!("unexpected argument '{extra}'"));
        }
        return Ok(Parsed::Key(k));
    }
    let cmd = match name.as_str() {
        "info" => Cmd::Info,
        "upload" => Cmd::Upload {
            file: PathBuf::from(pos.next().ok_or("upload needs a .wasm file")?),
            start,
            corrupt_chunk,
            key,
            version,
        },
        "trust info" => Cmd::TrustInfo,
        "trust claim" => Cmd::TrustClaim {
            key: key.ok_or("trust claim needs --key <KEYFILE>")?,
        },
        "start" => Cmd::Start { key, counter },
        "stop" => Cmd::Stop { key, counter },
        "monitor" => Cmd::Monitor { seconds },
        "reset" => Cmd::Reset,
        other => return Err(format!("unknown command '{other}'")),
    };
    if let Some(extra) = pos.next() {
        return Err(format!("unexpected argument '{extra}'"));
    }
    if espnow.is_some() && matches!(cmd, Cmd::Monitor { .. } | Cmd::Reset) {
        return Err(format!(
            "'{name}' does not go over the air; drop --espnow and use the device's own port"
        ));
    }
    let port = port.ok_or("--port <PATH> is required (mwctl never scans for ports)")?;
    Ok(Parsed::Run(Cli {
        port,
        baud,
        timeout,
        espnow,
        cmd,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Parsed, String> {
        parse(s.split_whitespace().map(String::from))
    }

    fn run(s: &str) -> Cli {
        match p(s) {
            Ok(Parsed::Run(c)) => c,
            other => panic!("{s}: {other:?}"),
        }
    }

    #[test]
    fn o9_espnow_selects_the_bridge_and_refuses_local_commands() {
        let c = run("--port /dev/ttyUSB0 --espnow e4:b0:63:81:d3:24 upload a.wasm --key k --start");
        assert_eq!(c.espnow, Some([0xe4, 0xb0, 0x63, 0x81, 0xd3, 0x24]));
        assert_eq!(c.baud, crate::bridge::BAUD);
        assert_eq!(
            run("--port p --espnow e4:b0:63:81:d3:24 --baud 115200 info").baud,
            115_200
        );
        assert_eq!(run("--port p info").espnow, None);
        assert_eq!(
            run("--port p --espnow e4:b0:63:81:d3:24 trust claim --key k").cmd,
            Cmd::TrustClaim { key: "k".into() }
        );
        assert_eq!(
            run("--port p --espnow e4:b0:63:81:d3:24 stop --key k").cmd,
            Cmd::Stop {
                key: Some("k".into()),
                counter: None
            }
        );
        assert_eq!(
            run("--port p --espnow e4:b0:63:81:d3:24 start --key k --counter 77").cmd,
            Cmd::Start {
                key: Some("k".into()),
                counter: Some(77)
            }
        );
        for bad in [
            "--port p --espnow e4:b0:63:81:d3 info",
            "--port p --espnow",
            "--port p --espnow e4:b0:63:81:d3:24 monitor",
            "--port p --espnow e4:b0:63:81:d3:24 reset",
        ] {
            assert!(p(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn parses_every_command() {
        let c = run("--port /dev/ttyUSB0 info");
        assert_eq!(
            c,
            Cli {
                port: "/dev/ttyUSB0".into(),
                baud: 115_200,
                timeout: DEFAULT_TIMEOUT,
                espnow: None,
                cmd: Cmd::Info
            }
        );
        assert_eq!(
            run("upload a.wasm --start --port p --baud 921600 --corrupt-chunk 3"),
            Cli {
                port: "p".into(),
                baud: 921_600,
                timeout: DEFAULT_TIMEOUT,
                espnow: None,
                cmd: Cmd::Upload {
                    file: "a.wasm".into(),
                    start: true,
                    corrupt_chunk: Some(3),
                    key: None,
                    version: None,
                }
            }
        );
        assert_eq!(
            run("--port p start").cmd,
            Cmd::Start {
                key: None,
                counter: None
            }
        );
        assert_eq!(
            run("--port p stop").cmd,
            Cmd::Stop {
                key: None,
                counter: None
            }
        );
        assert_eq!(run("--port p reset").cmd, Cmd::Reset);
        assert_eq!(run("--port p monitor").cmd, Cmd::Monitor { seconds: None });
        assert_eq!(
            run("--port p monitor --seconds 5").cmd,
            Cmd::Monitor { seconds: Some(5) }
        );
        assert_eq!(
            run("--port p --timeout 250 info").timeout,
            Duration::from_millis(250)
        );
        assert_eq!(p("info --help"), Ok(Parsed::Help));
    }

    #[test]
    fn parses_trust_key_and_signed_upload_commands() {
        assert_eq!(run("--port p trust info").cmd, Cmd::TrustInfo);
        assert_eq!(
            run("trust claim --key k.key --port p").cmd,
            Cmd::TrustClaim {
                key: "k.key".into()
            }
        );
        assert_eq!(
            run("--port p upload a.wasm --key k.key").cmd,
            Cmd::Upload {
                file: "a.wasm".into(),
                start: false,
                corrupt_chunk: None,
                key: Some("k.key".into()),
                version: None,
            }
        );
        match run("--port p upload a.wasm --key k --version 42").cmd {
            Cmd::Upload { version, .. } => assert_eq!(version, Some(42)),
            other => panic!("{other:?}"),
        }
        // Key commands are local: no port needed.
        assert_eq!(
            p("key new k.key"),
            Ok(Parsed::Key(KeyCmd::New {
                file: "k.key".into()
            }))
        );
        assert_eq!(
            p("key pub k.key"),
            Ok(Parsed::Key(KeyCmd::Pub {
                file: "k.key".into()
            }))
        );
        assert_eq!(
            p("--port p key pub k.key"),
            Ok(Parsed::Key(KeyCmd::Pub {
                file: "k.key".into()
            }))
        );
    }

    #[test]
    fn rejects_bad_trust_and_key_usage() {
        for (args, why) in [
            ("--port p trust claim", "needs --key"),
            ("--port p trust", "needs a subcommand"),
            ("--port p trust frob", "unknown command"),
            ("key", "needs a subcommand"),
            ("key new", "needs a key file"),
            ("key pub", "needs a key file"),
            ("key new a b", "unexpected"),
            ("trust info", "--port"),
            ("--port p upload a.wasm --version 3", "needs --key"),
            (
                "--port p upload a.wasm --key k --version x",
                "not a valid number",
            ),
            (
                "--port p upload a.wasm --key k --version 4294967296",
                "not a valid number",
            ),
            ("--port p info --key k", "only to 'upload' or 'trust claim'"),
            ("--port p trust info --key k", "only to"),
            (
                "--port p trust claim --key k --version 1",
                "only to 'upload'",
            ),
            ("--port p upload a.wasm --key", "needs a key file"),
            ("--port p start --key k", "needs no signature"),
            ("--port p stop --key k --counter 5", "needs no signature"),
            ("--port p --espnow e4:b0:63:81:d3:24 start", "pass --key"),
            ("--port p --espnow e4:b0:63:81:d3:24 stop", "pass --key"),
            (
                "--port p --espnow e4:b0:63:81:d3:24 upload a.wasm",
                "pass --key",
            ),
            (
                "--port p --espnow e4:b0:63:81:d3:24 stop --counter 5",
                "needs --key",
            ),
            ("--port p upload a.wasm --key k --counter 5", "only to"),
            (
                "--port p --espnow e4:b0:63:81:d3:24 stop --key k --counter x",
                "not a valid number",
            ),
        ] {
            match p(args) {
                Err(e) => assert!(e.contains(why), "{args}: {e}"),
                Ok(v) => panic!("{args}: accepted as {v:?}"),
            }
        }
    }

    #[test]
    fn rejects_bad_usage() {
        for (args, why) in [
            ("info", "--port"),
            ("--port p", "no command"),
            ("--port p upload", ".wasm"),
            ("--port p frob", "unknown command"),
            ("--port p info extra", "unexpected"),
            ("--port p --baud x info", "not a valid number"),
            ("--port p --baud 0 info", "positive"),
            ("--port p info --start", "only to 'upload'"),
            ("--port p start --seconds 1", "only to 'monitor'"),
            ("--port p info --verbose", "unknown option"),
            ("--port", "needs a path"),
        ] {
            match p(args) {
                Err(e) => assert!(e.contains(why), "{args}: {e}"),
                Ok(v) => panic!("{args}: accepted as {v:?}"),
            }
        }
    }
}
