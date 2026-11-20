//! The device over Zenoh (feature `zenoh`, off by default: the default
//! mwctl stays free of the cloud crates, `scripts/check-features.sh`).
//!
//! [`ZenohLink`] is an [`Endpoint`] that sends every request as a Zenoh
//! query on the board's per-node key (`@mw/node/<node>/<key>`,
//! `Target::Node`), so exactly one board answers. The host session is a
//! Zenoh peer that either connects to an endpoint (a router, or a board's
//! peer listener) or listens for boards that connect to it (the RT1050
//! base firmware is a Zenoh client of its `sys/zenoh/endpoint`, often the
//! host itself). With an mTLS identity ([`TlsFiles`]) it is instead a
//! Zenoh client of the `tls/` router it connects to, presenting that
//! certificate (design.md H5).
//!
//! A board on the network accepts only what its Zenoh gate allows
//! (`firmware-imxrt/src/zenoh_mgmt.rs`, ZN5): uploads and firmware
//! updates with signed commits, and start/stop and firmware
//! reboot/confirm/abort only as signed control requests addressed to the
//! board's [`target`] address. A posix-node over Zenoh (PN21–PN27) answers
//! at the id of its Zenoh session, which is not its node id; its control
//! requests name its reported node id's first six bytes ([`target_for`]). Monitoring is not offered here
//! ([`Endpoint::next_event`] only waits).

use crate::error::Error;
use crate::link::{Endpoint, Event};
use middleware::Key;
use middleware::transport::{HandlerError, RequestOptions, Requester, SenderId, Target};
use middleware::zenoh::{SessionConfig, TlsFiles, ZenohError, ZenohTransport};
use std::time::{Duration, Instant};

/// How long one query may take: an RT1050 erases a 256 KiB HyperFlash
/// block (about 2 s) inside some requests.
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(8);

/// How the host session reaches the board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Connect to this endpoint (`tcp/<ip>:<port>`).
    Connect(String),
    /// Listen on this endpoint for boards that connect.
    Listen(String),
}

/// The 6-byte address a signed control request names on an RT1050 board:
/// its Ethernet MAC, `02` and the node id's last five bytes
/// (`firmware-imxrt/src/enet/mac.rs`, `mac_from_uid`).
pub fn target(node: SenderId) -> [u8; 6] {
    let n = node.0;
    [0x02, n[3], n[4], n[5], n[6], n[7]]
}

/// A board reported a node id it cannot have: a posix-node with node id
/// `reported` answers at `expected`, not at the id it was addressed at, so
/// signing for `target` (that node's address) could let whoever answered
/// replay the request to that node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetMismatch {
    /// The node id `sys/info` reported.
    pub reported: SenderId,
    /// The sender id a posix-node with that node id answers at.
    pub expected: SenderId,
    /// The address that node id would give.
    pub target: [u8; 6],
}

impl std::fmt::Display for TargetMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        let mac = self
            .target
            .iter()
            .map(|x| format!("{x:02x}"))
            .collect::<Vec<_>>()
            .join(":");
        write!(
            f,
            "the board reports node {}, whose session would answer at {}, not at the --node given; \
             refusing to sign for its control target {mac} (sys/info is not authenticated). \
             If this board really is that node, pass --target {mac}",
            hex(&self.reported.0),
            hex(&self.expected.0),
        )
    }
}

impl std::error::Error for TargetMismatch {}

/// The 6-byte address a signed control request names on the board whose
/// per-node key is `addressed` and whose `sys/info` reported `chip` and
/// node id `reported` (design.md PN27): the RT1050's Ethernet MAC
/// ([`target`] of `addressed`) on chip `rt1050` or when no node id was
/// reported; else the reported node id's first six bytes (a posix-node,
/// whose key carries its Zenoh session's id), but only when that node's
/// session id is `addressed`
/// (`middleware::zenoh::wire::posix_sender_id`): `sys/info` is not
/// authenticated, and a responder claiming another machine's node id is
/// [`TargetMismatch`].
pub fn target_for(
    chip: &str,
    addressed: SenderId,
    reported: Option<SenderId>,
) -> Result<[u8; 6], TargetMismatch> {
    let Some(n) = reported.filter(|_| chip != "rt1050") else {
        return Ok(target(addressed));
    };
    let first6 = [n.0[0], n.0[1], n.0[2], n.0[3], n.0[4], n.0[5]];
    let expected = middleware::zenoh::wire::posix_sender_id(n);
    if expected == addressed {
        Ok(first6)
    } else {
        Err(TargetMismatch {
            reported: n,
            expected,
            target: first6,
        })
    }
}

/// Parses a node id: 16 hex digits.
pub fn parse_node(s: &str) -> Option<SenderId> {
    if s.len() != 16 {
        return None;
    }
    let mut id = [0u8; 8];
    for (i, b) in id.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(SenderId(id))
}

/// One board over a Zenoh session.
pub struct ZenohLink {
    rt: tokio::runtime::Runtime,
    z: ZenohTransport,
    node: SenderId,
    timeout: Duration,
}

impl ZenohLink {
    /// Opens the host session on `route` for the board `node`; with `tls`
    /// it connects as a Zenoh client over mTLS (only on [`Route::Connect`]).
    pub fn open(route: &Route, node: SenderId, tls: Option<TlsFiles>) -> Result<Self, Error> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(Error::Io)?;
        let config = match (route, tls) {
            (Route::Connect(e), None) => SessionConfig::new().connect(e.clone()),
            (Route::Connect(e), Some(files)) => {
                SessionConfig::new().client().connect(e.clone()).tls(files)
            }
            (Route::Listen(e), None) => SessionConfig::new().listen(e.clone()),
            (Route::Listen(_), Some(_)) => {
                return Err(Error::Io(std::io::Error::other(
                    "a Zenoh mTLS identity applies only to a connection (--zenoh)",
                )));
            }
        }
        .build()
        .map_err(zerr)?;
        let z = rt
            .block_on(ZenohTransport::open(config))
            .map_err(zerr)?
            .with_timeout(QUERY_TIMEOUT);
        Ok(ZenohLink {
            rt,
            z,
            node,
            timeout: QUERY_TIMEOUT,
        })
    }

    /// The board this link addresses.
    pub const fn node(&self) -> SenderId {
        self.node
    }

    /// Asks `sys/info` until the board answers (it may still be
    /// connecting) or `within` passes.
    pub fn wait_for_board(&mut self, within: Duration) -> Result<(), Error> {
        let end = Instant::now() + within;
        loop {
            match self.request(middleware::loader::keys::INFO, &[]) {
                Ok(_) => return Ok(()),
                Err(Error::Timeout { .. }) if Instant::now() < end => {
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(e) => return Err(e),
            }
        }
    }
}

fn zerr(e: ZenohError) -> Error {
    Error::Io(std::io::Error::other(format!("zenoh: {e:?}")))
}

impl Endpoint for ZenohLink {
    fn request(&mut self, key: Key<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
        let opts = RequestOptions::DEFAULT
            .with_target(Target::Node(self.node))
            .with_timeout(self.timeout);
        let mut out = vec![0u8; 4096];
        let r = self
            .rt
            .block_on(self.z.request_with(key, body, &mut out, &opts));
        match r {
            Ok(reply) => Ok(reply.to_vec()),
            Err(ZenohError::Handler(status)) => Err(Error::Device {
                key: key.as_str().to_owned(),
                status,
            }),
            Err(ZenohError::NoReply) => Err(Error::Timeout {
                key: key.as_str().to_owned(),
                attempts: 1,
            }),
            Err(ZenohError::BufferTooSmall { .. }) => Err(Error::Device {
                key: key.as_str().to_owned(),
                status: HandlerError::BufferTooSmall,
            }),
            Err(e) => Err(zerr(e)),
        }
    }

    fn take_events(&mut self) -> Vec<Event> {
        Vec::new()
    }

    fn next_event(&mut self, deadline: Instant) -> Result<Option<Event>, Error> {
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        Ok(None)
    }

    fn chunk_size(&self) -> usize {
        middleware::loader::CHUNK_MAX
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pn27_the_control_target_comes_from_sys_info() {
        use middleware::zenoh::wire::posix_sender_id;
        // A posix-node: its key carries its session id, derived from the
        // node id sys/info reports.
        let reported = parse_node("5e110de02a9c4107").unwrap();
        let addressed = posix_sender_id(reported);
        let first6 = [0x5e, 0x11, 0x0d, 0xe0, 0x2a, 0x9c];
        assert_eq!(
            target_for("linux-aarch64", addressed, Some(reported)),
            Ok(first6)
        );
        assert_eq!(
            target_for("linux-x86_64", addressed, Some(reported)),
            Ok(first6)
        );
        // An RT1050: its Ethernet MAC, 02 and the last five of its id,
        // whatever it reports.
        let rt = parse_node("310441d265f82484").unwrap();
        assert_eq!(target_for("rt1050", rt, Some(rt)), Ok(target(rt)));
        assert_eq!(target_for("rt1050", rt, Some(reported)), Ok(target(rt)));
        // No node id in the reply (older firmware): the RT1050 rule.
        assert_eq!(
            target_for("linux-aarch64", addressed, None),
            Ok(target(addressed))
        );
    }

    #[test]
    fn pn27_a_node_id_the_board_cannot_have_is_refused() {
        use middleware::zenoh::wire::posix_sender_id;
        // A responder at some id claiming another machine's node id: signing
        // for that machine would let it replay the request there.
        let victim = parse_node("5e110de02a9c4107").unwrap();
        let rogue = parse_node("7e729ac3f058a2e8").unwrap();
        assert_ne!(rogue, posix_sender_id(victim));
        let e = target_for("linux-aarch64", rogue, Some(victim)).expect_err("refused");
        assert_eq!(
            e,
            TargetMismatch {
                reported: victim,
                expected: posix_sender_id(victim),
                target: [0x5e, 0x11, 0x0d, 0xe0, 0x2a, 0x9c]
            }
        );
        let text = e.to_string();
        assert!(
            text.contains("5e110de02a9c4107")
                && text.contains("5e:11:0d:e0:2a:9c")
                && text.contains("--target"),
            "{text}"
        );
    }

    #[test]
    fn node_ids_and_rt_targets() {
        let n = parse_node("310441d265f82484").unwrap();
        assert_eq!(n.0, [0x31, 0x04, 0x41, 0xd2, 0x65, 0xf8, 0x24, 0x84]);
        assert_eq!(target(n), [0x02, 0xd2, 0x65, 0xf8, 0x24, 0x84]);
        for bad in [
            "",
            "310441d265f8248",
            "310441d265f824845",
            "zz0441d265f82484",
        ] {
            assert_eq!(parse_node(bad), None, "{bad}");
        }
    }
}
