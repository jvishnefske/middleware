//! Command-line parsing (pure).
//!
//! [`parse`] turns the arguments after the program name into a [`Cli`]
//! (device commands, which need `--port`), a local [`KeyCmd`] (no port) or
//! a [`Parsed::Help`] request. Options may appear before or after the
//! command; command-specific options are rejected on other commands.
//! [`HELP`] is the usage text printed by `--help` and on usage errors.
//!
//! `fw info|update|confirm|abort|reboot` drive a board's A/B firmware
//! updater (`middleware::fw_ota`: the RP2350 and the RT1050 today) over its
//! serial port, or over Zenoh (`--espnow` is refused). `fw update` takes
//! `--key`/`--pkcs11` (sign the commit), `--version MAJOR.MINOR` (a
//! cross-check of the image's own version) and `--no-reboot` (stop after
//! the commit).
//!
//! `--zenoh <ENDPOINT>` (connect) or `--zenoh-listen <ENDPOINT>` with
//! `--node <ID>` sends the command to that board over a Zenoh session
//! instead of a port (mwctl built with feature `zenoh`): `info`, `board`, `manifest`,
//! `trust info`, `request`, `upload`, `start`, `stop` and `fw *`. There
//! `upload`, `start`, `stop`, `fw update`, `fw reboot`, `fw confirm` and
//! `fw abort` need a key (signed commits, signed control requests for the
//! board's `--target` address, by default the RT1050 rule from the node
//! id); `trust claim`, `monitor` and `reset` are refused.
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
    mwctl --udp <HOST[:PORT]> [--timeout <MS>] <COMMAND>   (over UDP, see below)
    mwctl --zenoh <ENDPOINT> | --zenoh-listen <ENDPOINT> --node <ID> <COMMAND>  (network)
    mwctl scout [--seconds <N>]                     (Zenoh nodes on the LAN, see below)
    mwctl key new <KEYFILE> | key pub <KEYFILE>
    mwctl key new --pkcs11 <LABEL> | key pub --pkcs11 <LABEL> | key init-token <TOKEN>

COMMANDS:
    info                       Print chip, firmware, ABI, capacity and payload status
    board                      Print the board description (sys/board): pin classes,
                               PWM pins, limits; firmware 0.6 or later
    manifest                   Print what the node offers payloads (sys/manifest):
                               import groups, buses, sources and their rates, links,
                               limits
    bind <FILE.wasm> [CHANNEL=TARGET ...]
                               Bind the payload's channels (ABI 7, sys/payload/bind);
                               TARGET is key:<key>, remote:<node16>:<key>,
                               mic:<rate>:<levels|mulaw|pcm>[:<gain>],
                               imu:<rate>[:<range>[:mag]] or adapt:<adapter>:<TARGET>;
                               applies at the payload's next start
        --key <KEYFILE>            sign it (required over Zenoh on a claimed board)
        --version <N>              binding version (default: the Unix time)
    binding                    Print the stored binding (sys/payload/binding)
    peers                      Print the node ids the board has heard (sys/peers)
        manifest --of <ID>         a peer's manifest, asked through the board
                                   (sys/peer/<ID>/manifest; the RT1050 over Zenoh)
    upload <FILE.wasm>         Install a payload (sys/loader begin/chunk/commit)
        --key <KEYFILE>            sign the commit (Ed25519); without it the commit is
                                   unsigned, which only an unclaimed device accepts
        --pkcs11 <LABEL>           sign with that key in a PKCS#11 token instead (P-256
                                   or Ed25519; see KEYS IN A TOKEN)
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
    net [--link <N>]           Colorlight 5A-75B: print one link's addresses (sys/net/addr):
                               IP, MAC and the peer publications go to [default link 0]
        --ip <A.B.C.D>             set the link's IP address
        --mac <MAC>                set the link's MAC address
        --peer <A.B.C.D:PORT>      pin the peer; `--peer learn` returns to learning it
                                   from the latest unicast sender. Changes apply after
                                   the reply and are kept across resets
    monitor [--seconds <N>]    Print sys/log and sys/payload/status publications
                               (and other publications) until N seconds pass or Ctrl-C
        --samples                  decode sample batches (middleware::stream::samples):
                                   one line per batch with its header and values
    reset                      Reboot into run mode: RTS asserted for 100 ms with DTR
                               deasserted (EN pulse through the DTR/RTS auto-reset circuit)
    trust info                 Print the root of trust: unclaimed, or the pinned key and
                               the last signed version installed
    trust claim --key <KEYFILE>
                               Pin KEYFILE's public key (sys/trust/claim). Accepted when
                               unclaimed, or within 60 s after BOOT was held for 3 s
    fw info                    Firmware update state, running slot, slot versions (RP2350,
                               RT1050, and every board with A/B firmware updates)
    fw update <IMAGE>          Install a firmware image (UF2, .bin, .slot) in the
                               inactive A/B slot, reboot into it on trial, reconnect to the
                               same --port and confirm it once its self-test passes
        --key <KEYFILE>            sign the commit (a claimed device requires it)
        --version <MAJOR.MINOR>    expected image version (checked against its IMAGE_DEF)
        --no-reboot                stop after the commit (then fw reboot, fw confirm)
    request <KEY> [TEXT]       Send one request (body TEXT, UTF-8, or empty) and print the
                               reply as text, or hex if it is not text (e.g.
                               sys/link/sub app/**, sys/zenoh/endpoint tcp/192.168.1.10:7447)
    fw reboot                  Reboot into the committed image on trial
    fw confirm                 Keep the image on trial (explicit buy)
    fw abort                   Abandon a transfer or a committed update, or
                               reject the image on trial (reboot into the previous one)
    scout [--seconds <N>]      List the middleware nodes on the local network (mwctl built
                               with --features zenoh): Zenoh multicast scouting for N s
                               [default 3], each router's and peer's sessions from its
                               admin space (clients such as the RT1050 appear there), and
                               every node's sys/info. Not seen: ESP-NOW nodes (ask a
                               gateway's `peers`), sessions behind no scouted router, and
                               other subnets. Needs no --port
    key new <KEYFILE>          Create a signing key: 32 random bytes (the Ed25519 seed),
                               mode 0600, never overwriting; prints the public key
    key pub <KEYFILE>          Print a key file's public key (hex)
    key new --pkcs11 <LABEL>   Generate a P-256 key pair inside the PKCS#11 token (the
                               private key never leaves it); refuses a label in use
    key pub --pkcs11 <LABEL>   Print the token key's public key (hex, as pinned)
    key init-token <TOKEN>     Initialise the module's one UNinitialised token with label
                               TOKEN, SO PIN PKCS11_SO_PIN and user PIN PKCS11_PIN;
                               refuses when every token is initialised (that would erase it)

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
    --udp <HOST[:PORT]>
                       Talk to a node over UDP instead of a serial port: one raw frame
                       (no COBS) per datagram, port 7448 by default (the Colorlight
                       5A-75B gateware). Not with --port, --baud or --espnow; not for
                       reset or fw.
    --zenoh <ENDPOINT> Talk to one board over a Zenoh session that connects to ENDPOINT
                       (a router, e.g. tcp/192.168.1.10:7447) [needs mwctl built with
                       --features zenoh]
    --zenoh-listen <ENDPOINT>
                       The same, but listen on ENDPOINT for boards that connect (the
                       RT1050 is a Zenoh client of its sys/zenoh/endpoint)
    --node <ID>        The board's node id (16 hex digits, as `info` prints it)
    --target <MAC>     Address signed control requests name [default: the RT1050's
                       Ethernet MAC, 02 and the node id's last five bytes]. Over Zenoh
                       upload, start, stop and fw update/reboot/confirm/abort need a key
                       (signed commits and signed control requests); trust claim,
                       monitor and reset do not go over Zenoh.
    -h, --help         Print this help

KEYS IN A TOKEN:
    Every --key option also takes --pkcs11 <LABEL> (the release key is mw-release). The
    module is PKCS11_MODULE [default /usr/lib/x86_64-linux-gnu/opensc-pkcs11.so; a TPM
    2.0 is /usr/lib/x86_64-linux-gnu/pkcs11/libtpm2_pkcs11.so], the token PKCS11_TOKEN
    [default: the one token holding LABEL] and the PIN PKCS11_PIN. Each is read from the
    environment, else from a NAME=value line of ~/.env; a PIN is never taken from the
    command line. A browser cannot reach a token: sign with mwctl.

The port is opened with DTR and RTS deasserted so opening it neither resets
the board nor selects the ROM bootloader. Exit status is 0 on success, 1 when
the device or port fails, 2 on a usage error.
";

/// How `--zenoh` / `--zenoh-listen` reach the board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZenohRoute {
    /// `--zenoh <ENDPOINT>`: connect to it (a router or the board).
    Connect(String),
    /// `--zenoh-listen <ENDPOINT>`: listen for boards that connect.
    Listen(String),
}

/// The device over Zenoh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZenohTarget {
    /// How the session reaches it.
    pub route: ZenohRoute,
    /// Its node id (`--node`, 16 hex digits, as `info` prints it).
    pub node: [u8; 8],
    /// The address signed control requests name (`--target`); `None`
    /// derives the RT1050's Ethernet MAC from the node id.
    pub target: Option<[u8; 6]>,
}

/// Default baud rate of the base firmware's console UART.
pub const DEFAULT_BAUD: u32 = 115_200;
/// Default per-attempt timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(1000);

/// Where a signing key lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySpec {
    /// `--key <FILE>`: a raw 32-byte Ed25519 seed file.
    File(PathBuf),
    /// `--pkcs11 <LABEL>`: the key with this label in a PKCS#11 token
    /// (module, token and PIN from `PKCS11_*`, see [`crate::pkcs11`]).
    Pkcs11(String),
}

impl From<&str> for KeySpec {
    fn from(file: &str) -> Self {
        KeySpec::File(PathBuf::from(file))
    }
}

impl std::fmt::Display for KeySpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeySpec::File(p) => write!(f, "{}", p.display()),
            KeySpec::Pkcs11(l) => write!(f, "PKCS#11 key '{l}'"),
        }
    }
}

/// A local key command (no device, no port).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyCmd {
    /// `key new <file>` or `key new --pkcs11 <label>`.
    New {
        /// Key file to create, or token key to generate (P-256).
        key: KeySpec,
    },
    /// `key pub <file>` or `key pub --pkcs11 <label>`.
    Pub {
        /// Key whose public half is printed.
        key: KeySpec,
    },
    /// `key init-token <TOKEN-LABEL>`: initialise the one uninitialised
    /// token of `PKCS11_MODULE` (SO PIN `PKCS11_SO_PIN`, user PIN
    /// `PKCS11_PIN`).
    InitToken {
        /// The new token's label.
        token: String,
    },
}

/// A device subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// `info`.
    Info,
    /// `board`.
    Board,
    /// `manifest [--of <ID>]`: this node's, or a peer's through it.
    Manifest {
        /// The peer (`--of`), asked through the connected board.
        of: Option<[u8; 8]>,
    },
    /// `peers`.
    Peers,
    /// `bind <FILE.wasm> [CHANNEL=TARGET ...]`.
    Bind {
        /// The payload image the binding is for (its SHA-256).
        file: PathBuf,
        /// `CHANNEL=TARGET` specs ([`crate::bindspec`]).
        specs: Vec<String>,
        /// Signing key; `None` sends an unsigned binding.
        key: Option<KeySpec>,
        /// Binding version; `None` uses the current Unix time.
        version: Option<u32>,
    },
    /// `binding`.
    Binding,
    /// `upload`.
    Upload {
        /// Payload image.
        file: PathBuf,
        /// Start after commit.
        start: bool,
        /// Test hook: chunk to corrupt.
        corrupt_chunk: Option<usize>,
        /// Signing key; `None` sends an unsigned commit.
        key: Option<KeySpec>,
        /// Signed version; `None` uses the current Unix time.
        version: Option<u32>,
    },
    /// `trust info`.
    TrustInfo,
    /// `trust claim --key <file>`.
    TrustClaim {
        /// Key whose public key is pinned.
        key: KeySpec,
    },
    /// `start`.
    Start {
        /// Signing key for a control request (`--espnow` only).
        key: Option<KeySpec>,
        /// Control counter; `None` uses the current Unix time in ms.
        counter: Option<u64>,
    },
    /// `stop`.
    Stop {
        /// Signing key for a control request (`--espnow` only).
        key: Option<KeySpec>,
        /// Control counter; `None` uses the current Unix time in ms.
        counter: Option<u64>,
    },
    /// `net`: read one link's addresses, then set what is given.
    Net {
        /// The link.
        link: u8,
        /// New IP address.
        ip: Option<[u8; 4]>,
        /// New MAC address.
        mac: Option<[u8; 6]>,
        /// New pinned peer (`[0; 4]`, 0 to learn it again).
        peer: Option<([u8; 4], u16)>,
    },
    /// `monitor`; `None` runs until interrupted.
    Monitor {
        /// How long to listen.
        seconds: Option<u64>,
        /// Decode sample batches (`--samples`).
        samples: bool,
    },
    /// `reset`.
    Reset,
    /// `fw info`.
    FwInfo,
    /// `fw update`.
    FwUpdate {
        /// Image file (UF2 or raw binary).
        file: PathBuf,
        /// Signing key.
        key: Option<KeySpec>,
        /// Expected version.
        version: Option<middleware::loader::trust::Version>,
        /// Reboot into the new image and confirm it.
        reboot: bool,
    },
    /// `fw reboot`.
    FwReboot {
        /// Signs the request (over Zenoh only).
        key: Option<KeySpec>,
    },
    /// `request <KEY> [TEXT]`: one request with a UTF-8 body (empty if
    /// none); the reply is printed as text, or hex if it is not text.
    Request {
        /// The key.
        key: String,
        /// The body.
        body: String,
    },
    /// `fw confirm`.
    FwConfirm {
        /// Signs the request (over Zenoh only).
        key: Option<KeySpec>,
    },
    /// `fw abort`.
    FwAbort {
        /// Signs the request (over Zenoh only).
        key: Option<KeySpec>,
    },
}

/// Parsed arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    /// Serial port path; empty with [`Cli::udp`] or over Zenoh.
    pub port: String,
    /// Over UDP to this `host:port` (the default port filled in) instead
    /// of a serial port.
    pub udp: Option<String>,
    /// Baud rate.
    pub baud: u32,
    /// Per-attempt timeout.
    pub timeout: Duration,
    /// Over ESP-NOW to the device with this MAC, through the bridge on
    /// `port`.
    pub espnow: Option<[u8; 6]>,
    /// Over Zenoh to this board instead of a port.
    pub zenoh: Option<ZenohTarget>,
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
    /// `scout [--seconds N]`: list the nodes on the local network.
    Scout {
        /// How long to scout.
        seconds: u64,
    },
    /// Print [`HELP`] and exit successfully.
    Help,
}

/// Parses a node id: 16 hex digits.
pub fn parse_node(s: &str) -> Option<[u8; 8]> {
    if s.len() != 16 || !s.is_ascii() {
        return None;
    }
    let mut id = [0u8; 8];
    for (i, b) in id.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(id)
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
    let mut udp = None;
    let mut timeout = DEFAULT_TIMEOUT;
    let mut positional: Vec<String> = Vec::new();
    let mut start = false;
    let mut corrupt_chunk = None;
    let mut seconds = None;
    let mut key: Option<KeySpec> = None;
    let mut key_flags = 0;
    let mut version: Option<String> = None;
    let mut counter = None;
    let mut no_reboot = false;
    let mut link: Option<u8> = None;
    let mut net_ip = None;
    let mut net_mac = None;
    let mut net_peer = None;
    let mut samples = false;
    let mut zroute: Option<ZenohRoute> = None;
    let mut zroutes = 0;
    let mut node: Option<[u8; 8]> = None;
    let mut of: Option<[u8; 8]> = None;
    let mut ztarget: Option<[u8; 6]> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "--port" => port = Some(args.next().ok_or("--port needs a path")?),
            "--udp" => {
                let v = args.next().ok_or("--udp needs HOST[:PORT]")?;
                udp = Some(crate::udp::with_default_port(&v)?);
            }
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
            "--key" => {
                key = Some(KeySpec::File(PathBuf::from(
                    args.next().ok_or("--key needs a key file")?,
                )));
                key_flags += 1;
            }
            "--pkcs11" => {
                key = Some(KeySpec::Pkcs11(
                    args.next()
                        .ok_or("--pkcs11 needs a key label, e.g. mw-release")?,
                ));
                key_flags += 1;
            }
            "--version" => version = Some(args.next().ok_or("--version needs a value")?),
            "--no-reboot" => no_reboot = true,
            "--samples" => samples = true,
            "--counter" => counter = Some(number("--counter", args.next())?),
            "--link" => link = Some(number("--link", args.next())?),
            "--ip" => {
                let v = args.next().ok_or("--ip needs an address such as 192.168.1.50")?;
                net_ip = Some(parse_ipv4(&v).ok_or_else(|| format!("--ip: '{v}' is not an IPv4 address"))?);
            }
            "--mac" => {
                let v = args.next().ok_or("--mac needs a MAC such as 02:5a:75:b0:00:00")?;
                net_mac = Some(
                    crate::bridge::parse_mac(&v).ok_or_else(|| format!("--mac: '{v}' is not a MAC"))?,
                );
            }
            "--peer" => {
                let v = args.next().ok_or("--peer needs A.B.C.D:PORT or 'learn'")?;
                net_peer = Some(parse_peer(&v).ok_or_else(|| {
                    format!("--peer: '{v}' is not A.B.C.D:PORT (port 1..65535) or 'learn'")
                })?);
            }
            "--zenoh" => {
                zroute = Some(ZenohRoute::Connect(
                    args.next()
                        .ok_or("--zenoh needs an endpoint such as tcp/192.168.1.10:7447")?,
                ));
                zroutes += 1;
            }
            "--zenoh-listen" => {
                zroute = Some(ZenohRoute::Listen(
                    args.next()
                        .ok_or("--zenoh-listen needs an endpoint such as tcp/0.0.0.0:7447")?,
                ));
                zroutes += 1;
            }
            "--of" => {
                let v = args.next().ok_or("--of needs a node id (16 hex digits)")?;
                of = Some(parse_node(&v).ok_or_else(|| format!("--of: '{v}' is not a node id (16 hex digits)"))?);
            }
            "--node" => {
                let v = args
                    .next()
                    .ok_or("--node needs a node id (16 hex digits)")?;
                node = Some(parse_node(&v).ok_or_else(|| {
                    format!("--node: '{v}' is not a node id (16 hex digits, as `info` prints it)")
                })?);
            }
            "--target" => {
                let v = args
                    .next()
                    .ok_or("--target needs a MAC such as 02:d2:65:f8:24:84")?;
                ztarget = Some(
                    crate::bridge::parse_mac(&v)
                        .ok_or_else(|| format!("--target: '{v}' is not a MAC"))?,
                );
            }
            s if s.starts_with('-') => return Err(format!("unknown option '{s}'")),
            _ => positional.push(a),
        }
    }
    let baud_given = baud.is_some();
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
    if zroutes > 1 {
        return Err("give one of --zenoh <ENDPOINT> or --zenoh-listen <ENDPOINT>".into());
    }
    let zenoh = match (zroute, node) {
        (Some(route), Some(node)) => Some(ZenohTarget {
            route,
            node,
            target: ztarget,
        }),
        (Some(_), None) => {
            return Err("--zenoh needs --node <ID> (the board's node id, from `info`)".into());
        }
        (None, Some(_)) => return Err("--node applies only with --zenoh or --zenoh-listen".into()),
        (None, None) if ztarget.is_some() => {
            return Err("--target applies only with --zenoh or --zenoh-listen".into());
        }
        (None, None) => None,
    };
    if zenoh.is_some() && espnow.is_some() {
        return Err("--espnow and --zenoh exclude each other".into());
    }
    let mut pos = positional.into_iter();
    let first = pos.next().ok_or("no command given")?;
    // Two-word commands are named "trust info", "key new", ...
    let name = match first.as_str() {
        "trust" | "key" | "fw" => {
            let sub = pos
                .next()
                .ok_or_else(|| format!("'{first}' needs a subcommand"))?;
            format!("{first} {sub}")
        }
        _ => first,
    };
    if name == "scout" {
        if port.is_some() || udp.is_some() || espnow.is_some() || zenoh.is_some() || baud_given {
            return Err(
                "scout finds the nodes itself: drop --port, --udp, --espnow and --zenoh".into(),
            );
        }
        let other = start
            || corrupt_chunk.is_some()
            || key.is_some()
            || version.is_some()
            || counter.is_some()
            || no_reboot
            || samples
            || link.is_some()
            || net_ip.is_some()
            || net_mac.is_some()
            || net_peer.is_some()
            || of.is_some();
        if other {
            return Err("scout takes only --seconds <N>".into());
        }
        if let Some(extra) = pos.next() {
            return Err(format!("unexpected argument '{extra}'"));
        }
        let seconds = seconds.unwrap_or(crate::scout::DEFAULT_SECONDS);
        if seconds == 0 {
            return Err("--seconds must be positive".into());
        }
        return Ok(Parsed::Scout { seconds });
    }
    let only = |flag: &str, set: bool, cmds: &[&str]| {
        if set && !cmds.contains(&name.as_str()) {
            Err(format!("{flag} applies only to '{}'", cmds.join("' or '")))
        } else {
            Ok(())
        }
    };
    if zenoh.is_some()
        && matches!(
            name.as_str(),
            "trust claim" | "monitor" | "reset" | "key new" | "key pub" | "key init-token"
        )
    {
        return Err(format!(
            "'{name}' does not go over Zenoh; use the board's serial port"
        ));
    }
    only("--start", start, &["upload"])?;
    only("--corrupt-chunk", corrupt_chunk.is_some(), &["upload"])?;
    only("--seconds", seconds.is_some(), &["monitor"])?;
    only("--samples", samples, &["monitor"])?;
    if key_flags > 1 {
        return Err("give one key: --key <KEYFILE> or --pkcs11 <LABEL>".into());
    }
    let key_cmds: &[&str] = if zenoh.is_some() {
        &[
            "upload",
            "bind",
            "start",
            "stop",
            "fw update",
            "fw reboot",
            "fw confirm",
            "fw abort",
        ]
    } else {
        &["upload", "bind", "trust claim", "start", "stop", "fw update"]
    };
    match &key {
        Some(KeySpec::File(_)) => only("--key", true, key_cmds)?,
        Some(KeySpec::Pkcs11(_)) => only(
            "--pkcs11",
            true,
            &[key_cmds, &["key new", "key pub"]].concat(),
        )?,
        None => {}
    }
    only("--version", version.is_some(), &["upload", "bind", "fw update"])?;
    only("--no-reboot", no_reboot, &["fw update"])?;
    let fw_version = match (&version, name.as_str()) {
        (Some(v), "fw update") => Some(
            crate::fw::parse_version(v)
                .ok_or_else(|| format!("--version: '{v}' is not MAJOR.MINOR (each 0..65535)"))?,
        ),
        _ => None,
    };
    let version: Option<u32> = match (version, name.as_str()) {
        (Some(v), "upload" | "bind") => Some(number("--version", Some(v))?),
        _ => None,
    };
    if name.starts_with("fw ") && espnow.is_some() {
        return Err(format!(
            "'{name}' goes over the serial port only; drop --espnow and use the device's own port"
        ));
    }
    only("--counter", counter.is_some(), &["start", "stop"])?;
    only("--link", link.is_some(), &["net"])?;
    only("--ip", net_ip.is_some(), &["net"])?;
    only("--mac", net_mac.is_some(), &["net"])?;
    only("--peer", net_peer.is_some(), &["net"])?;
    only("--of", of.is_some(), &["manifest"])?;
    if zenoh.is_some() {
        let signed = matches!(
            name.as_str(),
            "upload" | "start" | "stop" | "fw update" | "fw reboot" | "fw confirm" | "fw abort"
        );
        if signed && key.is_none() {
            return Err(format!(
                "over Zenoh a claimed board accepts '{name}' only signed by its pinned key: pass --key <KEYFILE> or --pkcs11 <LABEL>"
            ));
        }
    }
    if version.is_some() && key.is_none() && name != "bind" {
        return Err("--version needs --key (only signed commits carry a version)".into());
    }
    if counter.is_some() && key.is_none() {
        return Err("--counter needs --key (only signed control requests carry a counter)".into());
    }
    let control = matches!(name.as_str(), "start" | "stop");
    if control && key.is_some() && espnow.is_none() && zenoh.is_none() {
        return Err(format!(
            "'{name} --key' signs a control request for --espnow or --zenoh; over serial '{name}' needs no signature"
        ));
    }
    if espnow.is_some() && key.is_none() && (control || name == "upload") {
        return Err(format!(
            "over ESP-NOW a claimed device accepts '{name}' only signed by its pinned key: pass --key <KEYFILE> or --pkcs11 <LABEL>"
        ));
    }
    let key_arg = |pos: &mut std::vec::IntoIter<String>, key: Option<KeySpec>, cmd: &str| match key
    {
        Some(k) => Ok(k),
        None => pos
            .next()
            .map(|f| KeySpec::File(PathBuf::from(f)))
            .ok_or_else(|| format!("{cmd} needs a key file (or --pkcs11 <LABEL>)")),
    };
    let local = match name.as_str() {
        "key new" => Some(KeyCmd::New {
            key: key_arg(&mut pos, key.clone(), "key new")?,
        }),
        "key pub" => Some(KeyCmd::Pub {
            key: key_arg(&mut pos, key.clone(), "key pub")?,
        }),
        "key init-token" => Some(KeyCmd::InitToken {
            token: pos
                .next()
                .ok_or("key init-token needs the new token's label")?,
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
        "board" => Cmd::Board,
        "manifest" => Cmd::Manifest { of },
        "peers" => Cmd::Peers,
        "bind" => Cmd::Bind {
            file: PathBuf::from(pos.next().ok_or("bind needs the payload's .wasm file")?),
            specs: pos.by_ref().collect(),
            key,
            version,
        },
        "binding" => Cmd::Binding,
        "upload" => Cmd::Upload {
            file: PathBuf::from(pos.next().ok_or("upload needs a .wasm file")?),
            start,
            corrupt_chunk,
            key,
            version,
        },
        "trust info" => Cmd::TrustInfo,
        "trust claim" => Cmd::TrustClaim {
            key: key.ok_or("trust claim needs --key <KEYFILE> or --pkcs11 <LABEL>")?,
        },
        "start" => Cmd::Start { key, counter },
        "stop" => Cmd::Stop { key, counter },
        "net" => Cmd::Net {
            link: link.unwrap_or(0),
            ip: net_ip,
            mac: net_mac,
            peer: net_peer,
        },
        "monitor" => Cmd::Monitor { seconds, samples },
        "reset" => Cmd::Reset,
        "fw info" => Cmd::FwInfo,
        "fw update" => Cmd::FwUpdate {
            file: PathBuf::from(
                pos.next()
                    .ok_or("fw update needs an image file (.uf2 or .bin)")?,
            ),
            key,
            version: fw_version,
            reboot: !no_reboot,
        },
        "fw reboot" => Cmd::FwReboot { key },
        "request" => Cmd::Request {
            key: pos.next().ok_or("request needs a key")?,
            body: pos.next().unwrap_or_default(),
        },
        "fw confirm" => Cmd::FwConfirm { key },
        "fw abort" => Cmd::FwAbort { key },
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
    if udp.is_some() {
        if port.is_some() || baud_given || espnow.is_some() || zenoh.is_some() {
            return Err(
                "--udp replaces --port, --baud, --espnow and --zenoh; give only one link".into(),
            );
        }
        if matches!(cmd, Cmd::Reset) || name.starts_with("fw ") {
            return Err(format!(
                "'{name}' needs the board's serial port; drop --udp and use --port"
            ));
        }
        return Ok(Parsed::Run(Cli {
            port: String::new(),
            udp,
            baud,
            timeout,
            espnow,
            zenoh,
            cmd,
        }));
    }
    let port = match (&zenoh, port) {
        (Some(_), Some(_)) => return Err("--port and --zenoh exclude each other".into()),
        (Some(_), None) => String::new(),
        (None, port) => {
            port.ok_or("--port <PATH> (or --udp <HOST>) is required (mwctl never scans for ports)")?
        }
    };
    Ok(Parsed::Run(Cli {
        port,
        udp,
        baud,
        timeout,
        espnow,
        zenoh,
        cmd,
    }))
}

/// `A.B.C.D`.
pub fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
    let a: std::net::Ipv4Addr = s.parse().ok()?;
    Some(a.octets())
}

/// `A.B.C.D:PORT` (port 1..=65535), or `learn` for 0.0.0.0:0.
pub fn parse_peer(s: &str) -> Option<([u8; 4], u16)> {
    if s == "learn" {
        return Some(([0; 4], 0));
    }
    let a: std::net::SocketAddrV4 = s.parse().ok()?;
    if a.port() == 0 || a.ip().is_unspecified() {
        return None;
    }
    Some((a.ip().octets(), a.port()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cl8_net_reads_or_sets_a_link() {
        assert_eq!(
            run("--udp 192.168.1.50 net").cmd,
            Cmd::Net { link: 0, ip: None, mac: None, peer: None }
        );
        assert_eq!(
            run("--udp 192.168.1.50 net --link 1 --ip 192.168.2.51 --mac 02:5a:75:b0:00:11 --peer 10.0.1.9:7000").cmd,
            Cmd::Net {
                link: 1,
                ip: Some([192, 168, 2, 51]),
                mac: Some([0x02, 0x5a, 0x75, 0xb0, 0x00, 0x11]),
                peer: Some(([10, 0, 1, 9], 7000)),
            }
        );
        assert_eq!(
            run("--udp h net --peer learn").cmd,
            Cmd::Net { link: 0, ip: None, mac: None, peer: Some(([0; 4], 0)) }
        );
        assert!(p("--udp h net --peer 10.0.0.1").unwrap_err().contains("--peer"));
        assert!(p("--udp h net --peer 0.0.0.0:5").unwrap_err().contains("--peer"));
        assert!(p("--udp h net --ip 300.1.1.1").unwrap_err().contains("--ip"));
        assert!(p("--udp h info --link 1").unwrap_err().contains("applies only to 'net'"));
        assert!(p("--udp h net --link x").unwrap_err().contains("--link"));
    }

    #[test]
    fn h4_scout_needs_no_link_and_takes_only_seconds() {
        assert_eq!(p("scout"), Ok(Parsed::Scout { seconds: 3 }));
        assert_eq!(p("scout --seconds 10"), Ok(Parsed::Scout { seconds: 10 }));
        assert_eq!(p("--seconds 1 scout"), Ok(Parsed::Scout { seconds: 1 }));
        assert!(p("scout --seconds 0").unwrap_err().contains("positive"));
        assert!(p("--port p scout").unwrap_err().contains("drop --port"));
        assert!(p("--udp h scout").unwrap_err().contains("drop --port"));
        assert!(p("scout --key k").unwrap_err().contains("only --seconds"));
        assert!(
            p("scout extra")
                .unwrap_err()
                .contains("unexpected argument")
        );
        // --seconds still belongs to monitor elsewhere.
        assert!(
            p("--port p info --seconds 2")
                .unwrap_err()
                .contains("applies only to 'monitor'")
        );
    }

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
    fn request_takes_a_key_and_an_optional_text_body() {
        assert_eq!(
            run("--port p request sys/zenoh/endpoint tcp/192.168.1.10:7447").cmd,
            Cmd::Request {
                key: "sys/zenoh/endpoint".into(),
                body: "tcp/192.168.1.10:7447".into()
            }
        );
        assert_eq!(
            run("--port p request sys/info").cmd,
            Cmd::Request {
                key: "sys/info".into(),
                body: String::new()
            }
        );
        assert!(p("--port p request").is_err());
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
    fn zenoh_routes_need_a_node_and_keys_for_what_changes_the_board() {
        let c = run(
            "--zenoh-listen tcp/0.0.0.0:7447 --node 310441d265f82484 fw update a.slot --pkcs11 mw-release",
        );
        let z = c.zenoh.unwrap();
        assert_eq!(z.route, ZenohRoute::Listen("tcp/0.0.0.0:7447".into()));
        assert_eq!(z.node, [0x31, 0x04, 0x41, 0xd2, 0x65, 0xf8, 0x24, 0x84]);
        assert_eq!(z.target, None);
        assert_eq!(c.port, "");
        let c = run(
            "--zenoh tcp/10.0.0.2:7447 --node 310441d265f82484 --target 02:01:02:03:04:05 stop --key k",
        );
        assert_eq!(c.zenoh.unwrap().target, Some([2, 1, 2, 3, 4, 5]));
        assert_eq!(
            c.cmd,
            Cmd::Stop {
                key: Some("k".into()),
                counter: None
            }
        );
        assert_eq!(
            run("--zenoh tcp/h:7447 --node 310441d265f82484 info").cmd,
            Cmd::Info
        );
        for (args, want) in [
            ("--zenoh tcp/h:7447 info", "needs --node"),
            ("--node 310441d265f82484 --port p info", "only with --zenoh"),
            (
                "--target 02:01:02:03:04:05 --port p info",
                "only with --zenoh",
            ),
            (
                "--zenoh tcp/h:7447 --zenoh-listen tcp/0:1 --node 310441d265f82484 info",
                "one of",
            ),
            ("--zenoh tcp/h:7447 --node 3104 info", "16 hex digits"),
            (
                "--zenoh tcp/h:7447 --node 310441d265f82484 --port p info",
                "exclude",
            ),
            (
                "--zenoh tcp/h:7447 --node 310441d265f82484 --port p --espnow e4:b0:63:81:d3:24 info",
                "exclude",
            ),
            (
                "--zenoh tcp/h:7447 --node 310441d265f82484 trust claim --key k",
                "does not go over Zenoh",
            ),
            (
                "--zenoh tcp/h:7447 --node 310441d265f82484 monitor",
                "does not go over Zenoh",
            ),
            (
                "--zenoh tcp/h:7447 --node 310441d265f82484 upload a.wasm",
                "only signed",
            ),
            (
                "--zenoh tcp/h:7447 --node 310441d265f82484 fw reboot",
                "only signed",
            ),
            (
                "--zenoh tcp/h:7447 --node 310441d265f82484 fw confirm",
                "only signed",
            ),
            (
                "--zenoh tcp/h:7447 --node 310441d265f82484 fw abort",
                "only signed",
            ),
            (
                "--zenoh tcp/h:7447 --node 310441d265f82484 stop",
                "only signed",
            ),
            ("--port p fw reboot --key k", "only to"),
            (
                "--port p upload a.wasm --key k --pkcs11 mw-release",
                "give one key",
            ),
        ] {
            let e = p(args).unwrap_err();
            assert!(e.contains(want), "{args}: {e}");
        }
        // Signed fw controls over Zenoh.
        assert_eq!(
            run("--zenoh tcp/h:7447 --node 310441d265f82484 fw confirm --pkcs11 mw-release").cmd,
            Cmd::FwConfirm {
                key: Some(KeySpec::Pkcs11("mw-release".into()))
            }
        );
    }

    #[test]
    fn pkcs11_keys_name_a_token_label() {
        assert_eq!(
            run("--port p trust claim --pkcs11 mw-release").cmd,
            Cmd::TrustClaim {
                key: KeySpec::Pkcs11("mw-release".into())
            }
        );
        assert_eq!(
            p("key new --pkcs11 mw-release"),
            Ok(Parsed::Key(KeyCmd::New {
                key: KeySpec::Pkcs11("mw-release".into())
            }))
        );
        assert_eq!(
            p("key pub --pkcs11 mw-release"),
            Ok(Parsed::Key(KeyCmd::Pub {
                key: KeySpec::Pkcs11("mw-release".into())
            }))
        );
        assert_eq!(
            p("key init-token mw"),
            Ok(Parsed::Key(KeyCmd::InitToken { token: "mw".into() }))
        );
        assert!(p("key init-token").unwrap_err().contains("label"));
        assert!(
            p("--port p info --pkcs11 x")
                .unwrap_err()
                .contains("only to")
        );
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
                udp: None,
                zenoh: None,
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
                udp: None,
                zenoh: None,
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
        assert_eq!(
            run("--port p monitor").cmd,
            Cmd::Monitor {
                seconds: None,
                samples: false
            }
        );
        assert_eq!(
            run("--port p monitor --samples").cmd,
            Cmd::Monitor {
                seconds: None,
                samples: true
            }
        );
        assert_eq!(
            run("--port p monitor --seconds 5").cmd,
            Cmd::Monitor {
                seconds: Some(5),
                samples: false
            }
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
        assert_eq!(run("--port p board").cmd, Cmd::Board);
        assert_eq!(run("--port p manifest").cmd, Cmd::Manifest { of: None });
        assert_eq!(
            run("--port p manifest --of 0011223344556677").cmd,
            Cmd::Manifest { of: Some([0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77]) }
        );
        assert_eq!(run("--port p peers").cmd, Cmd::Peers);
        assert_eq!(run("--port p binding").cmd, Cmd::Binding);
        assert_eq!(
            run("--port p bind m.wasm audio=mic:16000:levels level=key:lab/level --version 7").cmd,
            Cmd::Bind {
                file: "m.wasm".into(),
                specs: vec!["audio=mic:16000:levels".into(), "level=key:lab/level".into()],
                key: None,
                version: Some(7),
            }
        );
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
                key: "k.key".into()
            }))
        );
        assert_eq!(
            p("key pub k.key"),
            Ok(Parsed::Key(KeyCmd::Pub {
                key: "k.key".into()
            }))
        );
        assert_eq!(
            p("--port p key pub k.key"),
            Ok(Parsed::Key(KeyCmd::Pub {
                key: "k.key".into()
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
            ("--port p info --key k", "only to 'upload' or 'bind' or 'trust claim'"),
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
    fn y9_parses_fw_commands() {
        use middleware::loader::trust::Version;
        assert_eq!(run("--port p fw info").cmd, Cmd::FwInfo);
        assert_eq!(run("--port p fw confirm").cmd, Cmd::FwConfirm { key: None });
        assert_eq!(run("--port p fw abort").cmd, Cmd::FwAbort { key: None });
        assert_eq!(run("--port p fw reboot").cmd, Cmd::FwReboot { key: None });
        assert_eq!(
            run("--port p fw update fw.uf2").cmd,
            Cmd::FwUpdate {
                file: "fw.uf2".into(),
                key: None,
                version: None,
                reboot: true
            }
        );
        assert_eq!(
            run("fw update fw.bin --key k --version 1.2 --no-reboot --port p").cmd,
            Cmd::FwUpdate {
                file: "fw.bin".into(),
                key: Some("k".into()),
                version: Some(Version(0x0001_0002)),
                reboot: false
            }
        );
        for (args, why) in [
            ("--port p fw update", "needs an image"),
            ("--port p fw update a --version 3", "MAJOR.MINOR"),
            ("--port p fw info --no-reboot", "only to 'fw update'"),
            ("--port p upload a.wasm --no-reboot", "only to 'fw update'"),
            ("--port p fw confirm --key k", "only to"),
            ("--port p fw frob", "unknown command"),
            ("--port p fw", "needs a subcommand"),
            (
                "--port p --espnow e4:b0:63:81:d3:24 fw info",
                "serial port only",
            ),
            ("--port p fw info extra", "unexpected"),
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
            ("--port p info --samples", "only to 'monitor'"),
            ("--port p info --verbose", "unknown option"),
            ("--port", "needs a path"),
        ] {
            match p(args) {
                Err(e) => assert!(e.contains(why), "{args}: {e}"),
                Ok(v) => panic!("{args}: accepted as {v:?}"),
            }
        }
    }

    #[test]
    fn cl1_udp_replaces_the_port() {
        let c = run("--udp 192.168.1.50 info");
        assert_eq!(c.udp.as_deref(), Some("192.168.1.50:7448"));
        assert!(c.port.is_empty());
        assert_eq!(run("upload a.wasm --start --udp b:9").udp.as_deref(), Some("b:9"));
        for bad in [
            "--udp h --port p info",
            "--udp h --baud 9600 info",
            "--udp h --espnow e4:b0:63:81:d3:24 info",
            "--udp h reset",
            "--udp h fw info",
            "--udp h:99999 info",
            "--udp",
        ] {
            assert!(p(bad).is_err(), "{bad}");
        }
    }
}
