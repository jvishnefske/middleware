//! The device over Zenoh (feature `zenoh`, off by default: the default
//! mwctl stays free of the cloud crates, `scripts/check-features.sh`).
//!
//! [`ZenohLink`] is an [`Endpoint`] that sends every request as a Zenoh
//! query on the board's per-node key (`@mw/node/<node>/<key>`,
//! `Target::Node`), so exactly one board answers. The host session is a
//! Zenoh peer that either connects to an endpoint (a router, or a board's
//! peer listener) or listens for boards that connect to it (the RT1050
//! base firmware is a Zenoh client of its `sys/zenoh/endpoint`, often the
//! host itself).
//!
//! A board on the network accepts only what its Zenoh gate allows
//! (`firmware-imxrt/src/zenoh_mgmt.rs`, ZN5): uploads and firmware
//! updates with signed commits, and start/stop and firmware
//! reboot/confirm/abort only as signed control requests addressed to the
//! board's [`target`] address. Monitoring is not offered here
//! ([`Endpoint::next_event`] only waits).

use crate::error::Error;
use crate::link::{Endpoint, Event};
use middleware::Key;
use middleware::transport::{HandlerError, RequestOptions, Requester, SenderId, Target};
use middleware::zenoh::{ClientTls, SessionConfig, ZenohError, ZenohTransport};
use std::time::{Duration, Instant};

/// How long one query may take: an RT1050 erases a 256 KiB HyperFlash
/// block (about 2 s) inside some requests.
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(8);

/// How long `sys/os/commit` may take: a posix-node reads the written slot
/// back to hash it (a few GB from a USB stick) before it answers, and waits
/// as long for its controller (posix-node `sysnode::DIRECT_WAIT_MAX`).
pub const OS_COMMIT_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// The wait for one query on `key` when the link's own is `base`.
pub fn query_timeout(base: Duration, key: Key<'_>) -> Duration {
    if key == middleware::os_ab::wire::keys::COMMIT {
        base.max(OS_COMMIT_TIMEOUT)
    } else {
        base
    }
}

/// How the host session reaches the board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Connect to this endpoint (`tcp/<ip>:<port>`).
    Connect(String),
    /// Listen on this endpoint for boards that connect.
    Listen(String),
}

/// The mutual-TLS files in `dir`, named as the mesh CA writes them: `ca`
/// (the router's CA), `cert` and `key` (this client's, PEM, no extensions).
pub fn tls_from_dir(dir: &std::path::Path) -> Result<ClientTls, Error> {
    let file = |name: &str| -> Result<std::path::PathBuf, Error> {
        let p = dir.join(name);
        if p.is_file() {
            Ok(p)
        } else {
            Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{}: no {name} file (ca, cert and key are needed)", dir.display()),
            )))
        }
    };
    Ok(ClientTls {
        ca: file("ca")?,
        cert: file("cert")?,
        key: file("key")?,
    })
}

/// The 6-byte address a signed control request names on an RT1050 board:
/// its Ethernet MAC, `02` and the node id's last five bytes
/// (`firmware-imxrt/src/enet/mac.rs`, `mac_from_uid`).
pub fn target(node: SenderId) -> [u8; 6] {
    let n = node.0;
    [0x02, n[3], n[4], n[5], n[6], n[7]]
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
    /// Opens the host session on `route` for the board `node`.
    /// `tls` presents the client certificate for mutual TLS (`tls/` routers).
    pub fn open(route: &Route, node: SenderId, tls: Option<ClientTls>) -> Result<Self, Error> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(Error::Io)?;
        let mut session = match route {
            // A client: through a router (Zenoh 1.10 does not relay between
            // two peers of one router), and still fine to a node's listener.
            Route::Connect(e) => SessionConfig::new().client().connect(e.clone()),
            Route::Listen(e) => SessionConfig::new().listen(e.clone()),
        };
        if let Some(t) = tls {
            session = session.client_tls(t);
        }
        let config = session.build().map_err(zerr)?;
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

    /// Waits `timeout` for each query (`mwctl --timeout`), never less than
    /// [`QUERY_TIMEOUT`]; `sys/os/commit` waits at least
    /// [`OS_COMMIT_TIMEOUT`] ([`query_timeout`]).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout.max(QUERY_TIMEOUT);
        self
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
            .with_timeout(query_timeout(self.timeout, key));
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
    #[test]
    fn an_os_commit_waits_for_the_slot_to_be_hashed() {
        use super::{OS_COMMIT_TIMEOUT, QUERY_TIMEOUT, query_timeout};
        use middleware::os_ab::wire::keys as os;
        assert_eq!(query_timeout(QUERY_TIMEOUT, os::COMMIT), OS_COMMIT_TIMEOUT);
        assert_eq!(query_timeout(QUERY_TIMEOUT, os::CHUNK), QUERY_TIMEOUT);
        assert_eq!(query_timeout(QUERY_TIMEOUT, os::INFO), QUERY_TIMEOUT);
        let long = OS_COMMIT_TIMEOUT * 2;
        assert_eq!(
            query_timeout(long, os::COMMIT),
            long,
            "--timeout above it wins"
        );
    }

    use super::*;

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

    #[test]
    fn tls_dir_needs_ca_cert_and_key() {
        let dir = std::env::temp_dir().join(format!("mwctl-tls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["ca", "cert"] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        assert!(tls_from_dir(&dir).is_err(), "no key");
        std::fs::write(dir.join("key"), b"x").unwrap();
        let t = tls_from_dir(&dir).unwrap();
        assert_eq!(t.ca, dir.join("ca"));
        assert_eq!(t.cert, dir.join("cert"));
        assert_eq!(t.key, dir.join("key"));
        let _ = std::fs::remove_dir_all(&dir);
    }

}
