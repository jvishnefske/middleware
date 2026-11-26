//! `mwctl` binary: the imperative shell around the `mwctl` library.
//!
//! Parses arguments, opens the one serial port named by `--port`
//! ([`mwctl::port::SerialTransport`]) or the UDP socket named by `--udp`
//! ([`UdpLink`]), runs the command through
//! [`mwctl::device::Device`] over the serial [`Link`] or, with `--espnow`,
//! over [`AirLink`] through the USB↔ESP-NOW bridge on that port (printing
//! the bridge's MAC first and the request counters and upload rate last),
//! and prints results to stdout, progress and
//! errors to stderr. Over `--espnow`, `start`/`stop` (and `upload --start`)
//! are signed control requests for the target MAC with `--key`
//! ([`mwctl::sign::sign_control`], counter `--counter` or the Unix time in
//! milliseconds). `key new` / `key pub` touch only the key file: `key
//! new` reads 32 bytes from `/dev/urandom` and creates the file with mode
//! 0600, refusing to overwrite one. Every key option also takes
//! `--pkcs11 <LABEL>`, a key in a PKCS#11 token ([`mwctl::pkcs11`]); the
//! token is opened (and its PIN read from `PKCS11_PIN` or `~/.env`) once
//! per run, only when a signature or the public key is needed.
//!
//! Over Zenoh (`--zenoh`, feature `zenoh`) every command goes to one board
//! through [`mwctl::zlink::ZenohLink`]; `fw reboot|confirm|abort` and the
//! reboot and confirm of `fw update` are signed control requests there.
//!
//! `fw update` over a serial port loads and checks the image
//! ([`mwctl::fw::load_image`]), sends it with progress, and unless
//! `--no-reboot` reboots the device into it on trial; the USB CDC port
//! disappears while the device resets, so it closes the port, reopens the
//! **same** `--port` path (every 500 ms for up to [`RECONNECT_SECONDS`]),
//! waits for the trial, and confirms it (retrying while the self-test is
//! still running), all within the bootrom's trial watchdog window.
//!
//! Exit status: 0 success, 1 device or port failure, 2 usage error.

use middleware::fw_ota::StateCode;
use middleware::loader::trust::KeyKind;
use middleware::loader::trust::{ControlCounter, ControlOp, SignedControl, Version};
use mwctl::air::{AirLink, AirTiming};
use mwctl::bridge::{Mac, Msg, format_mac};
use mwctl::cli::{self, Cmd, KeyCmd, KeySpec, Parsed};
use mwctl::device::{Device, Progress, UploadOptions};
use mwctl::error::Error;
use mwctl::fw::{UpdateOptions as FwOptions, format_version, load_image};
use mwctl::link::{Endpoint, Event, Link, Timeouts};
use mwctl::pkcs11::{self, Location, Pkcs11Signer};
use mwctl::port::SerialTransport;
use mwctl::reflash;
use mwctl::report::{
    format_binding, format_board, format_event, format_fw_info, format_info, format_manifest,
    format_net, format_trust,
};
use mwctl::sign::{SEED_LEN, Seed, Signer, counter_at, hex, sign_commit, sign_control, version_at};
use mwctl::udp::UdpLink;
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
        // Sample streams stay out of the default output (`monitor --samples`
        // shows them decoded).
        if mwctl::report::is_sample_stream(e) {
            continue;
        }
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

/// Opens the signing key `spec` names: a seed file, or a token key
/// (logging in with `PKCS11_PIN`).
fn open_signer(spec: &KeySpec) -> Result<Box<dyn Signer>, Error> {
    match spec {
        KeySpec::File(path) => Ok(Box::new(read_seed(path)?)),
        KeySpec::Pkcs11(label) => {
            let at = Location::from_settings();
            let pin = pkcs11::pin(pkcs11::ENV_PIN)?;
            let signer = Pkcs11Signer::open(&at, label, &pin)?;
            eprintln!("signing with {signer:?} via {}", at.module.display());
            Ok(Box::new(signer))
        }
    }
}

/// `key new` / `key pub` / `key init-token`.
fn key(cmd: KeyCmd) -> Result<(), Error> {
    match cmd {
        KeyCmd::New {
            key: KeySpec::Pkcs11(label),
        } => {
            let at = Location::from_settings();
            let pin = pkcs11::pin(pkcs11::ENV_PIN)?;
            let public = pkcs11::generate(&at, &label, KeyKind::P256, &pin)?;
            eprintln!(
                "generated P-256 key '{label}' in the token of {} (not extractable)",
                at.module.display()
            );
            println!("{}", hex(public.as_bytes()));
        }
        KeyCmd::Pub {
            key: KeySpec::Pkcs11(label),
        } => {
            let signer = open_signer(&KeySpec::Pkcs11(label))?;
            println!("{}", hex(signer.public_key().as_bytes()));
        }
        KeyCmd::InitToken { token } => {
            let at = Location::from_settings();
            let so = pkcs11::pin(pkcs11::ENV_SO_PIN)?;
            let user = pkcs11::pin(pkcs11::ENV_PIN)?;
            pkcs11::init_token(&at.module, &token, &so, &user)?;
            eprintln!("initialised token '{token}' of {}", at.module.display());
        }
        KeyCmd::New {
            key: KeySpec::File(file),
        } => {
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
            println!("{}", hex(seed.public_key().as_bytes()));
        }
        KeyCmd::Pub {
            key: KeySpec::File(file),
        } => println!("{}", hex(read_seed(&file)?.public_key().as_bytes())),
    }
    Ok(())
}

fn run(cli: cli::Cli) -> Result<(), Error> {
    if let Some(z) = &cli.zenoh {
        return run_zenoh(&cli, z);
    }
    if let Cmd::Reflash { file, version } = &cli.cmd {
        return reflash_command(&cli.port, cli.baud, file, *version);
    }
    let timeouts = Timeouts {
        request: cli.timeout,
        ..Timeouts::default()
    };
    if let Some(addr) = &cli.udp {
        use std::net::ToSocketAddrs;
        let ctx = |e: std::io::Error| std::io::Error::new(e.kind(), format!("{addr}: {e}"));
        let peer = addr
            .to_socket_addrs()
            .map_err(ctx)?
            .next()
            .ok_or_else(|| ctx(std::io::ErrorKind::NotFound.into()))?;
        let local = if peer.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        let socket = std::net::UdpSocket::bind(local)?;
        socket.connect(peer).map_err(ctx)?;
        let mut dev = Device::new(UdpLink::new(socket, timeouts));
        // Over UDP, the Colorlight 5A-75B (chip `ecp5`) takes the module
        // compiled on the host (design.md CL6): a module it cannot run is
        // refused here, with the reason, before anything is sent. A file
        // that is not a module is uploaded as it is (an image already).
        let mut compiled = None;
        if let Cmd::Upload { file, .. } = &cli.cmd {
            let wasm = std::fs::read(file).map_err(|e| io_context(file, e))?;
            if mwctl::ecp5::is_wasm(&wasm) && dev.info()?.chip == mwctl::ecp5::CHIP {
                let image = mwctl::ecp5::image_for_ecp5(&wasm)
                    .map_err(|why| Error::Payload(format!("{}: {why}", file.display())))?;
                eprintln!(
                    "compiled {} for the 5A-75B: {} byte image",
                    file.display(),
                    image.len()
                );
                compiled = Some(image);
            }
        }
        let result = command_with(&mut dev, cli.cmd, None, compiled);
        print_events(&dev.take_events());
        return result;
    }
    let mut transport = SerialTransport::open(&cli.port, cli.baud)?;
    if cli.cmd == Cmd::Reset {
        transport.reset()?;
        println!(
            "reset {}: RTS pulsed {} ms with DTR deasserted",
            cli.port,
            mwctl::port::RESET_PULSE.as_millis()
        );
        return Ok(());
    }
    let Some(target) = cli.espnow else {
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

/// `reflash`: the application image through the nRF52 bootloader's MWFW
/// protocol on the port. The header is checked before the port is opened.
/// The port is opened without a reset: the board must already be in reflash
/// mode.
fn reflash_command(port: &str, baud: u32, file: &Path, version: u32) -> Result<(), Error> {
    let image = std::fs::read(file).map_err(|e| io_context(file, e))?;
    reflash::header(&image, version).map_err(Error::Fw)?;
    let mut transport = SerialTransport::open(port, baud)?;
    let len = image.len();
    reflash::reflash(&mut transport, &image, version, |sent| {
        eprint!("\rreflash: {sent}/{len} bytes");
        let _ = std::io::stderr().flush();
    })
    .map_err(Error::Fw)?;
    eprintln!();
    println!("reflashed {len} bytes, verified");
    Ok(())
}

fn is_fw(cmd: &Cmd) -> bool {
    matches!(
        cmd,
        Cmd::FwInfo
            | Cmd::FwUpdate { .. }
            | Cmd::FwReboot { .. }
            | Cmd::FwConfirm { .. }
            | Cmd::FwAbort { .. }
    )
}

/// `fw ...` over the serial port; `fw update` reconnects after the reboot.
fn fw_command(transport: SerialTransport, cli: &cli::Cli, timeouts: Timeouts) -> Result<(), Error> {
    let mut dev = Device::new(Link::new(transport, timeouts));
    let confirm_attempts = (RECONNECT_SECONDS * 1000 / RETRY_PAUSE.as_millis() as u64) as u32;
    let result = match &cli.cmd {
        Cmd::FwInfo => dev.fw_info().map(|i| print!("{}", format_fw_info(&i))),
        Cmd::FwReboot { .. } => dev
            .fw_reboot()
            .map(|()| println!("rebooting into the committed image on trial")),
        Cmd::FwConfirm { .. } => dev
            .fw_confirm(confirm_attempts, || std::thread::sleep(RETRY_PAUSE))
            .map(|()| println!("confirmed: the new firmware is permanent")),
        Cmd::FwAbort { .. } => dev.fw_abort().map(|()| println!("aborted")),
        Cmd::FwUpdate {
            file,
            key,
            version,
            reboot,
        } => {
            let bytes = std::fs::read(file).map_err(|e| io_context(file, e))?;
            let image = load_image(&bytes, *version)?;
            let signer = key.as_ref().map(open_signer).transpose()?;
            println!(
                "firmware {} ({} bytes{}), version {}",
                file.display(),
                image.bytes.len(),
                if image.from_uf2 { ", from UF2" } else { "" },
                format_version(image.version)
            );
            let began = Instant::now();
            let r = dev.fw_update(
                &image,
                FwOptions {
                    release: None,
                    key: signer.as_deref(),
                    reboot: *reboot,
                    control: None,
                },
                progress,
            );
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
            eprintln!(
                "rebooting into slot {} on trial; reconnecting to {}",
                done.slot.name(),
                cli.port
            );
            let mut dev = reconnect(cli, timeouts)?;
            dev.fw_confirm(confirm_attempts, || std::thread::sleep(RETRY_PAUSE))?;
            println!(
                "confirmed: slot {} is the permanent firmware",
                done.slot.name()
            );
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
    key: Option<&KeySpec>,
    counter: Option<u64>,
) -> Result<Option<SignedControl>, Error> {
    let (Some(target), Some(key)) = (target, key) else {
        return Ok(None);
    };
    let signer = open_signer(key)?;
    let counter = match counter {
        Some(c) => ControlCounter(c),
        None => counter_at(SystemTime::now())?,
    };
    eprintln!(
        "signing {} for {} as counter {} with {}",
        op_name(op),
        format_mac(&target),
        counter.0,
        key
    );
    Ok(Some(sign_control(&*signer, op, &target, counter)?))
}

fn op_name(op: ControlOp) -> &'static str {
    match op {
        ControlOp::Start => "start",
        ControlOp::Stop => "stop",
        ControlOp::FwReboot => "fw reboot",
        ControlOp::FwConfirm => "fw confirm",
        ControlOp::FwAbort => "fw abort",
        ControlOp::OsReboot => "os reboot",
        ControlOp::OsConfirm => "os confirm",
        ControlOp::OsAbort => "os abort",
    }
}

/// How long `--zenoh` waits for the board to answer at first (it may be
/// reconnecting to a host that just started listening).
#[cfg(feature = "zenoh")]
const ZENOH_WAIT_SECONDS: u64 = 30;
/// How long a firmware update over Zenoh waits for the board to come back
/// on trial (reset, boot, DHCP, reconnect) before confirming.
#[cfg(feature = "zenoh")]
const ZENOH_TRIAL_SECONDS: u64 = 90;

#[cfg(not(feature = "zenoh"))]
fn run_zenoh(_cli: &cli::Cli, _z: &cli::ZenohTarget) -> Result<(), Error> {
    Err(Error::Io(std::io::Error::other(
        "this mwctl was built without Zenoh: cargo build -p mwctl --features zenoh",
    )))
}

/// A command to one board over Zenoh ([`mwctl::zlink`]).
#[cfg(feature = "zenoh")]
fn run_zenoh(cli: &cli::Cli, z: &cli::ZenohTarget) -> Result<(), Error> {
    use middleware::transport::SenderId;
    use mwctl::fw::ControlSigner;
    use mwctl::zlink::{Route, ZenohLink};
    let node = SenderId(z.node);
    let route = match &z.route {
        cli::ZenohRoute::Connect(e) => Route::Connect(e.clone()),
        cli::ZenohRoute::Listen(e) => Route::Listen(e.clone()),
    };
    let target = z.target.unwrap_or_else(|| mwctl::zlink::target(node));
    eprintln!(
        "zenoh: {} {}, board {}, control target {}",
        match &route {
            Route::Connect(_) => "connecting to",
            Route::Listen(_) => "listening on",
        },
        match &route {
            Route::Connect(e) | Route::Listen(e) => e,
        },
        hex(&z.node),
        format_mac(&target)
    );
    let tls = match &z.tls_dir {
        Some(dir) => Some(mwctl::zlink::tls_from_dir(dir)?),
        None => None,
    };
    let mut link = ZenohLink::open(&route, node, tls)?;
    link.wait_for_board(Duration::from_secs(ZENOH_WAIT_SECONDS))?;
    let mut dev = Device::new(link);
    let key = match &cli.cmd {
        Cmd::FwUpdate { key, .. }
        | Cmd::FwReboot { key }
        | Cmd::FwConfirm { key }
        | Cmd::FwAbort { key }
        | Cmd::OsReboot { key }
        | Cmd::OsConfirm { key }
        | Cmd::OsAbort { key } => key.clone(),
        _ => None,
    };
    let is_os_control = matches!(
        cli.cmd,
        Cmd::OsReboot { .. } | Cmd::OsConfirm { .. } | Cmd::OsAbort { .. }
    );
    let Some(spec) = key.filter(|_| is_fw(&cli.cmd) || is_os_control) else {
        if is_fw(&cli.cmd) {
            // fw info.
            return dev.fw_info().map(|i| print!("{}", format_fw_info(&i)));
        }
        let r = command(&mut dev, cli.cmd.clone(), Some(target));
        print_events(&dev.take_events());
        return r;
    };
    let signer = open_signer(&spec)?;
    // Strictly increasing counters, also within one millisecond.
    let last = std::cell::Cell::new(0u64);
    let sign = |op: ControlOp| -> Result<SignedControl, Error> {
        let now = counter_at(SystemTime::now())?.0;
        let c = ControlCounter(now.max(last.get() + 1));
        last.set(c.0);
        eprintln!(
            "signing {} for {} as counter {}",
            op_name(op),
            format_mac(&target),
            c.0
        );
        Ok(sign_control(&*signer, op, &target, c)?)
    };
    let control = Some(ControlSigner(&sign));
    let attempts = (ZENOH_TRIAL_SECONDS * 1000 / RETRY_PAUSE.as_millis() as u64) as u32;
    match &cli.cmd {
        Cmd::FwReboot { .. } => dev
            .fw_reboot_signed(control)
            .map(|()| println!("rebooting into the committed image on trial")),
        Cmd::FwConfirm { .. } => dev
            .fw_confirm_signed(attempts, || std::thread::sleep(RETRY_PAUSE), control)
            .map(|()| println!("confirmed: the new firmware is permanent")),
        Cmd::FwAbort { .. } => dev.fw_abort_signed(control).map(|()| println!("aborted")),
        Cmd::OsReboot { .. } => dev
            .os_reboot(control)
            .map(|()| println!("rebooting into the committed root on trial")),
        Cmd::OsConfirm { .. } => dev
            .os_confirm(attempts, || std::thread::sleep(RETRY_PAUSE), control)
            .map(|()| println!("confirmed: the root is permanent")),
        Cmd::OsAbort { .. } => dev.os_abort(control).map(|()| println!("aborted")),
        Cmd::FwUpdate {
            file,
            version,
            reboot,
            ..
        } => {
            let bytes = std::fs::read(file).map_err(|e| io_context(file, e))?;
            let image = load_image(&bytes, *version)?;
            println!(
                "firmware {} ({} bytes), version {}, over Zenoh",
                file.display(),
                image.bytes.len(),
                format_version(image.version)
            );
            let began = Instant::now();
            let done = dev.fw_update(
                &image,
                FwOptions {
                    release: None,
                    key: Some(&*signer),
                    reboot: *reboot,
                    control,
                },
                progress,
            );
            eprintln!();
            let done = done?;
            println!(
                "installed in slot {} in {:.2} s",
                done.slot.name(),
                began.elapsed().as_secs_f64()
            );
            if !done.rebooted {
                println!("not rebooted (--no-reboot): run `fw reboot`, then `fw confirm`");
                return Ok(());
            }
            eprintln!(
                "rebooting into slot {} on trial; waiting for the board",
                done.slot.name()
            );
            // The board drops its session, resets, boots the trial and
            // reconnects: poll until it answers on trial.
            let end = Instant::now() + Duration::from_secs(ZENOH_TRIAL_SECONDS);
            loop {
                match dev.fw_info() {
                    Ok(i) if i.state == StateCode::Trial => break,
                    Ok(i) if !matches!(i.state, StateCode::Committed) => {
                        return Err(Error::Fw(format!(
                            "the board came back {} in slot {} instead of on trial",
                            i.state.name(),
                            i.running.name()
                        )));
                    }
                    _ if Instant::now() >= end => {
                        return Err(Error::Fw("the board did not come back on trial".into()));
                    }
                    _ => std::thread::sleep(RETRY_PAUSE),
                }
            }
            dev.fw_confirm_signed(attempts, || std::thread::sleep(RETRY_PAUSE), control)?;
            println!(
                "confirmed: slot {} is the permanent firmware",
                done.slot.name()
            );
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Runs one device command over any endpoint; `target` is the device's
/// MAC over ESP-NOW (signed control requests name it).
/// `os confirm` waits this long for the trial to become the running slot.
const OS_CONFIRM_ATTEMPTS: u32 = 120;

fn command<L: Endpoint>(dev: &mut Device<L>, cmd: Cmd, target: Option<Mac>) -> Result<(), Error> {
    command_with(dev, cmd, target, None)
}

/// [`command`], with `upload` sending `image` (the 5A-75B's compiled
/// image) in place of the file's bytes when it is given.
fn command_with<L: Endpoint>(
    dev: &mut Device<L>,
    cmd: Cmd,
    target: Option<Mac>,
    image: Option<Vec<u8>>,
) -> Result<(), Error> {
    match cmd {
        Cmd::Info => dev.info().map(|i| print!("{}", format_info(&i))),
        Cmd::Board => dev.board().map(|b| print!("{}", format_board(&b.info()))),
        Cmd::Manifest { of: None } => dev.manifest().map(|m| print!("{}", format_manifest(&m))),
        Cmd::Manifest { of: Some(id) } => dev
            .peer_manifest(middleware::SenderId(id))
            .map(|m| print!("{}", format_manifest(&m))),
        Cmd::Peers => dev.peers().map(|p| {
            if p.is_empty() {
                println!("no peers heard");
            }
            for id in p {
                println!(
                    "{}",
                    id.0.iter().map(|b| format!("{b:02x}")).collect::<String>()
                );
            }
        }),
        Cmd::Binding => dev.binding().map(|r| match r {
            None => println!("no binding stored"),
            Some(r) => print!(
                "{}",
                format_binding(&r).unwrap_or_else(|| "a binding this mwctl cannot decode\n".into())
            ),
        }),
        Cmd::Bind {
            file,
            specs,
            key,
            version,
        } => {
            use middleware::loader::binding::{SignedBinding, binding_message};
            let image = std::fs::read(&file).map_err(|e| io_context(&file, e))?;
            let v = match version {
                Some(v) => Version(v),
                None => version_at(SystemTime::now())?,
            };
            let record = mwctl::bindspec::record(mwctl::sign::image_sha256(&image), v.0, &specs)
                .map_err(Error::Payload)?;
            let mut body = record.clone();
            if let Some(k) = key {
                let signer = open_signer(&k)?;
                let sha = mwctl::sign::image_sha256(&record);
                let node = dev.info()?.node.ok_or_else(|| {
                    Error::Payload("the device reports no node id to sign the binding for".into())
                })?;
                let message = binding_message(node, &mwctl::sign::image_sha256(&image), v, &sha);
                let signature = signer.sign(&message)?;
                body.extend_from_slice(
                    &SignedBinding {
                        version: v,
                        signature,
                    }
                    .encode(),
                );
                eprintln!("signing binding version {} with {k}", v.0);
            }
            print!("{}", format_binding(&record).unwrap_or_default());
            dev.bind(&body).map(|()| {
                println!(
                    "bound {}: applies at the payload's next start",
                    file.display()
                )
            })
        }
        Cmd::OsInfo => dev
            .os_info()
            .map(|i| print!("{}", mwctl::os::format_os_info(&i))),
        Cmd::OsUpdate {
            file,
            cert,
            version,
        } => {
            let image = std::fs::read(&file).map_err(Error::File)?;
            let cert_bytes = match &cert {
                Some(p) => Some(std::fs::read(p).map_err(Error::File)?),
                None => None,
            };
            let signed = match &cert_bytes {
                Some(b) => Some(
                    middleware::loader::release::SignedCert::decode(b)
                        .ok_or_else(|| Error::Fw(format!("{}: not a [cert][sig] release", cert.as_ref().map_or(String::new(), |p| p.display().to_string()))))?,
                ),
                None => None,
            };
            let began = Instant::now();
            let slot = dev.os_update(&image, Version(version), signed, |p| {
                eprint!("\r  step {}/{}  {} of {} bytes", p.step + 1, p.steps, p.done, p.total);
            })?;
            eprintln!();
            println!(
                "root written to slot {slot} in {:.1} s; then `os reboot`, then `os confirm`",
                began.elapsed().as_secs_f64()
            );
            Ok(())
        }
        // Over the serial port the board is in reach: no signature.
        Cmd::OsReboot { .. } => dev
            .os_reboot(None)
            .map(|()| println!("rebooting into the committed root on trial")),
        Cmd::OsConfirm { .. } => dev
            .os_confirm(OS_CONFIRM_ATTEMPTS, || std::thread::sleep(mwctl::os::CONFIRM_PAUSE), None)
            .map(|()| println!("confirmed: the root is permanent")),
        Cmd::OsAbort { .. } => dev.os_abort(None).map(|()| println!("aborted")),
        Cmd::Request { key, body } => {
            let key = middleware::Key::new(&key).map_err(|_| {
                Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("'{key}' is not a valid key"),
                ))
            })?;
            dev.request(key, body.as_bytes())
                .map(|reply| match std::str::from_utf8(&reply) {
                    Ok(t) if !t.chars().any(char::is_control) => println!("{t}"),
                    _ => println!(
                        "{}",
                        reply
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    ),
                })
        }
        Cmd::Start { key, counter } => {
            match control_auth(ControlOp::Start, target, key.as_ref(), counter)
                .and_then(|auth| dev.start_signed(auth))
            {
                Ok(()) => {
                    println!("payload started");
                    Ok(())
                }
                Err(e) => {
                    // The device logs why a start failed right after its reply;
                    // show those lines before reporting the error.
                    let _ = monitor(dev, Some(START_FAILURE_LOG_SECONDS), false);
                    Err(e)
                }
            }
        }
        Cmd::Stop { key, counter } => control_auth(ControlOp::Stop, target, key.as_ref(), counter)
            .and_then(|auth| dev.stop_signed(auth))
            .map(|()| println!("payload stopped")),
        Cmd::TrustInfo => dev.trust_info().map(|t| print!("{}", format_trust(&t))),
        Cmd::TrustClaim { key } => {
            let public = open_signer(&key)?.public_key();
            dev.trust_claim(public).map(|()| {
                println!(
                    "claimed: pinned {} key {}",
                    public.kind().name(),
                    hex(public.as_bytes())
                )
            })
        }
        Cmd::Upload {
            file,
            start,
            corrupt_chunk,
            key,
            version,
        } => {
            let image = match image {
                Some(i) => i,
                None => std::fs::read(&file).map_err(|e| io_context(&file, e))?,
            };
            if let Some(i) = corrupt_chunk {
                eprintln!("test hook: corrupting chunk {i}; the device must reject the commit");
            }
            let start_auth = match start {
                true => control_auth(ControlOp::Start, target, key.as_ref(), None)?,
                false => None,
            };
            let auth = match key {
                None => None,
                Some(k) => {
                    let signer = open_signer(&k)?;
                    let v = match version {
                        Some(v) => Version(v),
                        None => version_at(SystemTime::now())?,
                    };
                    let auth = sign_commit(&image, &*signer, v)?;
                    eprintln!("signing as version {} with {k}", v.0);
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
        Cmd::Net {
            link,
            ip,
            mac,
            peer,
        } => {
            let changed = ip.is_some() || mac.is_some() || peer.is_some();
            dev.net(link, ip, mac, peer).map(|a| {
                print!("{}", format_net(link, &a));
                if changed {
                    println!(
                        "(applied after the reply; an IP change takes effect for the next request)"
                    );
                }
            })
        }
        Cmd::Monitor { seconds, samples } => monitor(dev, seconds, samples),
        // Serial only, handled by `fw_command` or `reflash_command` before
        // any device is opened (the CLI refuses --espnow).
        Cmd::Reset
        | Cmd::Reflash { .. }
        | Cmd::FwInfo
        | Cmd::FwUpdate { .. }
        | Cmd::FwReboot { .. }
        | Cmd::FwConfirm { .. }
        | Cmd::FwAbort { .. } => Ok(()),
    }
}

fn monitor<L: Endpoint>(
    dev: &mut Device<L>,
    seconds: Option<u64>,
    samples: bool,
) -> Result<(), Error> {
    let end = seconds.map(|s| Instant::now() + Duration::from_secs(s));
    match seconds {
        Some(s) => eprintln!("monitoring for {s} s"),
        None => eprintln!("monitoring until Ctrl-C"),
    }
    // One request first: a USB CDC device that found no reader drops its
    // publications until a frame from the host arrives (the RP and RT base
    // firmwares' host-presence rule), so the monitor says it is here. The
    // reply (or its absence) does not matter.
    let _ = dev.info();
    // Bounded by `end` when given; otherwise runs until interrupted.
    loop {
        let slice = Instant::now() + Duration::from_millis(500);
        let deadline = end.map_or(slice, |e| e.min(slice));
        if let Some(e) = dev.next_event(deadline)? {
            let line = match samples.then(|| mwctl::report::format_samples(&e)).flatten() {
                Some(line) => Some(line),
                None if mwctl::report::is_sample_stream(&e) => None,
                None => Some(format_event(&e)),
            };
            if let Some(line) = line {
                println!("{line}");
            }
            let _ = std::io::stdout().flush();
        }
        if end.is_some_and(|e| Instant::now() >= e) {
            return Ok(());
        }
    }
}

/// `scout`: the nodes on the local network ([`mwctl::scout`]).
#[cfg(feature = "zenoh")]
fn scout(seconds: u64) -> Result<(), Error> {
    eprintln!("scouting Zenoh on the local network for {seconds} s ...");
    let rows = mwctl::scout::run(seconds)?;
    print!("{}", mwctl::scout::format(&rows));
    Ok(())
}

#[cfg(not(feature = "zenoh"))]
fn scout(_seconds: u64) -> Result<(), Error> {
    Err(Error::Io(std::io::Error::other(
        "this mwctl was built without Zenoh: cargo build -p mwctl --features zenoh",
    )))
}

/// `auth login|token|logout|status` ([`mwctl::auth`]). Only `token` writes a
/// token, to stdout; everything else goes to stderr.
#[cfg(feature = "auth")]
fn auth(cmd: cli::AuthCmd) -> Result<(), String> {
    use mwctl::auth;
    let cfg = auth::Config::from_env(|k| std::env::var(k).ok())?;
    match cmd {
        cli::AuthCmd::Login => {
            let t = auth::login(&cfg, &mut |line| eprintln!("{line}"))?;
            let renew = if t.refresh.is_some() { ", renewable" } else { "" };
            eprintln!("signed in to {} as client {}{renew}", cfg.issuer, cfg.client_id);
        }
        cli::AuthCmd::Token => println!("{}", auth::access_token(&cfg)?),
        cli::AuthCmd::Logout => eprintln!("{}", auth::logout(&cfg)?),
        cli::AuthCmd::Status => eprintln!("{}", auth::status(&cfg)?),
    }
    Ok(())
}

#[cfg(not(feature = "auth"))]
fn auth(_cmd: cli::AuthCmd) -> Result<(), String> {
    Err("this mwctl was built without auth: cargo build -p mwctl --features auth".into())
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
        Ok(Parsed::Auth(a)) => match auth(a) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("mwctl: {e}");
                ExitCode::from(1)
            }
        },
        Ok(Parsed::Scout { seconds }) => match scout(seconds) {
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
