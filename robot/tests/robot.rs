//! The robot end to end, in process. The "local Envoy" is a direct
//! loopback hop: the static endpoint dials an in-process gRPC echo
//! service on 127.0.0.1 (the address a real robot gives its Envoy's h2c
//! listener), and `MW_ZENOH_ROUTER` points at an in-process Zenoh session
//! listening on one explicit 127.0.0.1 port (reserved by binding port 0,
//! never probed), standing for the Envoy TCP listener to zenohd.

use std::net::Ipv4Addr;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use middleware::grpc::server::serve;
use middleware::grpc::{GrpcServer, responder};
use middleware::key::{Key, KeyFilter};
use middleware::transport::{HandlerError, Request, Responder, Subscribe, Subscriber, handler_fn};
use middleware::zenoh::{SessionConfig, ZenohTransport};
use mw_robot::{Robot, RobotConfig};
use mw_service::Config;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

const TIMEOUT: Duration = Duration::from_secs(10);

/// What the echo handler saw of one call: (traceparent, x-request-id).
type Seen = (Option<String>, Option<String>);

/// A gRPC echo service on 127.0.0.1:0 recording each call's trace context.
async fn echo_service() -> (String, std_mpsc::Receiver<Seen>, oneshot::Sender<()>) {
    let (svc, mut resp) = responder(8);
    let (seen_tx, seen) = std_mpsc::channel();
    tokio::spawn(async move {
        let mut handler = handler_fn(move |r: &Request<'_>, out: &mut [u8]| {
            let _ = seen_tx.send((
                r.meta.trace.map(|t| t.to_string()),
                r.meta.request_id.map(|i| i.as_str().to_owned()),
            ));
            let dst = out
                .get_mut(..r.body.len())
                .ok_or(HandlerError::BufferTooSmall)?;
            dst.copy_from_slice(r.body);
            Ok(r.body.len())
        });
        let mut scratch = [0u8; 512];
        while resp.serve_one(&mut handler, &mut scratch).await.is_ok() {}
    });
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (stop, stopped) = oneshot::channel::<()>();
    tokio::spawn(serve(
        GrpcServer::new().with_responder(svc),
        listener,
        async {
            let _ = stopped.await;
        },
    ));
    (addr.to_string(), seen, stop)
}

/// The field `name` of a telemetry object: the text after `"name":`, up
/// to the next `,` or `}`, without quotes.
fn field<'a>(json: &'a str, name: &str) -> Option<&'a str> {
    let at = json.find(&format!("\"{name}\":"))? + name.len() + 3;
    let rest = &json[at..];
    let end = rest.find([',', '}'])?;
    Some(rest[..end].trim_matches('"'))
}

#[tokio::test(flavor = "multi_thread")]
async fn v4_robot_calls_through_its_envoy_with_trace_and_publishes_state() {
    let (echo_addr, seen, _stop_echo) = echo_service().await;
    let port = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let router = format!("tcp/127.0.0.1:{port}");
    let mut cluster = ZenohTransport::open(
        SessionConfig::new()
            .listen(router.clone())
            .build()
            .expect("config"),
    )
    .await
    .expect("open router");
    let mut states = cluster
        .subscribe(KeyFilter::Exact(Key::from_static("app/robot/r1/state")))
        .await
        .expect("subscribe");

    let robot_env = |k: &str| match k {
        "MW_ROBOT_NAME" => Some("r1".to_owned()),
        "MW_ROBOT_PERIOD_MS" => Some("50".to_owned()),
        _ => None,
    };
    let robot = Robot::new(RobotConfig::from_env(&robot_env).expect("robot config")).expect("spec");
    assert_eq!(robot.service().spec().calls, ["demo/echo"]);
    assert_eq!(robot.service().spec().publishes, ["app/robot/r1/**"]);
    let endpoints = format!("demo/echo=http://{echo_addr}@demo-echo.apps.svc.cluster.local:50051");
    let runner = Config::from_env(&|k| match k {
        "MW_ZENOH_ROUTER" => Some(router.clone()),
        "MW_STATIC_ENDPOINTS" => Some(endpoints.clone()),
        _ => None,
    })
    .expect("runner config");
    let (stop, stopped) = oneshot::channel::<()>();
    let running = tokio::spawn(robot.run(runner, async {
        let _ = stopped.await;
    }));

    // Collect published states until three answered calls are reported.
    let mut buf = [0u8; 512];
    let mut published = Vec::new();
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let s = states.recv(&mut buf).await.expect("state");
            let json = std::str::from_utf8(s.payload).expect("utf-8").to_owned();
            let ok: u64 = field(&json, "echo_ok")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            published.push(json);
            if ok >= 3 {
                break;
            }
        }
    })
    .await
    .expect("three answered ticks");
    let _ = stop.send(());
    let report = running.await.expect("join").expect("robot ran");
    assert!(report.state.echo_ok >= 3, "{report:?}");
    assert_eq!(report.publish_failed, 0);

    // Every call the echo service answered carried the tick's trace and
    // request id, and the state published for that tick names the same
    // trace.
    let calls: Vec<Seen> = seen.try_iter().collect();
    assert!(calls.len() >= 3, "{calls:?}");
    let mut matched = 0;
    for json in &published {
        let (Some(seq), Some(trace)) = (field(json, "seq"), field(json, "trace")) else {
            continue;
        };
        if field(json, "rtt_us") == Some("null") {
            continue;
        }
        let rid = format!("r1-{seq}");
        let call = calls
            .iter()
            .find(|(_, id)| id.as_deref() == Some(rid.as_str()));
        if let Some((Some(t), _)) = call {
            assert_eq!(
                t, trace,
                "tick {seq}: the handler's traceparent is the robot's span"
            );
            matched += 1;
        }
    }
    assert!(matched >= 1, "published {published:?}, calls {calls:?}");
    for (t, id) in &calls {
        let t = t.as_deref().expect("every call is traced");
        assert!(t.starts_with("00-") && t.ends_with("-01"), "{t}");
        assert!(id.as_deref().is_some_and(|i| i.starts_with("r1-")));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn v4_robot_without_a_static_endpoint_fails_at_start() {
    let robot = Robot::new(
        RobotConfig::from_env(&|k| (k == "MW_ROBOT_NAME").then(|| "r2".to_owned()))
            .expect("config"),
    )
    .expect("spec");
    // A service map entry is not reachable off cluster: no static endpoint.
    let runner = Config::from_env(&|k| {
        (k == "MW_SERVICE_MAP").then(|| "demo/echo=demo-echo.apps:grpc".to_owned())
    })
    .expect("runner config");
    let e = robot.run(runner, async {}).await.unwrap_err();
    assert!(
        matches!(e, mw_service::ServiceError::Unmapped(ref k) if k == "demo/echo"),
        "{e}"
    );
}
