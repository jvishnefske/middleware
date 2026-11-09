//! `mw-robot`: the Linux robot application. It runs **off cluster**, on a
//! robot whose spire-agent (join token attestor) holds the SVID
//! `spiffe://infra.local/robot/<name>`, and whose local Envoy (fed by
//! spiffe-helper) holds mTLS with that SVID to the cluster's ingress
//! gateway. The application has no TLS code: it talks plaintext to the
//! local Envoy on loopback.
//!
//! | local port | protocol | what the robot sends there |
//! |------------|----------|----------------------------|
//! | `127.0.0.1:15001` | h2c (gRPC) | calls to cluster services, `:authority` = the target's mesh name ([`config::default_endpoint`]) |
//! | `127.0.0.1:17447` | TCP | its Zenoh session to zenohd (publishes `app/robot/<name>/state`) |
//!
//! It is built on `mw-service`: the robot is a [`Service`](mw_service::Service)
//! `robot/<name>` that calls one kind (default `demo/echo`) and publishes
//! `app/robot/<name>/**`, configured by the runner's
//! [`Config`](mw_service::Config) with off-cluster defaults
//! ([`config`]); called kinds resolve through static endpoints
//! (`MW_STATIC_ENDPOINTS`, `mw_service::endpoints`) instead of cluster DNS.
//! Each tick's call carries a fresh root trace, and the published state
//! names it ([`telemetry`]); the loop is in [`run`].
#![forbid(unsafe_code)]

pub mod config;
pub mod run;
pub mod telemetry;

pub use config::RobotConfig;
pub use run::{Robot, RobotReport};
