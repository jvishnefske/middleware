//! Zenoh over `wss://` through a TLS-terminating proxy (feature `zenoh-ws`,
//! design.md WS1–WS3).
//!
//! Zenoh 1.10 builds a `ws/` link, but its client dials plain `ws://` to a
//! resolved IP: no TLS, no path, and the IP in the `Host` header. So a stock
//! session cannot reach `wss://updates.example.com/zenoh` behind Traefik,
//! which routes by host name and ends TLS. The router side is fine: zenohd
//! listens on `ws/0.0.0.0:7448` and Traefik forwards the upgraded request
//! to it.
//!
//! [`WsBridge`] closes the gap inside the client process. It listens on
//! `tcp/127.0.0.1:<ephemeral>` ([`WsBridge::locator`]); the Zenoh session
//! connects there with its ordinary TCP link, and for each connection the
//! bridge dials the [`WsTarget`] (TLS with the host's SNI and the webpki
//! roots, the right `Host` header and path) and converts the framing:
//!
//! * Zenoh's TCP link is *streamed*: every batch is `[len u16 LE][batch]`.
//! * Its WS link is not: every batch is one binary WebSocket message, with
//!   no length prefix.
//!
//! So a TCP batch becomes one binary message and a binary message one TCP
//! batch ([`read_batch`], [`write_batch`]); a message longer than 65535
//! bytes cannot be a batch and ends the connection. The bridge sends no
//! WebSocket pings: Zenoh's WS reader fails the link on any message that is
//! not binary, and Zenoh's own keep-alive already keeps the link busy.
//! Pings from the server are answered with pongs by the WebSocket layer and
//! never reach the TCP side.
//!
//! Every byte above TLS is Zenoh's own, unmodified: authentication
//! (user/password), the router's ACL and Zenoh's keep-alive and lease work
//! exactly as over TCP.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::{self, ClientConfig, RootCertStore, pki_types::ServerName};
use tokio_tungstenite::tungstenite::Message;

/// Largest Zenoh batch (its `BatchSize` is a u16).
pub const MAX_BATCH: usize = u16::MAX as usize;

/// Where the bridge dials.
#[derive(Clone)]
pub struct WsTarget {
    /// `wss://host[:port]/path` or `ws://host[:port]/path`.
    pub url: String,
    /// Connect to this address instead of resolving the URL's host (the
    /// TLS name and `Host` header stay the URL's). For a LAN without
    /// hairpin NAT, or a test.
    pub connect_addr: Option<SocketAddr>,
    /// Trust anchors for `wss://`; `None` uses the webpki roots.
    pub roots: Option<Arc<RootCertStore>>,
}

impl core::fmt::Debug for WsTarget {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WsTarget")
            .field("url", &self.url)
            .field("connect_addr", &self.connect_addr)
            .field("custom_roots", &self.roots.is_some())
            .finish()
    }
}

impl WsTarget {
    /// `wss://<host>/zenoh` with the webpki roots: the updates endpoint
    /// for a host name such as `updates.example.com`.
    pub fn wss(host: &str) -> Self {
        WsTarget {
            url: format!("wss://{host}/zenoh"),
            connect_addr: None,
            roots: None,
        }
    }
}

/// The parts of a target URL the bridge needs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Parsed {
    tls: bool,
    host: String,
    port: u16,
}

fn parse(url: &str) -> io::Result<Parsed> {
    let bad = || io::Error::new(io::ErrorKind::InvalidInput, format!("bad ws url {url}"));
    let (tls, rest) = if let Some(r) = url.strip_prefix("wss://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("ws://") {
        (false, r)
    } else {
        return Err(bad());
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => {
            (h, p.parse::<u16>().map_err(|_| bad())?)
        }
        _ => (authority, if tls { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err(bad());
    }
    Ok(Parsed {
        tls,
        host: host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned(),
        port,
    })
}

/// Reads one `[len u16 LE][batch]` from Zenoh's TCP link into `buf`;
/// `Ok(false)` on a clean end of stream before a length.
pub async fn read_batch<R: AsyncRead + Unpin>(r: &mut R, buf: &mut Vec<u8>) -> io::Result<bool> {
    let mut len = [0u8; 2];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
        Err(e) => return Err(e),
    }
    buf.resize(u16::from_le_bytes(len) as usize, 0);
    r.read_exact(buf).await?;
    Ok(true)
}

/// Writes `batch` as `[len u16 LE][batch]`; a batch over [`MAX_BATCH`]
/// bytes is `InvalidData`.
pub async fn write_batch<W: AsyncWrite + Unpin>(w: &mut W, batch: &[u8]) -> io::Result<()> {
    let len = u16::try_from(batch.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("websocket message of {} bytes is not a batch", batch.len()),
        )
    })?;
    let mut frame = Vec::with_capacity(2 + batch.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(batch);
    w.write_all(&frame).await?;
    w.flush().await
}

/// A running bridge: a loopback TCP listener whose every connection is
/// carried to the [`WsTarget`]. Dropping it stops accepting (connections
/// already bridged run until either side closes).
#[derive(Debug)]
pub struct WsBridge {
    local: SocketAddr,
    task: JoinHandle<()>,
}

impl Drop for WsBridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl WsBridge {
    /// Binds `127.0.0.1:0` and starts bridging to `target`. The URL is
    /// checked now; the target is dialled per connection.
    pub async fn start(target: WsTarget) -> io::Result<WsBridge> {
        let parsed = parse(&target.url)?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let local = listener.local_addr()?;
        let tls = if parsed.tls {
            Some(connector(target.roots.clone()))
        } else {
            None
        };
        let target = Arc::new((target, parsed, tls));
        let task = tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    continue;
                };
                let target = target.clone();
                tokio::spawn(async move {
                    let (t, p, tls) = &*target;
                    // The library logs nothing of its own: a failed dial
                    // shows up as the session's link failing, which Zenoh
                    // reports and retries.
                    let _ = bridge_one(tcp, t, p, tls.as_ref()).await;
                });
            }
        });
        Ok(WsBridge { local, task })
    }

    /// The loopback address the session connects to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// The Zenoh endpoint of [`WsBridge::local_addr`]: `tcp/127.0.0.1:<port>`.
    pub fn locator(&self) -> String {
        format!("tcp/{}", self.local)
    }
}

fn connector(roots: Option<Arc<RootCertStore>>) -> TlsConnector {
    let roots = roots.unwrap_or_else(|| {
        let mut r = RootCertStore::empty();
        r.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Arc::new(r)
    });
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}

fn other<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(e.to_string())
}

async fn bridge_one(
    tcp: TcpStream,
    target: &WsTarget,
    parsed: &Parsed,
    tls: Option<&TlsConnector>,
) -> io::Result<()> {
    tcp.set_nodelay(true)?;
    let upstream = match target.connect_addr {
        Some(a) => TcpStream::connect(a).await?,
        None => TcpStream::connect((parsed.host.as_str(), parsed.port)).await?,
    };
    upstream.set_nodelay(true)?;
    match tls {
        Some(c) => {
            let name = ServerName::try_from(parsed.host.clone()).map_err(other)?;
            let s = c.connect(name, upstream).await?;
            relay(tcp, s, &target.url).await
        }
        None => relay(tcp, upstream, &target.url).await,
    }
}

/// Upgrades `upstream` to a WebSocket at `url` and carries batches between
/// it and Zenoh's TCP link `tcp` until either side ends.
async fn relay<S>(tcp: TcpStream, upstream: S, url: &str) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (ws, _) = tokio_tungstenite::client_async(url, upstream)
        .await
        .map_err(other)?;
    let (mut ws_tx, mut ws_rx) = ws.split();
    let (mut tcp_rx, mut tcp_tx) = tcp.into_split();
    let up = async {
        let mut buf = Vec::with_capacity(MAX_BATCH);
        while read_batch(&mut tcp_rx, &mut buf).await? {
            ws_tx
                .send(Message::Binary(buf.clone()))
                .await
                .map_err(other)?;
        }
        let _ = ws_tx.send(Message::Close(None)).await;
        Ok::<_, io::Error>(())
    };
    let down = async {
        while let Some(m) = ws_rx.next().await {
            match m.map_err(other)? {
                Message::Binary(b) => write_batch(&mut tcp_tx, &b).await?,
                Message::Close(_) => break,
                // Pongs are queued by tungstenite; nothing else is Zenoh's.
                Message::Ping(_) | Message::Pong(_) => {}
                Message::Text(_) | Message::Frame(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "non-binary websocket message",
                    ));
                }
            }
        }
        let _ = tcp_tx.shutdown().await;
        Ok::<_, io::Error>(())
    };
    tokio::select! {
        r = up => r,
        r = down => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws2_urls_parse() {
        let p = |u| parse(u).unwrap();
        assert_eq!(
            p("wss://updates.example.com/zenoh"),
            Parsed {
                tls: true,
                host: "updates.example.com".into(),
                port: 443
            }
        );
        assert_eq!(p("ws://127.0.0.1:7448/").port, 7448);
        assert!(!p("ws://h:1/x").tls);
        assert_eq!(p("ws://[::1]:9/z").host, "::1");
        assert!(parse("http://x/").is_err());
        assert!(parse("wss:///zenoh").is_err());
        assert!(parse("wss://h:x/").is_err());
    }

    #[tokio::test]
    async fn ws1_framing_survives_split_reads_max_batches_and_close() {
        let (mut a, mut b) = tokio::io::duplex(8);
        let big: Vec<u8> = (0..MAX_BATCH).map(|i| (i * 7) as u8).collect();
        let small = b"zenoh".to_vec();
        let (s, b2) = (small.clone(), big.clone());
        let writer = tokio::spawn(async move {
            write_batch(&mut a, &s).await.unwrap();
            write_batch(&mut a, &b2).await.unwrap();
            write_batch(&mut a, &[]).await.unwrap();
            // A torn frame: length, then half the body, then close.
            a.write_all(&10u16.to_le_bytes()).await.unwrap();
            a.write_all(&[1, 2, 3]).await.unwrap();
        });
        let mut buf = Vec::new();
        // The duplex moves at most 8 bytes per read: every batch is split.
        assert!(read_batch(&mut b, &mut buf).await.unwrap());
        assert_eq!(buf, small);
        assert!(read_batch(&mut b, &mut buf).await.unwrap());
        assert_eq!(buf, big);
        assert!(read_batch(&mut b, &mut buf).await.unwrap());
        assert!(buf.is_empty());
        writer.await.unwrap();
        let torn = read_batch(&mut b, &mut buf).await.unwrap_err();
        assert_eq!(torn.kind(), io::ErrorKind::UnexpectedEof);
        // A clean close between frames.
        let (a, mut b) = tokio::io::duplex(8);
        drop(a);
        assert!(!read_batch(&mut b, &mut buf).await.unwrap());
        // A message too long to be a batch.
        let (mut a, _b) = tokio::io::duplex(8);
        let e = write_batch(&mut a, &vec![0; MAX_BATCH + 1])
            .await
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }
}
