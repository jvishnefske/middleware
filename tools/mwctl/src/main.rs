//! `mwctl` binary: the imperative shell around the `mwctl` library.
//!
//! Parses arguments, opens the one serial port named by `--port`
//! ([`port::SerialTransport`]), runs the command through
//! [`mwctl::device::Device`], and prints results to stdout, progress and
//! errors to stderr. Exit status: 0 success, 1 device or port failure, 2
//! usage error.

mod port;

use mwctl::cli::{self, Cmd, Parsed};
use mwctl::device::{Device, Progress, UploadOptions};
use mwctl::error::Error;
use mwctl::link::{Event, Link, Timeouts};
use mwctl::report::{format_event, format_info};
use port::SerialTransport;
use std::io::Write as _;
use std::process::ExitCode;
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
        Cmd::Upload {
            file,
            start,
            corrupt_chunk,
        } => {
            let image = std::fs::read(&file).map_err(|e| {
                Error::Io(std::io::Error::new(
                    e.kind(),
                    format!("{}: {e}", file.display()),
                ))
            })?;
            if let Some(i) = corrupt_chunk {
                eprintln!("test hook: corrupting chunk {i}; the device must reject the commit");
            }
            println!("uploading {} ({} bytes)", file.display(), image.len());
            let r = dev.upload(
                &image,
                UploadOptions {
                    start,
                    corrupt_chunk,
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
        Ok(Parsed::Run(c)) => match run(c) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("mwctl: {e}");
                ExitCode::from(1)
            }
        },
    }
}
