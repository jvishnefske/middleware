//! `hello`: the smallest `mw-service` binary, serving kind `demo/hello`.
//!
//! It exists to prove the container pipeline (`nix build .#hello`,
//! `nix build .#image-hello`, design.md M40-M42): a musl static binary
//! whose `main` is [`Service::main`], so the same executable serves
//! (`hello`), prints its spec (`hello --print-spec`) and renders its
//! manifests (`hello --print-manifests`).
//!
//! The handler answers every request with `hello, ` followed by the
//! request body, or `BufferTooSmall` when the reply does not fit.

use std::process::ExitCode;

use middleware::transport::{HandlerError, handler_fn};
use mw_service::Service;

/// The reply prefix.
const GREETING: &[u8] = b"hello, ";

fn main() -> ExitCode {
    let hello = handler_fn(|req, out| {
        let len = GREETING.len() + req.body.len();
        let dst = out.get_mut(..len).ok_or(HandlerError::BufferTooSmall)?;
        let (greeting, body) = dst.split_at_mut(GREETING.len());
        greeting.copy_from_slice(GREETING);
        body.copy_from_slice(req.body);
        Ok(len)
    });
    match Service::builder("hello").serve("demo/hello", hello).build() {
        Ok(service) => service.main(),
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
