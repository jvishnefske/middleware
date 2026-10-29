//! Request/response cases (design.md K-requirements on `request_with`,
//! `HandlerError`, `RequestMeta` and `Gather`).

use std::time::{Duration, Instant};

use middleware::key::Key;
use middleware::{
    Flow, Gather, HandlerError, Request, RequestOptions, Requester, Responder, SenderId,
    transport::handler_fn,
};

use super::{GatherFixture, ReqRepFixture, prompt};

const KEY: Key<'static> = Key::from_static("conformance/echo");

/// Copies the request into the response.
fn echo(req: &Request<'_>, out: &mut [u8]) -> Result<usize, HandlerError> {
    let out = out
        .get_mut(..req.body.len())
        .ok_or(HandlerError::BufferTooSmall)?;
    out.copy_from_slice(req.body);
    Ok(req.body.len())
}

/// A request reaches the responder and its reply comes back.
pub async fn round_trip<F: ReqRepFixture>(f: &mut F) {
    let (mut req, mut resp) = f.connect(KEY).await;
    let mut h = handler_fn(echo);
    let (mut out, mut scratch) = ([0u8; 64], [0u8; 256]);
    let (got, served) = prompt("round trip", async {
        tokio::join!(
            req.request(KEY, b"ping", &mut out),
            resp.serve_one(&mut h, &mut scratch)
        )
    })
    .await;
    served.expect("serve_one");
    assert_eq!(got.expect("request"), b"ping");
}

/// With a timeout and a responder that never serves, `request_with` ends
/// with the backend's timeout error, no earlier than the timeout and no
/// later than the timeout plus `TIMEOUT_SLACK`.
pub async fn timeout_is_honoured<F: ReqRepFixture>(f: &mut F) {
    let (mut req, _idle) = f.connect(KEY).await;
    let timeout = Duration::from_millis(200);
    let mut out = [0u8; 64];
    let start = Instant::now();
    let r = prompt(
        "request_with timeout",
        req.request_with(
            KEY,
            b"x",
            &mut out,
            &RequestOptions::DEFAULT.with_timeout(timeout),
        ),
    )
    .await;
    let elapsed = start.elapsed();
    let e = r.expect_err("nobody served, yet a reply came");
    assert!(F::is_timeout(&e), "not a timeout: {e:?}");
    assert!(
        elapsed + Duration::from_millis(5) >= timeout,
        "returned early: {elapsed:?}"
    );
    assert!(
        elapsed <= timeout + F::TIMEOUT_SLACK,
        "returned late: {elapsed:?}"
    );
}

/// `Unavailable`, `DeadlineExceeded` and `Denied` returned by a handler
/// reach the requester as the same `HandlerError`.
pub async fn handler_errors_round_trip<F: ReqRepFixture>(f: &mut F) {
    let (mut req, mut resp) = f.connect(KEY).await;
    for e in [
        HandlerError::Unavailable,
        HandlerError::DeadlineExceeded,
        HandlerError::Denied,
    ] {
        let mut h = handler_fn(|_req, _out| Err(e));
        let (mut out, mut scratch) = ([0u8; 64], [0u8; 256]);
        let (got, served) = prompt("handler error", async {
            tokio::join!(
                req.request(KEY, b"x", &mut out),
                resp.serve_one(&mut h, &mut scratch)
            )
        })
        .await;
        served.expect("serve_one");
        let err = got.expect_err("handler failed, yet a reply came");
        assert_eq!(F::handler_error(&err), Some(e), "{err:?}");
    }
}

/// The handler sees the caller as the fixture says it should
/// (`RequestMeta::sender`, `principal`) and, if the backend propagates
/// deadlines, a `deadline_ms` within the caller's timeout.
pub async fn handler_sees_caller_identity<F: ReqRepFixture>(f: &mut F) {
    let (mut req, mut resp) = f.connect(KEY).await;
    let mut seen: Option<(Option<SenderId>, Option<String>, Option<u32>)> = None;
    let mut h = handler_fn(|r, out| {
        seen = Some((
            r.meta.sender,
            r.meta.principal.map(String::from),
            r.meta.deadline_ms,
        ));
        echo(r, out)
    });
    let (mut out, mut scratch) = ([0u8; 64], [0u8; 256]);
    let options = RequestOptions::DEFAULT.with_timeout(Duration::from_secs(3));
    let (got, served) = prompt("identity", async {
        tokio::join!(
            req.request_with(KEY, b"who", &mut out, &options),
            resp.serve_one(&mut h, &mut scratch)
        )
    })
    .await;
    served.expect("serve_one");
    got.expect("request");
    let (sender, principal, deadline) = seen.expect("handler ran");
    assert_eq!(sender, f.expected_sender());
    assert_eq!(principal.as_deref(), f.expected_principal());
    match (F::PROPAGATES_DEADLINE, deadline) {
        (true, Some(ms)) => assert!(ms <= 3000, "deadline {ms} ms over the 3 s timeout"),
        (true, None) => panic!("backend claims to propagate deadlines, handler saw none"),
        (false, d) => assert_eq!(d, None, "backend claims not to propagate deadlines"),
    }
}

/// Serves one request on each responder, concurrently, echoing.
async fn serve_all<R: Responder>(responders: &mut [R]) -> Vec<Result<(), R::Error>> {
    futures::future::join_all(responders.iter_mut().map(|r| async move {
        let mut h = handler_fn(echo);
        let mut scratch = [0u8; 256];
        r.serve_one(&mut h, &mut scratch).await
    }))
    .await
}

/// One request to `n` responders: `request_each` passes every reply to the
/// callback (each from a distinct sender when the backend names senders),
/// returns `n`, and stops early on `Flow::Stop`.
pub async fn gather_collects_every_responder<F: GatherFixture>(f: &mut F, n: usize) {
    let (mut g, mut responders) = f.connect(KEY, n).await;
    let options = RequestOptions::DEFAULT.with_timeout(Duration::from_secs(3));
    let mut senders = Vec::new();
    let mut buf = [0u8; 64];
    let (count, served) = prompt("gather", async {
        tokio::join!(
            g.request_each(KEY, b"all", &mut buf, &options, n, |reply| {
                assert_eq!(reply.result, Ok(&b"all"[..]));
                senders.push(reply.sender);
                Flow::Continue
            }),
            serve_all(&mut responders)
        )
    })
    .await;
    assert!(served.iter().all(Result::is_ok), "serve_one failed");
    assert_eq!(count.expect("request_each"), n);
    assert_eq!(senders.len(), n);
    let named: Vec<_> = senders.iter().flatten().collect();
    let mut distinct = named.clone();
    distinct.sort_by_key(|s| s.0);
    distinct.dedup();
    assert_eq!(
        distinct.len(),
        named.len(),
        "two replies from one sender: {senders:?}"
    );

    // Flow::Stop after the first reply ends the gather with 1.
    let (count, _served) = prompt("gather stop", async {
        tokio::join!(
            g.request_each(KEY, b"one", &mut buf, &options, n, |_| Flow::Stop),
            serve_all(&mut responders)
        )
    })
    .await;
    assert_eq!(count.expect("request_each"), 1);
}
