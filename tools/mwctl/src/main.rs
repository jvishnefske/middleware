//! `mwctl` binary: the imperative shell around the `mwctl` library.
//!
//! Parses arguments, opens the one serial port named by `--port`
//! ([`port::SerialTransport`]), runs the command through
//! [`mwctl::device::Device`], and prints results to stdout, progress and
//! errors to stderr. `key new` / `key pub` touch only the key file: `key
//! new` reads 32 bytes from `/dev/urandom` and creates the file with mode
//! 0600, refusing to overwrite one. Exit status: 0 success, 1 device or port failure, 2
//! usage error.

mod port;

use middleware::loader::trust::Version;
use mwctl::cli::{self, Cmd, KeyCmd, Parsed};
use mwctl::device::{Device, Progress, UploadOptions};
use mwctl::error::Error;
use mwctl::link::{Event, Link, Timeouts};
use mwctl::report::{format_event, format_info, format_trust};
use mwctl::sign::{SEED_LEN, Seed, hex, sign_commit, version_at};
use port::SerialTransport;
use std::io::Read as _;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::process::ExitCode;
use std::time::SystemTime;
use std::time::{Duration, Instant};

fn print_events(events: &[Event]) {
    for e in events {
        println!("{}", format_event(e));
    }
}

fn progress(p: Progress) {
    print_events(&p.events);
    let pct = if p.total == 0 {
        100
    } else {
        u64::from(p.done) * 100 / u64::from(p.total)
    };
    eprint!(
        "\r  step {}/{}: {}/{} bytes ({pct}%)",
        p.step + 1,
        p.steps,
        p.done,
        p.total
    );
    let _ = std::io::stderr().flush();
}

fn io_context(path: &Path, e: std::io::Error) -> Error {
    Error::File(std::io::Error::new(
        e.kind(),
        format!("{}: {e}", path.display()),
    ))
}

fn read_seed(path: &Path) -> Result<Seed, Error> {
    let bytes = std::fs::read(path).map_err(|e| io_context(path, e))?;
    Ok(Seed::from_file_bytes(&bytes)?)
}

/// `key new` / `key pub`.
fn key(cmd: KeyCmd) -> Result<(), Error> {
    match cmd {
        KeyCmd::New { file } => {
            let mut seed = [0u8; SEED_LEN];
            std::fs::File::open("/dev/urandom")
                .and_then(|mut r| r.read_exact(&mut seed))
                .map_err(|e| io_context(Path::new("/dev/urandom"), e))?;
            let seed = Seed::new(seed);
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&file)
                .map_err(|e| io_context(&file, e))?;
            f.write_all(seed.bytes())
                .and_then(|()| f.sync_all())
                .map_err(|e| io_context(&file, e))?;
            eprintln!(
                "wrote signing key {} (mode 0600); keep it secret",
                file.display()
            );
            println!("{}", hex(&seed.public_key().0));
        }
        KeyCmd::Pub { file } => println!("{}", hex(&read_seed(&file)?.public_key().0)),
    }
    Ok(())
}

fn run(cli: cli::Cli) -> Result<(), Error> {
    let mut transport = SerialTransport::open(&cli.port, cli.baud)?;
    if cli.cmd == Cmd::Reset {
        transport.reset()?;
        println!(
            "reset {}: RTS pulsed {} ms with DTR deasserted",
            cli.port,
            port::RESET_PULSE.as_millis()
        );
        return Ok(());
    }
    let timeouts = Timeouts {
        request: cli.timeout,
        ..Timeouts::default()
    };
    let mut dev = Device::new(Link::new(transport, timeouts));
    let result = match cli.cmd {
        Cmd::Info => dev.info().map(|i| print!("{}", format_info(&i))),
        Cmd::Start => dev.start().map(|()| println!("payload started")),
        Cmd::Stop => dev.stop().map(|()| println!("payload stopped")),
        Cmd::TrustInfo => dev.trust_info().map(|t| print!("{}", format_trust(&t))),
        Cmd::TrustClaim { key } => {
            let public = read_seed(&key)?.public_key();
            dev.trust_claim(public)
                .map(|()| println!("claimed: pinned key {}", hex(&public.0)))
        }
        Cmd::Upload {
            file,
            start,
            corrupt_chunk,
            key,
            version,
        } => {
            let image = std::fs::read(&file).map_err(|e| io_context(&file, e))?;
            if let Some(i) = corrupt_chunk {
                eprintln!("test hook: corrupting chunk {i}; the device must reject the commit");
            }
            let auth = match key {
                None => None,
                Some(k) => {
                    let seed = read_seed(&k)?;
                    let v = match version {
                        Some(v) => Version(v),
                        None => version_at(SystemTime::now())?,
                    };
                    let auth = sign_commit(&image, &seed, v)
                        .ok_or(Error::Plan(mwctl::plan::PlanError::TooLarge(image.len())))?;
                    eprintln!("signing as version {} with {}", v.0, k.display());
                    Some(auth)
                }
            };
            println!("uploading {} ({} bytes)", file.display(), image.len());
            let r = dev.upload(
                &image,
                UploadOptions {
                    start,
                    corrupt_chunk,
                    auth,
                },
                progress,
            );
            eprintln!();
            r.map(|()| {
                println!(
                    "installed {} ({} bytes){}",
                    file.display(),
                    image.len(),
                    if start { " and started" } else { "" }
                );
            })
        }
        Cmd::Monitor { seconds } => monitor(&mut dev, seconds),
        Cmd::Reset => Ok(()),
    };
    print_events(&dev.take_events());
    result
}

fn monitor(dev: &mut Device<SerialTransport>, seconds: Option<u64>) -> Result<(), Error> {
    let end = seconds.map(|s| Instant::now() + Duration::from_secs(s));
    match seconds {
        Some(s) => eprintln!("monitoring for {s} s"),
        None => eprintln!("monitoring until Ctrl-C"),
    }
    // Bounded by `end` when given; otherwise runs until interrupted.
    loop {
        let slice = Instant::now() + Duration::from_millis(500);
        let deadline = end.map_or(slice, |e| e.min(slice));
        if let Some(e) = dev.next_event(deadline)? {
            println!("{}", format_event(&e));
            let _ = std::io::stdout().flush();
        }
        if end.is_some_and(|e| Instant::now() >= e) {
            return Ok(());
        }
    }
}

fn main() -> ExitCode {
    let parsed = cli::parse(std::env::args().skip(1));
    match parsed {
        Ok(Parsed::Help) => {
            print!("{}", cli::HELP);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("mwctl: {e}\n\n{}", cli::HELP);
            ExitCode::from(2)
        }
        Ok(Parsed::Key(k)) => match key(k) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("mwctl: {e}");
                ExitCode::from(1)
            }
        },
        Ok(Parsed::Run(c)) => match run(c) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("mwctl: {e}");
                ExitCode::from(1)
            }
        },
    }
}
