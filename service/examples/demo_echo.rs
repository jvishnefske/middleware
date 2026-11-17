//! `demo-echo`: the reference served service of the 0.6 end-to-end run.
//!
//! Serves `demo/echo` and answers one line,
//! `echo pod=<POD_NAME> from=<caller principal> trace=<trace id> body=<request>`
//! (`-` for what is absent), so the caller can see which pod answered,
//! which identity the sidecar authenticated and that the trace continued.
//! Admits only `demo-caller`'s service account; runs three replicas.
//!
//! ```text
//! cargo run -p mw-service --example demo_echo                  # serve (MW_PORT, ...)
//! cargo run -p mw-service --example demo_echo -- --print-spec
//! ```

use std::process::ExitCode;

use middleware::transport::{HandlerError, Request, handler_fn};
use mw_service::Service;

/// The caller this service admits.
const CALLER: &str = "spiffe://infra.local/ns/apps/sa/demo-caller";

/// The answer to `req` from pod `pod`. Pure.
fn answer(pod: &str, req: &Request<'_>) -> String {
    format!(
        "echo pod={pod} from={} trace={} body={}",
        req.meta.principal.unwrap_or("-"),
        req.meta.trace.map_or("-".to_owned(), |t| {
            t.trace_id().iter().map(|b| format!("{b:02x}")).collect()
        }),
        String::from_utf8_lossy(req.body),
    )
}

fn main() -> ExitCode {
    // The downward API's pod name; only shown, never trusted.
    let pod = std::env::var("POD_NAME").unwrap_or_else(|_| "-".to_owned());
    let echo = handler_fn(move |req, out| {
        let text = answer(&pod, req);
        let dst = out
            .get_mut(..text.len())
            .ok_or(HandlerError::BufferTooSmall)?;
        dst.copy_from_slice(text.as_bytes());
        Ok(text.len())
    });
    match Service::builder("demo/echo")
        .serve("demo/echo", echo)
        .allow_callers([CALLER])
        .replicas(3)
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
    use middleware::Key;
    use middleware::transport::{RequestMeta, TraceParent};

    #[test]
    fn answers_pod_caller_trace_and_body() {
        let tp = TraceParent::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01");
        let meta = RequestMeta {
            principal: Some(CALLER),
            trace: tp,
            ..RequestMeta::NONE
        };
        let req = Request::new(Key::from_static("demo/echo"), b"hi").with_meta(meta);
        assert_eq!(
            answer("demo-echo-0", &req),
            format!(
                "echo pod=demo-echo-0 from={CALLER} trace=4bf92f3577b34da6a3ce929d0e0e4736 body=hi"
            )
        );
        let bare = Request::new(Key::from_static("demo/echo"), b"");
        assert_eq!(answer("-", &bare), "echo pod=- from=- trace=- body=");
    }
}
