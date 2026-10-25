//! Command-line parsing (pure).
//!
//! [`parse`] turns the arguments after the program name into a [`Cli`] or
//! a [`Parsed::Help`] request. Options may appear before or after the
//! command; command-specific options are rejected on other commands.
//! [`HELP`] is the usage text printed by `--help` and on usage errors.

use std::path::PathBuf;
use std::time::Duration;

/// Usage text.
pub const HELP: &str = "\
mwctl - talk to the middleware ESP base firmware over one serial port

USAGE:
    mwctl --port <PATH> [--baud <N>] [--timeout <MS>] <COMMAND>

COMMANDS:
    info                       Print chip, firmware, ABI, capacity and payload status
    upload <FILE.wasm>         Install a payload (sys/loader begin/chunk/commit)
        --start                    then send sys/payload/start
        --corrupt-chunk <INDEX>    test hook: flip a byte in that chunk's data but
                                   declare the original CRC, so commit must fail
    start                      sys/payload/start
    stop                       sys/payload/stop
    monitor [--seconds <N>]    Print sys/log and sys/payload/status publications
                               (and other publications) until N seconds pass or Ctrl-C
    reset                      Reboot into run mode: RTS asserted for 100 ms with DTR
                               deasserted (EN pulse through the DTR/RTS auto-reset circuit)

OPTIONS:
    --port <PATH>      Serial port to open, e.g. /dev/ttyUSB0 (required; never scanned)
    --baud <N>         Baud rate [default: 115200]
    --timeout <MS>     Per-attempt response timeout [default: 1000]; each request is
                       retried once on timeout, sys/loader/begin waits at least 5000 ms
    -h, --help         Print this help

The port is opened with DTR and RTS deasserted so opening it neither resets
the board nor selects the ROM bootloader. Exit status is 0 on success, 1 when
the device or port fails, 2 on a usage error.
";

/// Default baud rate of the base firmware's console UART.
pub const DEFAULT_BAUD: u32 = 115_200;
/// Default per-attempt timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(1000);

/// A subcommand.
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
    },
    /// `start`.
    Start,
    /// `stop`.
    Stop,
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
    /// What to do.
    pub cmd: Cmd,
}

/// Result of a successful parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    /// Run a command.
    Run(Cli),
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
    let mut baud = DEFAULT_BAUD;
    let mut timeout = DEFAULT_TIMEOUT;
    let mut positional: Vec<String> = Vec::new();
    let mut start = false;
    let mut corrupt_chunk = None;
    let mut seconds = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "--port" => port = Some(args.next().ok_or("--port needs a path")?),
            "--baud" => baud = number("--baud", args.next())?,
            "--timeout" => timeout = Duration::from_millis(number("--timeout", args.next())?),
            "--start" => start = true,
            "--corrupt-chunk" => corrupt_chunk = Some(number("--corrupt-chunk", args.next())?),
            "--seconds" => seconds = Some(number("--seconds", args.next())?),
            s if s.starts_with('-') => return Err(format!("unknown option '{s}'")),
            _ => positional.push(a),
        }
    }
    if baud == 0 {
        return Err("--baud must be positive".into());
    }
    if timeout.is_zero() {
        return Err("--timeout must be positive".into());
    }
    let mut pos = positional.into_iter();
    let name = pos.next().ok_or("no command given")?;
    let only = |flag: &str, set: bool, cmd: &str| {
        if set && name != cmd {
            Err(format!("{flag} applies only to '{cmd}'"))
        } else {
            Ok(())
        }
    };
    only("--start", start, "upload")?;
    only("--corrupt-chunk", corrupt_chunk.is_some(), "upload")?;
    only("--seconds", seconds.is_some(), "monitor")?;
    let cmd = match name.as_str() {
        "info" => Cmd::Info,
        "upload" => Cmd::Upload {
            file: PathBuf::from(pos.next().ok_or("upload needs a .wasm file")?),
            start,
            corrupt_chunk,
        },
        "start" => Cmd::Start,
        "stop" => Cmd::Stop,
        "monitor" => Cmd::Monitor { seconds },
        "reset" => Cmd::Reset,
        other => return Err(format!("unknown command '{other}'")),
    };
    if let Some(extra) = pos.next() {
        return Err(format!("unexpected argument '{extra}'"));
    }
    let port = port.ok_or("--port <PATH> is required (mwctl never scans for ports)")?;
    Ok(Parsed::Run(Cli {
        port,
        baud,
        timeout,
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
    fn parses_every_command() {
        let c = run("--port /dev/ttyUSB0 info");
        assert_eq!(
            c,
            Cli {
                port: "/dev/ttyUSB0".into(),
                baud: 115_200,
                timeout: DEFAULT_TIMEOUT,
                cmd: Cmd::Info
            }
        );
        assert_eq!(
            run("upload a.wasm --start --port p --baud 921600 --corrupt-chunk 3"),
            Cli {
                port: "p".into(),
                baud: 921_600,
                timeout: DEFAULT_TIMEOUT,
                cmd: Cmd::Upload {
                    file: "a.wasm".into(),
                    start: true,
                    corrupt_chunk: Some(3)
                }
            }
        );
        assert_eq!(run("--port p start").cmd, Cmd::Start);
        assert_eq!(run("--port p stop").cmd, Cmd::Stop);
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
