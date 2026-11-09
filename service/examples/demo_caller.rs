//! `demo-caller`: the reference calling service of the 0.6 end-to-end
//! run.
//!
//! Serves `demo/hello` and calls `demo/echo` (discovered through the
//! cluster DNS, `MW_SERVICE_MAP=demo/echo=demo-echo.apps:grpc`) with
//! `ctx.client`, which continues the trace, the request id and the
//! deadline. The request body chooses what it does:
//!
//! * `gather` — asks every ready `demo/echo` pod at once and answers one
//!   line per reply, `<node id> <echo's answer>`, sorted;
//! * `node:<16 hex digits>` — asks the pod with that node id
//!   (`Target::Node`, the id from a gather line) and answers its reply;
//! * anything else — asks any one pod (round robin) and answers its reply.
//!
//! Admits the end-to-end client (`ns/apps/sa/e2e-client`) and the
//! emulated robot (`robot/robot-1`); edit [`CALLERS`] for others.

use std::process::ExitCode;

use middleware::Key;
use middleware::transport::{
    Flow, Gather, HandlerError, Request, RequestOptions, Requester, Target,
};
use mw_service::{Ctx, Downstream, Service, ServiceHandler};

/// Who may call `demo/hello`.
const CALLERS: [&str; 2] = [
    "spiffe://infra.local/ns/apps/sa/e2e-client",
    "spiffe://infra.local/robot/robot-1",
];

/// The called kind.
const ECHO: Key<'static> = Key::from_static("demo/echo");

/// Most `demo/echo` replies one gather collects.
const MAX_REPLIES: usize = 32;

/// What a request asks for (module docs). Pure.
#[derive(Debug, PartialEq, Eq)]
enum Ask {
    Gather,
    Node(middleware::transport::SenderId),
    Any,
}

fn ask(body: &[u8]) -> Result<Ask, HandlerError> {
    if body == b"gather" {
        return Ok(Ask::Gather);
    }
    match body.strip_prefix(b"node:") {
        Some(hex) => middleware::grpc::health::parse_node(hex)
            .map(Ask::Node)
            .ok_or(HandlerError::InvalidRequest),
        None => Ok(Ask::Any),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A downstream failure as the handler's error.
fn failed(e: middleware::grpc::GrpcError) -> HandlerError {
    e.handler_error().unwrap_or(HandlerError::Unavailable)
}

async fn gather(down: &mut Downstream<'_>, body: &[u8]) -> Result<String, HandlerError> {
    let mut buf = vec![0u8; 4096];
    let mut lines = Vec::new();
    down.request_each(
        ECHO,
        body,
        &mut buf,
        &RequestOptions::DEFAULT,
        MAX_REPLIES,
        |r| {
            let node = r.sender.map_or("-".to_owned(), |s| hex(&s.0));
            let text = r.result.map_or_else(
                |e| format!("error {e:?}"),
                |b| String::from_utf8_lossy(b).into_owned(),
            );
            lines.push(format!("{node} {text}"));
            Flow::Continue
        },
    )
    .await
    .map_err(failed)?;
    lines.sort();
    Ok(lines.join("\n"))
}

/// The `demo/hello` handler.
struct Hello;

impl ServiceHandler for Hello {
    async fn handle(
        &mut self,
        ctx: &mut Ctx<'_>,
        req: &Request<'_>,
        out: &mut [u8],
    ) -> Result<usize, HandlerError> {
        let ask = ask(req.body)?;
        let mut down = ctx.client("demo/echo").ok_or(HandlerError::Unavailable)?;
        let mut buf = vec![0u8; 4096];
        let text = match ask {
            Ask::Gather => gather(&mut down, b"hello").await?,
            Ask::Node(id) => {
                let options = RequestOptions::DEFAULT.with_target(Target::Node(id));
                let r = down
                    .request_with(ECHO, req.body, &mut buf, &options)
                    .await
                    .map_err(failed)?;
                String::from_utf8_lossy(r).into_owned()
            }
            Ask::Any => {
                let r = down
                    .request(ECHO, req.body, &mut buf)
                    .await
                    .map_err(failed)?;
                String::from_utf8_lossy(r).into_owned()
            }
        };
        let dst = out
            .get_mut(..text.len())
            .ok_or(HandlerError::BufferTooSmall)?;
        dst.copy_from_slice(text.as_bytes());
        Ok(text.len())
    }
}

fn main() -> ExitCode {
    match Service::builder("demo/caller")
        .serve("demo/hello", Hello)
        .calls("demo/echo")
        .allow_callers(CALLERS)
        .build()
    {
        Ok(service) => service.main(),
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::transport::SenderId;

    #[test]
    fn body_selects_gather_node_or_any() {
        assert_eq!(ask(b"gather"), Ok(Ask::Gather));
        assert_eq!(
            ask(b"node:0102030405060708"),
            Ok(Ask::Node(SenderId([1, 2, 3, 4, 5, 6, 7, 8])))
        );
        assert_eq!(ask(b"node:zz"), Err(HandlerError::InvalidRequest));
        assert_eq!(ask(b"hi"), Ok(Ask::Any));
        assert_eq!(ask(b""), Ok(Ask::Any));
    }

    #[test]
    fn spec_declares_the_dependency_and_callers() {
        let s = Service::builder("demo/caller")
            .serve("demo/hello", Hello)
            .calls("demo/echo")
            .allow_callers(CALLERS)
            .build()
            .unwrap();
        assert_eq!(s.spec().calls, ["demo/echo"]);
    }
}
