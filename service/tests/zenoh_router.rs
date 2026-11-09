//! The rendered router config enforced by a real zenoh 1.10 router, in
//! process, on a 127.0.0.1 port reserved by binding port 0 (no probing):
//! usrpwd authentication refuses bad credentials, the access-control rules
//! derived from the specs let declared traffic through and drop the rest,
//! and discovery liveliness works for every principal while only the
//! server of a kind may announce it (design.md Z19, Z20). Also pins the
//! generated `deploy/zenoh/` output (Z22): `MW_BLESS=1 cargo test -p
//! mw-service --test zenoh_router` rewrites it.

use std::path::PathBuf;
use std::time::Duration;

use middleware::discovery::{Announce, Browse, Endpoint, Event, Service, ServiceKind};
use middleware::key::{Key, KeyFilter};
use middleware::transport::{
    HandlerError, Publisher, Request, RequestOptions, Requester, Responder, Subscribe, Subscriber,
    Target,
};
use middleware::zenoh::{ZenohError, ZenohTransport};
use mw_service::ServiceSpec;
use mw_service::zenoh_router::config::client_config;
use mw_service::zenoh_router::credentials::{Credentials, Password};
use mw_service::zenoh_router::{RouterOptions, plan, render_router};

const TIMEOUT: Duration = Duration::from_secs(5);
const ECHO: ServiceKind<'static> = ServiceKind::from_static("demo/echo");

/// A temporary directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "mw-zenoh-router-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn password(tag: &str) -> Password {
    Password::new(format!("pw-{tag}-0123456789abcdef")).expect("password")
}

fn creds(user: &str) -> Credentials {
    Credentials::new(user, password(user)).expect("credentials")
}

/// `demo/echo` serves its kind and publishes `app/demo/**`; `demo/caller`
/// calls `demo/echo` and subscribes to `app/demo/**`; `ops/admin`
/// publishes `sys/**`.
fn specs() -> Vec<ServiceSpec> {
    let mut echo = ServiceSpec::new("demo/echo");
    echo.serves.push("demo/echo".into());
    echo.publishes.push("app/demo/**".into());
    let mut caller = ServiceSpec::new("demo/caller");
    caller.calls.push("demo/echo".into());
    caller.subscribes.push("app/demo/**".into());
    let mut admin = ServiceSpec::new("ops/admin");
    admin.publishes.push("sys/**".into());
    vec![echo, caller, admin]
}

async fn free_loopback_port() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    l.local_addr().expect("addr").port()
}

struct Router {
    session: zenoh::Session,
    endpoint: String,
    _dir: TempDir,
}

/// Opens the rendered router in process.
async fn router(tag: &str) -> Router {
    let dir = TempDir::new(tag);
    let dictionary_file = dir.0.join("dictionary");
    let port = free_loopback_port().await;
    let options = RouterOptions {
        listen_host: "127.0.0.1".into(),
        port,
        dictionary_file: dictionary_file.display().to_string(),
        ..RouterOptions::default()
    };
    let all = [creds("demo-caller"), creds("demo-echo"), creds("ops-admin")];
    let bundle = render_router(&specs(), &options, &all).expect("render");
    std::fs::write(&dictionary_file, &bundle.dictionary).expect("dictionary");
    let config = zenoh::Config::from_json5(&bundle.plan.config_json5).expect("config");
    let session = zenoh::open(config).await.expect("open router");
    Router {
        session,
        endpoint: format!("tcp/127.0.0.1:{port}"),
        _dir: dir,
    }
}

async fn client(endpoint: &str, credentials: &Credentials) -> zenoh::Result<ZenohTransport> {
    let config = client_config(endpoint, credentials).expect("client config");
    zenoh::open(config).await.map(ZenohTransport::new)
}

async fn connect(r: &Router, user: &str) -> ZenohTransport {
    client(&r.endpoint, &creds(user)).await.expect("client")
}

async fn recv_key(sub: &mut impl Subscriber) -> String {
    let mut buf = [0u8; 64];
    let sample = tokio::time::timeout(TIMEOUT, sub.recv(&mut buf))
        .await
        .expect("timeout")
        .expect("recv");
    sample.key.as_str().to_owned()
}

/// Publishes `key` with a few retries until `sub` sees `key` (declarations
/// propagate asynchronously), returning what arrived first.
async fn publish_until_seen(
    publisher: &mut ZenohTransport,
    key: Key<'_>,
    sub: &mut impl Subscriber,
) -> String {
    for _ in 0..50 {
        publisher.publish(key, b"x").await.expect("publish");
        let mut buf = [0u8; 64];
        if let Ok(Ok(sample)) =
            tokio::time::timeout(Duration::from_millis(100), sub.recv(&mut buf)).await
        {
            return sample.key.as_str().to_owned();
        }
    }
    panic!("{key} never arrived");
}

#[tokio::test(flavor = "multi_thread")]
async fn z20_wrong_unknown_or_missing_credentials_are_refused() {
    let r = router("auth").await;
    let wrong = Credentials::new("demo-echo", password("wrong")).unwrap();
    assert!(client(&r.endpoint, &wrong).await.is_err(), "wrong password");
    let stranger = Credentials::new("stranger", password("stranger")).unwrap();
    assert!(
        client(&r.endpoint, &stranger).await.is_err(),
        "unknown user"
    );
    let mut anonymous = zenoh::Config::default();
    anonymous.insert_json5("mode", r#""client""#).unwrap();
    anonymous
        .insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    anonymous
        .insert_json5("connect/endpoints", &format!(r#"["{}"]"#, r.endpoint))
        .unwrap();
    assert!(zenoh::open(anonymous).await.is_err(), "no credentials");
    assert!(client(&r.endpoint, &creds("demo-echo")).await.is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn z19_declared_puts_arrive_and_undeclared_puts_are_dropped() {
    let r = router("acl").await;
    let mut echo = connect(&r, "demo-echo").await;
    let mut caller = connect(&r, "demo-caller").await;
    let mut admin = connect(&r, "ops-admin").await;
    // The router's own session is not a link: it sees whatever got in.
    let mut observer = ZenohTransport::new(r.session.clone());
    let mut seen = observer
        .subscribe(KeyFilter::Prefix(Key::from_static("app")))
        .await
        .expect("observer");
    let mut sys_seen = observer
        .subscribe(KeyFilter::Prefix(Key::from_static("sys")))
        .await
        .expect("observer");
    let mut demo = caller
        .subscribe(KeyFilter::Prefix(Key::from_static("app/demo")))
        .await
        .expect("subscribe");
    // A subscription outside its grant: declared, never fed.
    let mut sys_sub = caller
        .subscribe(KeyFilter::Prefix(Key::from_static("sys")))
        .await
        .expect("subscribe");

    // Allowed: echo → app/demo/x reaches the caller (and the router).
    let first = publish_until_seen(&mut echo, Key::from_static("app/demo/x"), &mut demo).await;
    assert_eq!(first, "app/demo/x");
    assert_eq!(recv_key(&mut seen).await, "app/demo/x");
    while let Ok(Some(_)) = seen.try_recv(&mut [0u8; 64]) {}
    while let Ok(Some(_)) = demo.try_recv(&mut [0u8; 64]) {}

    // Refused at ingress: echo may not publish outside app/demo/**. One
    // link keeps order, so the sentinel arriving first proves the drop.
    echo.publish(Key::from_static("app/other"), b"x")
        .await
        .unwrap();
    echo.publish(Key::from_static("sys/reboot"), b"x")
        .await
        .unwrap();
    echo.publish(Key::from_static("app/demo/sentinel"), b"x")
        .await
        .unwrap();
    assert_eq!(recv_key(&mut seen).await, "app/demo/sentinel");
    assert_eq!(recv_key(&mut demo).await, "app/demo/sentinel");

    // Refused at egress: admin's sys/reboot gets into the router but not
    // to the caller, whose sys/** subscription is outside its grant.
    let got = publish_until_seen(&mut admin, Key::from_static("sys/reboot"), &mut sys_seen).await;
    assert_eq!(got, "sys/reboot");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        matches!(sys_sub.try_recv(&mut [0u8; 64]), Ok(None)),
        "egress denied"
    );
    // The caller may not publish at all.
    let mut caller_pub = caller.clone();
    caller_pub
        .publish(Key::from_static("app/demo/forged"), b"x")
        .await
        .unwrap();
    echo.publish(Key::from_static("app/demo/sentinel2"), b"x")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut keys = Vec::new();
    let mut buf = [0u8; 64];
    while let Ok(Some(sample)) = seen.try_recv(&mut buf) {
        keys.push(sample.key.as_str().to_owned());
    }
    assert_eq!(keys, ["app/demo/sentinel2"], "no forged put");
}

fn echo_service(instance: &'static str, txt: &'static [u8]) -> Service<'static> {
    Service {
        kind: ECHO,
        instance,
        node: None,
        endpoint: Endpoint::Locator("tcp/127.0.0.1:7447"),
        txt,
        ttl_s: 60,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn z19_discovery_works_for_all_and_only_the_server_announces() {
    let r = router("disc").await;
    let echo = connect(&r, "demo-echo").await;
    let caller = connect(&r, "demo-caller").await;
    let admin = connect(&r, "ops-admin").await;

    // ops/admin tries to forge demo/echo first: its token and record
    // queryable are refused at the router, so no browser sees a conflict
    // (without the ACL this is Z18's ConflictingRecords).
    let mut forger = admin.discovery();
    forger
        .announce(&echo_service("pod-1", b"\x06evil=1"))
        .await
        .expect("declared locally");
    let mut announcer = echo.discovery();
    announcer
        .announce(&echo_service("pod-1", b"\x05ver=1"))
        .await
        .expect("announce");

    for (who, t) in [("caller", &caller), ("echo", &echo)] {
        let mut browser = t.discovery();
        browser
            .browse(KeyFilter::Exact(ECHO.key()))
            .await
            .expect("browse");
        let mut buf = [0u8; 256];
        let e = tokio::time::timeout(TIMEOUT, browser.next(&mut buf))
            .await
            .expect("timeout")
            .unwrap_or_else(|e| panic!("{who}: {e:?}"));
        let Event::Up(s) = e else {
            panic!("{who}: {e:?}")
        };
        assert_eq!((s.instance, s.txt), ("pod-1", &b"\x05ver=1"[..]), "{who}");
    }
}

/// Answers `demo/echo` with `name` from `t` until the task is dropped.
async fn serve(t: &ZenohTransport, name: &'static [u8]) -> tokio::task::JoinHandle<()> {
    let mut responder = t.responder(ECHO.key()).await.expect("responder");
    tokio::spawn(async move {
        let mut handler = move |_r: &Request<'_>, out: &mut [u8]| {
            let out = out
                .get_mut(..name.len())
                .ok_or(HandlerError::BufferTooSmall)?;
            out.copy_from_slice(name);
            Ok(name.len())
        };
        let mut scratch = [0u8; 64];
        while responder
            .serve_one(&mut handler, &mut scratch)
            .await
            .is_ok()
        {}
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn z19_only_callers_query_and_only_the_server_answers() {
    let r = router("rpc").await;
    let echo = connect(&r, "demo-echo").await;
    let mut caller = connect(&r, "demo-caller").await;
    let admin = connect(&r, "ops-admin").await;
    let _echo = serve(&echo, b"echo").await;
    // ops/admin squats on demo/echo: its queryables are refused.
    let _squat = serve(&admin, b"squat").await;

    let opts = RequestOptions::DEFAULT.with_timeout(Duration::from_millis(500));
    let mut out = [0u8; 16];
    let mut answered = None;
    for _ in 0..20 {
        if let Ok(got) = caller.request_with(ECHO.key(), b"?", &mut out, &opts).await {
            answered = Some(got.to_vec());
            break;
        }
    }
    assert_eq!(answered.as_deref(), Some(&b"echo"[..]));
    for target in [Target::Any, Target::Node(echo.sender_id())] {
        for _ in 0..5 {
            let got = caller
                .request_with(ECHO.key(), b"?", &mut out, &opts.with_target(target))
                .await
                .expect("request");
            assert_eq!(got, b"echo", "never the squatter ({target:?})");
        }
    }
    // A principal that does not call demo/echo may not query it (from a
    // second session: its own squatting queryable would answer locally).
    let mut admin2 = connect(&r, "ops-admin").await;
    let r = admin2.request_with(ECHO.key(), b"?", &mut out, &opts).await;
    assert!(matches!(r, Err(ZenohError::NoReply)), "{r:?}");
}

/// `deploy/zenoh/`, relative to this crate.
fn deploy_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/zenoh")
}

#[test]
fn z22_deploy_zenoh_is_the_rendered_output() {
    let dir = deploy_dir();
    let options: RouterOptions =
        serde_norway::from_str(&std::fs::read_to_string(dir.join("router.yaml")).unwrap())
            .expect("router.yaml");
    let mut spec_files: Vec<PathBuf> = std::fs::read_dir(dir.join("specs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    spec_files.sort();
    let specs: Vec<ServiceSpec> = spec_files
        .iter()
        .map(|p| serde_norway::from_str(&std::fs::read_to_string(p).unwrap()).expect("spec"))
        .collect();
    let plan = plan(&specs, &options).expect("plan");
    let files = [
        ("config.json5", &plan.config_json5),
        ("zenohd.yaml", &plan.k8s_manifests_yaml),
    ];
    if std::env::var_os("MW_BLESS").is_some() {
        for (name, text) in files {
            std::fs::write(dir.join(name), text).unwrap();
        }
    }
    for (name, text) in files {
        let committed = std::fs::read_to_string(dir.join(name)).unwrap();
        assert!(
            &committed == text,
            "deploy/zenoh/{name} is stale: regenerate with MW_BLESS=1"
        );
    }
    zenoh::Config::from_json5(&plan.config_json5).expect("zenoh accepts the golden config");
    let users: Vec<&str> = plan.principals.iter().map(|p| p.user.as_str()).collect();
    assert_eq!(
        users,
        ["demo-caller", "demo-echo", "mw-gateway", "robot-r1"]
    );
}
