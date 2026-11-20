//! Zenoh over mutual TLS (ZT2–ZT5): a client built with
//! [`SessionConfig::client`] and [`SessionConfig::tls`] against an
//! in-process router listening on `tls/127.0.0.1:<port>` with mTLS on.
//!
//! Network conduct: as in `tests/zenoh.rs`, multicast scouting and gossip
//! are off and every session talks only to one explicit loopback port
//! (reserved by binding port 0, never probed). Certificates are throwaway
//! rcgen ECDSA P-256 keys written to a per-test temporary directory that
//! is removed when the test ends.
#![cfg(feature = "zenoh")]

use std::path::{Path, PathBuf};
use std::time::Duration;

use middleware::zenoh::{SessionConfig, TlsFiles, ZenohError, ZenohTransport};
use middleware::{HandlerError, Key, Publisher, Request, Requester, Responder, Subscriber};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};

const TIMEOUT: Duration = Duration::from_secs(10);

/// Reserves a free loopback port by binding port 0 (no probing).
async fn free_loopback_port() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    l.local_addr().expect("addr").port()
}

/// A temporary directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(test: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("mw-zenoh-tls-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A certificate authority that can sign leaves.
struct Ca {
    pem: String,
    issuer: Issuer<'static, KeyPair>,
}

impl Ca {
    fn new(name: &str) -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("ca params");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.distinguished_name.push(DnType::CommonName, name);
        let key = KeyPair::generate().expect("ca key");
        let cert = params.self_signed(&key).expect("ca cert");
        Self {
            pem: cert.pem(),
            issuer: Issuer::new(params, key),
        }
    }

    /// A leaf certificate and its PKCS#8 key, both PEM.
    fn leaf(&self, name: &str, sans: &[&str], usage: ExtendedKeyUsagePurpose) -> (String, String) {
        let sans: Vec<String> = sans.iter().map(|s| (*s).to_owned()).collect();
        let mut params = CertificateParams::new(sans).expect("leaf params");
        params.distinguished_name.push(DnType::CommonName, name);
        params.extended_key_usages = vec![usage];
        let key = KeyPair::generate().expect("leaf key");
        let cert = params.signed_by(&key, &self.issuer).expect("leaf cert");
        (cert.pem(), key.serialize_pem())
    }
}

/// One identity on disk: CA, certificate and key files.
struct Identity {
    ca: PathBuf,
    cert: PathBuf,
    key: PathBuf,
}

impl Identity {
    fn files(&self) -> TlsFiles {
        TlsFiles {
            root_ca: self.ca.clone(),
            cert: self.cert.clone(),
            key: self.key.clone(),
        }
    }
}

/// The test PKI: a real CA with a router and a client leaf, and a rogue
/// CA with its own router and client leaves.
#[allow(dead_code)] // the rogue identities are for the refusal tests
struct Pki {
    _dir: TempDir,
    /// Router identity signed by the real CA (trusting the real CA for clients).
    server: Identity,
    /// Router identity signed by the rogue CA (trusting the real CA for clients).
    rogue_server: Identity,
    /// Client identity signed by the real CA, pinning the real CA.
    client: Identity,
    /// Client identity signed by the rogue CA, pinning the real CA.
    rogue_client: Identity,
    /// The real CA and the private keys' PEM text, to check error messages.
    key_pems: Vec<String>,
}

fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, contents).expect("write pem");
    path
}

impl Pki {
    fn new(test: &str) -> Self {
        let dir = TempDir::new(test);
        let d = dir.0.clone();
        let ca = Ca::new("zt-ca");
        let rogue = Ca::new("zt-rogue-ca");
        let server_sans = ["127.0.0.1", "localhost"];
        let (server_cert, server_key) = ca.leaf(
            "zt-router",
            &server_sans,
            ExtendedKeyUsagePurpose::ServerAuth,
        );
        let (rogue_server_cert, rogue_server_key) = rogue.leaf(
            "zt-rogue-router",
            &server_sans,
            ExtendedKeyUsagePurpose::ServerAuth,
        );
        let (client_cert, client_key) =
            ca.leaf("zt-client", &[], ExtendedKeyUsagePurpose::ClientAuth);
        let (rogue_client_cert, rogue_client_key) =
            rogue.leaf("zt-rogue-client", &[], ExtendedKeyUsagePurpose::ClientAuth);
        let ca_file = write(&d, "ca", &ca.pem);
        let identity = |prefix: &str, cert: &str, key: &str| Identity {
            ca: ca_file.clone(),
            cert: write(&d, &format!("{prefix}-cert"), cert),
            key: write(&d, &format!("{prefix}-key"), key),
        };
        Self {
            server: identity("server", &server_cert, &server_key),
            rogue_server: identity("rogue-server", &rogue_server_cert, &rogue_server_key),
            client: identity("client", &client_cert, &client_key),
            rogue_client: identity("rogue-client", &rogue_client_cert, &rogue_client_key),
            key_pems: vec![server_key, rogue_server_key, client_key, rogue_client_key],
            _dir: dir,
        }
    }
}

fn json_path(path: &Path) -> String {
    format!("\"{}\"", path.to_str().expect("utf-8 temp path"))
}

/// The router's configuration: mode `router`, listening on `endpoint`
/// with `identity`'s certificate, requiring client certificates that chain
/// to `identity.ca`.
fn router_config(endpoint: &str, identity: &Identity) -> ::zenoh::Config {
    let mut config = SessionConfig::new()
        .listen(endpoint)
        .build()
        .expect("config");
    let set = |config: &mut ::zenoh::Config, key: &str, value: &str| {
        config.insert_json5(key, value).expect(key);
    };
    set(&mut config, "mode", r#""router""#);
    set(
        &mut config,
        "transport/link/tls/listen_certificate",
        &json_path(&identity.cert),
    );
    set(
        &mut config,
        "transport/link/tls/listen_private_key",
        &json_path(&identity.key),
    );
    set(
        &mut config,
        "transport/link/tls/root_ca_certificate",
        &json_path(&identity.ca),
    );
    set(&mut config, "transport/link/tls/enable_mtls", "true");
    config
}

async fn router(endpoint: &str, identity: &Identity) -> ZenohTransport {
    tokio::time::timeout(
        TIMEOUT,
        ZenohTransport::open(router_config(endpoint, identity)),
    )
    .await
    .expect("router open timeout")
    .expect("router open")
}

/// A client-mode mTLS configuration for `endpoint` with `identity`.
fn client_config(endpoint: &str, identity: &Identity) -> ::zenoh::Config {
    SessionConfig::new()
        .client()
        .connect(endpoint)
        .tls(identity.files())
        .build()
        .expect("config")
}

async fn open_client(config: ::zenoh::Config) -> Result<ZenohTransport, ZenohError> {
    tokio::time::timeout(TIMEOUT, ZenohTransport::open(config))
        .await
        .expect("client open timeout (should succeed or fail, not hang)")
}

/// Echoes the request reversed.
fn reverse(r: &Request<'_>, resp: &mut [u8]) -> Result<usize, HandlerError> {
    let req = r.body;
    let out = resp
        .get_mut(..req.len())
        .ok_or(HandlerError::BufferTooSmall)?;
    out.copy_from_slice(req);
    out.reverse();
    Ok(req.len())
}

/// A put from `client` reaches `server`'s subscriber, and a get from
/// `client` is answered by `server`'s responder.
async fn exchange(server: &ZenohTransport, client: &mut ZenohTransport, test: &str) {
    // The responder outlives this call in its serving task, so the keys are
    // 'static (a few bytes leaked per test).
    let topic = Key::new(format!("test/{test}/topic").leak()).expect("key");
    let rpc_key = Key::new(format!("test/{test}/rpc").leak()).expect("key");

    // Declarations propagate asynchronously: publish until a sample arrives.
    let mut sub = server.subscriber(topic).await.expect("subscriber");
    let mut buf = [0u8; 64];
    let sample = tokio::time::timeout(TIMEOUT, async {
        loop {
            client.publish(topic, b"hello").await.expect("publish");
            let wait = tokio::time::timeout(Duration::from_millis(200), sub.recv(&mut buf));
            if let Ok(r) = wait.await {
                break r.map(|s| s.payload.to_vec());
            }
        }
    })
    .await
    .expect("put timeout")
    .expect("recv");
    assert_eq!(sample, b"hello");

    let mut responder = server.responder(rpc_key).await.expect("responder");
    let serving = tokio::spawn(async move {
        let mut scratch = [0u8; 64];
        let mut handler = reverse;
        while responder
            .serve_one(&mut handler, &mut scratch)
            .await
            .is_ok()
        {}
    });
    let mut resp = [0u8; 16];
    let out = tokio::time::timeout(TIMEOUT, async {
        loop {
            match client.request(rpc_key, b"xyz", &mut resp).await {
                Err(ZenohError::NoReply) => continue,
                other => break other.map(<[u8]>::to_vec),
            }
        }
    })
    .await
    .expect("get timeout")
    .expect("get");
    assert_eq!(out, b"zyx");
    serving.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn zt2_mtls_client_publishes_and_gets() {
    let pki = Pki::new("zt2");
    let endpoint = format!("tls/127.0.0.1:{}", free_loopback_port().await);
    let server = router(&endpoint, &pki.server).await;
    let mut client = open_client(client_config(&endpoint, &pki.client))
        .await
        .expect("mTLS client open")
        .with_timeout(Duration::from_millis(500));
    exchange(&server, &mut client, "zt2").await;
}
