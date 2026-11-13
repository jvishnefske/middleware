//! `mw-service`: write a [`Handler`](middleware::Handler), get a meshed,
//! traced, SPIFFE-authenticated gRPC service, with Kubernetes manifests
//! derived from the same code.
//!
//! ```no_run
//! use middleware::transport::{HandlerError, handler_fn};
//! use mw_service::Service;
//!
//! fn main() -> std::process::ExitCode {
//!     let echo = handler_fn(|req, out| {
//!         let dst = out.get_mut(..req.body.len()).ok_or(HandlerError::BufferTooSmall)?;
//!         dst.copy_from_slice(req.body);
//!         Ok(req.body.len())
//!     });
//!     match Service::builder("demo/echo")
//!         .serve("demo/echo", echo)
//!         .allow_callers(["spiffe://infra.local/ns/apps/sa/demo-caller"])
//!         .replicas(3)
//!         .build()
//!     {
//!         Ok(service) => service.main(),
//!         Err(e) => {
//!             eprintln!("{e}");
//!             std::process::ExitCode::FAILURE
//!         }
//!     }
//! }
//! ```
//!
//! # Modules
//!
//! * [`builder`] — [`Service::builder`] → [`ServiceBuilder`] (immutable by
//!   value) → [`Service`].
//! * [`spec`] — [`ServiceSpec`], the pure, serializable record of every
//!   declaration, and its validation; manifests render from it.
//! * [`config`] — [`Config::from_env`], the pure parser of the deployment
//!   environment (`MW_PORT`, `POD_NAMESPACE`, `MW_SERVICE_MAP`, ...).
//! * [`lifecycle`] — [`Lifecycle`], the pure start/serve/drain/stop state
//!   machine the runner drives.
//! * [`endpoints`] — [`StaticEndpoints`] (`MW_STATIC_ENDPOINTS`): fixed
//!   dial addresses and `:authority` for called kinds, for services running
//!   off cluster (a robot behind its local Envoy).
//! * [`ctx`] — [`ServiceHandler`] (async, with a blanket implementation for
//!   every plain `Handler`) and its per-request [`Ctx`]: caller metadata,
//!   this span's trace context, downstream clients that continue the trace
//!   ([`Downstream`]) and the declared publisher ([`ServicePublisher`]).
//! * [`run`] — the command line ([`Command`]), [`Service::main`],
//!   [`Service::run`] and [`Service::run_with`].
//! * [`manifest`] — `--print-manifests` rendering.
//! * [`error`] — [`ServiceError`].
//!
//! # Status
//!
//! 0.6 phase 1: the public API is complete and the pure parts (builder,
//! spec, config, lifecycle, context, command line, `--print-spec`) are
//! implemented and tested. Serving and manifest rendering return
//! [`ServiceError::Unimplemented`] (never a panic) until their phase lands;
//! design.md "Mesh service runner (`mw-service`)" tracks which requirement
//! each phase closes.
//!
//! # Feature isolation
//!
//! This crate is host only: it depends on `middleware` with `grpc`,
//! `istio`, `dns-sd` and `zenoh`, and on tokio, tracing and OpenTelemetry.
//! The embedded builds of `middleware` never see it
//! (`scripts/check-features.sh`).
#![forbid(unsafe_code)]

pub mod builder;
pub mod config;
pub mod ctx;
pub mod endpoints;
pub mod error;
pub mod lifecycle;
pub mod manifest;
pub mod run;
pub mod spec;

pub use builder::{Served, Service, ServiceBuilder};
pub use config::{Config, ConfigError};
pub use ctx::{
    BoxedHandler, Ctx, Downstream, DownstreamClient, Links, PublishError, ServiceHandler,
    ServicePublisher, server_trace,
};
pub use endpoints::{StaticEndpoint, StaticEndpoints};
pub use error::ServiceError;
pub use lifecycle::{Action, Event, Lifecycle, Monotonic, Outcome};
pub use manifest::ManifestOptions;
pub use run::{ArgsError, Command};
pub use spec::{Mebibytes, MilliCpu, Resources, ServiceSpec, SpecError};
