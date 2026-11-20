//! A session to the updates channel (UP6, UP7).
//!
//! [`Client::connect`] opens a Zenoh `client` session to the updates
//! router, either through [`WsBridge`] to `wss://<host>/zenoh` (Traefik
//! ends TLS, the router's `ws/` listener takes the rest), or directly to a
//! locator (`tcp/…`, a LAN router or a test). With credentials the link
//! authenticates by `usrpwd`; the router's ACL then decides what the
//! session may do ([`crate::router`]).

use std::net::SocketAddr;
use std::time::Duration;

use middleware::zenoh::wsbridge::{WsBridge, WsTarget};
use zenoh::Session;
use zenoh::sample::Sample;

use crate::keys::UpdateKey;
use crate::wire::{CHUNK, Release, decode_latest, encode_publish};

/// How long one query waits for its answer.
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(15);

/// How to reach the router.
#[derive(Debug, Clone)]
pub enum Route {
    /// `wss://<host>/zenoh` through the bridge, dialling `addr` instead of
    /// resolving the host when given (a LAN without hairpin NAT).
    Wss {
        /// The host name (TLS name, `Host` header).
        host: String,
        /// The address to dial instead.
        addr: Option<SocketAddr>,
    },
    /// A Zenoh locator, e.g. `tcp/127.0.0.1:7447`.
    Direct(String),
    /// A custom bridge target (tests: a private root).
    Bridge(WsTarget),
}

/// Why a client operation failed.
#[derive(Debug)]
pub enum ClientError {
    /// The session or a query failed.
    Zenoh(String),
    /// No or a malformed answer.
    Answer(String),
    /// The fetched image is not the release's.
    Image,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Zenoh(e) => write!(f, "zenoh: {e}"),
            ClientError::Answer(e) => write!(f, "answer: {e}"),
            ClientError::Image => write!(f, "the fetched image does not match its cert"),
        }
    }
}

impl std::error::Error for ClientError {}

fn z<E: std::fmt::Display>(e: E) -> ClientError {
    ClientError::Zenoh(e.to_string())
}

/// A session to the channel.
pub struct Client {
    session: Session,
    _bridge: Option<WsBridge>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").finish_non_exhaustive()
    }
}

/// A `client`-mode config connecting only to `locator`, with scouting off
/// and, when given, `usrpwd` credentials.
pub fn session_config(
    locator: &str,
    credentials: Option<&(String, String)>,
) -> Result<zenoh::Config, ClientError> {
    let mut c = zenoh::Config::default();
    let set = |c: &mut zenoh::Config, k: &str, v: String| c.insert_json5(k, &v).map_err(z);
    set(&mut c, "mode", "\"client\"".into())?;
    set(&mut c, "scouting/multicast/enabled", "false".into())?;
    set(&mut c, "scouting/gossip/enabled", "false".into())?;
    set(
        &mut c,
        "connect/endpoints",
        serde_json::json!([locator]).to_string(),
    )?;
    set(&mut c, "listen/endpoints", "[]".into())?;
    if let Some((user, password)) = credentials {
        set(
            &mut c,
            "transport/auth/usrpwd/user",
            serde_json::json!(user).to_string(),
        )?;
        set(
            &mut c,
            "transport/auth/usrpwd/password",
            serde_json::json!(password).to_string(),
        )?;
    }
    Ok(c)
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).into()
}

impl Client {
    /// Opens the session on `route`.
    pub async fn connect(
        route: &Route,
        credentials: Option<&(String, String)>,
    ) -> Result<Self, ClientError> {
        let (locator, bridge) = match route {
            Route::Direct(l) => (l.clone(), None),
            Route::Wss { host, addr } => {
                let mut t = WsTarget::wss(host);
                t.connect_addr = *addr;
                let b = WsBridge::start(t).await.map_err(z)?;
                (b.locator(), Some(b))
            }
            Route::Bridge(t) => {
                let b = WsBridge::start(t.clone()).await.map_err(z)?;
                (b.locator(), Some(b))
            }
        };
        let session = zenoh::open(session_config(&locator, credentials)?)
            .await
            .map_err(z)?;
        Ok(Client {
            session,
            _bridge: bridge,
        })
    }

    /// A client over an open session (the server's own, tests).
    pub fn from_session(session: Session) -> Self {
        Client {
            session,
            _bridge: None,
        }
    }

    /// The session.
    pub fn session(&self) -> &Session {
        &self.session
    }

    async fn get_one(&self, key: &str) -> Result<Vec<u8>, ClientError> {
        let replies = self
            .session
            .get(key)
            .timeout(QUERY_TIMEOUT)
            .await
            .map_err(z)?;
        let reply = replies
            .recv_async()
            .await
            .map_err(|_| ClientError::Answer(format!("no answer on {key}")))?;
        match reply.result() {
            Ok(s) => Ok(s.payload().to_bytes().to_vec()),
            Err(e) => Err(ClientError::Answer(format!(
                "{key}: {}",
                String::from_utf8_lossy(&e.payload().to_bytes())
            ))),
        }
    }

    /// The newest release of each kind on `channel` for `target`.
    pub async fn latest(&self, channel: &str, target: &str) -> Result<Vec<Release>, ClientError> {
        let key = UpdateKey::Latest {
            channel: channel.into(),
            target: target.into(),
        }
        .render();
        let body = self.get_one(&key).await?;
        decode_latest(&body).ok_or_else(|| ClientError::Answer(format!("malformed {key}")))
    }

    /// The image of `release`, fetched in chunks; its length and SHA-256
    /// are checked against the cert.
    pub async fn fetch(&self, release: &Release) -> Result<Vec<u8>, ClientError> {
        let c = release.cert();
        let mut image = Vec::with_capacity(c.len as usize);
        while image.len() < c.len as usize {
            let key = UpdateKey::Img {
                sha256: c.sha256,
                offset: image.len() as u32,
            }
            .render();
            let chunk = self.get_one(&key).await?;
            if chunk.is_empty() || chunk.len() > CHUNK {
                return Err(ClientError::Image);
            }
            image.extend_from_slice(&chunk);
        }
        if image.len() != c.len as usize || sha256(&image) != c.sha256 {
            return Err(ClientError::Image);
        }
        Ok(image)
    }

    /// Publishes `release` with `image` on its cert's channel and target.
    pub async fn publish(&self, release: &Release, image: &[u8]) -> Result<(), ClientError> {
        let c = release.cert();
        let key = UpdateKey::Publish {
            channel: c.channel.into(),
            target: c.target.into(),
        }
        .render();
        self.session
            .put(key, encode_publish(release, image))
            .await
            .map_err(z)
    }

    /// Reports `json` on `@mw/updates/status/<node>`.
    pub async fn status(&self, node: &str, json: String) -> Result<(), ClientError> {
        let key = UpdateKey::Status { node: node.into() }.render();
        self.session.put(key, json).await.map_err(z)
    }

    /// A subscriber to every `latest` of `channel`: a sample means a
    /// release was published.
    pub async fn watch(
        &self,
        channel: &str,
    ) -> Result<zenoh::pubsub::Subscriber<zenoh::handlers::FifoChannelHandler<Sample>>, ClientError>
    {
        self.session
            .declare_subscriber(crate::keys::latest_of_channel(channel))
            .await
            .map_err(z)
    }
}
