//! `mwctl` binary: the imperative shell around the `mwctl` library.
//!
//! Parses arguments, opens the one serial port named by `--port`
//! ([`port::SerialTransport`]), runs the command through
//! [`mwctl::device::Device`] over the serial [`Link`] or, with `--espnow`,
//! over [`AirLink`] through the USB↔ESP-NOW bridge on that port (printing
//! the bridge's MAC first and the request counters and upload rate last),
//! and prints results to stdout, progress and
//! errors to stderr. Over `--espnow`, `start`/`stop` (and `upload --start`)
//! are signed control requests for the target MAC with `--key`
//! ([`mwctl::sign::sign_control`], counter `--counter` or the Unix time in
//! milliseconds). `key new` / `key pub` touch only the key file: `key
//! new` reads 32 bytes from `/dev/urandom` and creates the file with mode
//! 0600, refusing to overwrite one.
//!
//! `fw update` (RP2350, serial only) loads and checks the image
//! ([`mwctl::fw::load_image`]), sends it with progress, and unless
//! `--no-reboot` reboots the device into it on trial; the USB CDC port
//! disappears while the device resets, so it closes the port, reopens the
//! **same** `--port` path (every 500 ms for up to [`RECONNECT_SECONDS`]),
//! waits for the trial, and confirms it (retrying while the self-test is
//! still running), all within the bootrom's trial watchdog window.
//!
//! Exit status: 0 success, 1 device or port failure, 2 usage error.

mod port;

use middleware::loader::trust::{ControlCounter, ControlOp, SignedControl, Version};
use mwctl::air::{AirLink, AirTiming};
use mwctl::bridge::{Mac, Msg, format_mac};
use mwctl::cli::{self, Cmd, KeyCmd, Parsed};
use mwctl::device::{Device, Progress, UploadOptions};
use mwctl::error::Error;
use mwctl::link::{Endpoint, Event, Link, Timeouts};
use middleware::fw_ota::StateCode;
use mwctl::fw::{UpdateOptions as FwOptions, format_version, load_image};
use mwctl::report::{format_board, format_event, format_fw_info, format_info, format_trust};
use mwctl::sign::{SEED_LEN, Seed, counter_at, hex, sign_commit, sign_control, version_at};
use port::SerialTransport;
use std::io::Read as _;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::process::ExitCode;
use std::time::SystemTime;
use std::time::{Duration, Instant};

/// How long `fw update` keeps trying to reopen the port and find the
/// device on trial after the reboot.
const RECONNECT_SECONDS: u64 = 12;
/// Pause between reconnect and confirm attempts.
const RETRY_PAUSE: Duration = Duration::from_millis(500);

/// How long `start` keeps listening after a refused start, for the
/// device's `sys/log` line that explains it (sent after the reply).
const START_FAILURE_LOG_SECONDS: u64 = 2;

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
    let Some(target) = cli.espnow else {
        let timeouts = Timeouts {
            request: cli.timeout,
            ..Timeouts::default()
        };
        if is_fw(&cli.cmd) {
            return fw_command(transport, &cli, timeouts);
        }
        let mut dev = Device::new(Link::new(transport, timeouts));
        let result = command(&mut dev, cli.cmd, None);
        print_events(&dev.take_events());
        return result;
    };
    // A clock-derived first id, so a restarted mwctl does not reuse the ids
    // the device has cached responses for.
    let first_id = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(1, |d| (d.as_millis() & 0xFFFF) as u16);
    let mut link = AirLink::new(transport, target, AirTiming::default(), first_id);
    if let (
        Msg::Hello {
            mac,
            channel,
            version,
        },
        stats,
    ) = link.query()?
    {
        eprintln!(
            "bridge {} (channel {channel}, protocol {version}) -> device {} over ESP-NOW",
            format_mac(&mac),
            format_mac(&target)
        );
        if let Some(s) = stats {
            eprintln!(
                "bridge counters: rx {} (dropped {}), tx ok {} failed {} dropped {}, serial bad {}",
                s.rx, s.rx_dropped, s.tx_ok, s.tx_fail, s.tx_dropped, s.serial_bad
            );
        }
    }
    let mut dev = Device::new(link);
    let result = command(&mut dev, cli.cmd, Some(target));
    print_events(&dev.take_events());
    let s = dev.link().stats();
    eprintln!(
        "air: {} request(s), {} send(s), {} retransmission(s), {} unacknowledged, {} stale frame(s) skipped",
        s.requests, s.sends, s.retransmissions, s.nacks, s.skipped
    );
    result
}

fn is_fw(cmd: &Cmd) -> bool {
    matches!(
        cmd,
        Cmd::FwInfo | Cmd::FwUpdate { .. } | Cmd::FwReboot | Cmd::FwConfirm | Cmd::FwAbort
    )
}

/// `fw ...` over the serial port; `fw update` reconnects after the reboot.
fn fw_command(transport: SerialTransport, cli: &cli::Cli, timeouts: Timeouts) -> Result<(), Error> {
    let mut dev = Device::new(Link::new(transport, timeouts));
    let confirm_attempts = (RECONNECT_SECONDS * 1000 / RETRY_PAUSE.as_millis() as u64) as u32;
    let result = match &cli.cmd {
        Cmd::FwInfo => dev.fw_info().map(|i| print!("{}", format_fw_info(&i))),
        Cmd::FwReboot => dev.fw_reboot().map(|()| println!("rebooting into the committed image on trial")),
        Cmd::FwConfirm => dev
            .fw_confirm(confirm_attempts, || std::thread::sleep(RETRY_PAUSE))
            .map(|()| println!("confirmed: the new firmware is permanent")),
        Cmd::FwAbort => dev.fw_abort().map(|()| println!("aborted")),
        Cmd::FwUpdate { file, key, version, reboot } => {
            let bytes = std::fs::read(file).map_err(|e| io_context(file, e))?;
            let image = load_image(&bytes, *version)?;
            let seed = key.as_deref().map(read_seed).transpose()?;
            println!(
                "firmware {} ({} bytes{}), version {}",
                file.display(),
                image.bytes.len(),
                if image.from_uf2 { ", from UF2" } else { "" },
                format_version(image.version)
            );
            let began = Instant::now();
            let r = dev.fw_update(&image, FwOptions { key: seed.as_ref(), reboot: *reboot }, progress);
            eprintln!();
            let done = r?;
            println!(
                "installed in slot {} in {:.2} s",
                done.slot.name(),
                began.elapsed().as_secs_f64()
            );
            if !done.rebooted {
                println!("not rebooted (--no-reboot): run `fw reboot`, then `fw confirm`");
                print_events(&dev.take_events());
                return Ok(());
            }
            print_events(&dev.take_events());
            drop(dev);
            eprintln!("rebooting into slot {} on trial; reconnecting to {}", done.slot.name(), cli.port);
            let mut dev = reconnect(cli, timeouts)?;
            dev.fw_confirm(confirm_attempts, || std::thread::sleep(RETRY_PAUSE))?;
            println!("confirmed: slot {} is the permanent firmware", done.slot.name());
            print_events(&dev.take_events());
            return Ok(());
        }
        _ => Ok(()),
    };
    print_events(&dev.take_events());
    result
}

/// Reopens the same port until the device answers `sys/fw/info` on trial.
fn reconnect(cli: &cli::Cli, timeouts: Timeouts) -> Result<Device<Link<SerialTransport>>, Error> {
    let deadline = Instant::now() + Duration::from_secs(RECONNECT_SECONDS);
    // The device resets about 100 ms after its reply; let it go first.
    std::thread::sleep(RETRY_PAUSE);
    let mut last = Error::Fw(format!("{} did not come back", cli.port));
    // Bounded by the deadline.
    while Instant::now() < deadline {
        match SerialTransport::open(&cli.port, cli.baud) {
            Ok(t) => {
                let mut dev = Device::new(Link::new(t, timeouts));
                match dev.fw_info() {
                    Ok(i) if i.state == StateCode::Trial => return Ok(dev),
                    Ok(i) => {
                        return Err(Error::Fw(format!(
                            "the device came back {} in slot {} instead of on trial (the bootrom \
                             did not boot the new image)",
                            i.state.name(),
                            i.running.name()
                        )));
                    }
                    Err(e) => last = e,
                }
            }
            Err(e) => last = Error::Io(e),
        }
        std::thread::sleep(RETRY_PAUSE);
    }
    Err(last)
}

/// Signs control request `op` for `target` with `key` (both or neither:
/// the CLI requires `--key` exactly when `--espnow` is given).
fn control_auth(
    op: ControlOp,
    target: Option<Mac>,
    key: Option<&Path>,
    counter: Option<u64>,
) -> Result<Option<SignedControl>, Error> {
    let (Some(target), Some(key)) = (target, key) else {
        return Ok(None);
    };
    let seed = read_seed(key)?;
    let counter = match counter {
        Some(c) => ControlCounter(c),
        None => counter_at(SystemTime::now())?,
    };
    eprintln!(
        "signing {} for {} as counter {} with {}",
        if op == ControlOp::Start {
            "start"
        } else {
            "stop"
        },
        format_mac(&target),
        counter.0,
        key.display()
    );
    Ok(Some(sign_control(&seed, op, &target, counter)))
}

/// Runs one device command over any endpoint; `target` is the device's
/// MAC over ESP-NOW (signed control requests name it).
fn command<L: Endpoint>(dev: &mut Device<L>, cmd: Cmd, target: Option<Mac>) -> Result<(), Error> {
    match cmd {
        Cmd::Info => dev.info().map(|i| print!("{}", format_info(&i))),
        Cmd::Board => dev.board().map(|b| print!("{}", format_board(&b.info()))),
        Cmd::Start { key, counter } => {
            match control_auth(ControlOp::Start, target, key.as_deref(), counter)
                .and_then(|auth| dev.start_signed(auth))
            {
                Ok(()) => {
                    println!("payload started");
                    Ok(())
                }
                Err(e) => {
                    // The device logs why a start failed right after its reply;
                    // show those lines before reporting the error.
                    let _ = monitor(dev, Some(START_FAILURE_LOG_SECONDS));
                    Err(e)
                }
            }
        }
        Cmd::Stop { key, counter } => {
            control_auth(ControlOp::Stop, target, key.as_deref(), counter)
                .and_then(|auth| dev.stop_signed(auth))
                .map(|()| println!("payload stopped"))
        }
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
            let start_auth = match start {
                true => control_auth(ControlOp::Start, target, key.as_deref(), None)?,
                false => None,
            };
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
            let began = Instant::now();
            let r = dev.upload(
                &image,
                UploadOptions {
                    start,
                    corrupt_chunk,
                    auth,
                    start_auth,
                },
                progress,
            );
            eprintln!();
            let secs = began.elapsed().as_secs_f64();
            r.map(|()| {
                println!(
                    "installed {} ({} bytes){} in {secs:.2} s ({:.0} bytes/s)",
                    file.display(),
                    image.len(),
                    if start { " and started" } else { "" },
                    image.len() as f64 / secs.max(1e-6)
                );
            })
        }
        Cmd::Monitor { seconds } => monitor(dev, seconds),
        // Serial only, handled by `fw_command` (the CLI refuses --espnow).
        Cmd::Reset | Cmd::FwInfo | Cmd::FwUpdate { .. } | Cmd::FwReboot | Cmd::FwConfirm | Cmd::FwAbort => {
            Ok(())
        }
    }
}

fn monitor<L: Endpoint>(dev: &mut Device<L>, seconds: Option<u64>) -> Result<(), Error> {
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
