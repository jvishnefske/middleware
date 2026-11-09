//! `mw-gateway`: bridges one serial-attached middleware device (an ESP32
//! running the ESP base firmware) to the cluster's Zenoh fabric, as an
//! `mw-service` [`Service`] named `mw/gateway`.
//!
//! What crosses, in each direction:
//!
//! | from | to | what |
//! |------|----|------|
//! | device `app/**` publications | Zenoh `app/**` (or `app/<ns>/**`) | payload unchanged, so `middleware::stream` chunks (the camera's `app/cam/jpeg`) arrive intact |
//! | device `disc/<kind>` beacons | `ZenohDiscovery` announce / withdraw | node bound to the device's MAC (firmware rule E21), at most 4 records |
//! | device `sys/log` | the gateway's log tail | read through `gw/sys` |
//! | Zenoh samples on the configured downlink filters | device publish frames | key mapped back to the device's space |
//! | `gw/sys` requests (served kind) | device requests | only allowlisted keys (default `sys/info`, plus `sys/log` from the tail); the rest `Denied` |
//!
//! The rules are in [`bridge`] (pure), the tasks in [`shell`], the
//! settings in [`config`], the serial port and in-memory streams in
//! [`io`].
//!
//! # Running
//!
//! The binary reads [`config::GatewayConfig`] (`MW_GATEWAY_*`) and the
//! runner's `mw_service::Config` (`MW_ZENOH_ROUTER` is required: the
//! gateway's Zenoh session connects there). It opens the serial port,
//! starts the bridge and runs the service. The gateway speaks only the
//! serial link: an ESP-NOW path (through a radio bridge device) is not
//! implemented.
#![forbid(unsafe_code)]

pub mod bridge;
pub mod config;
pub mod io;
pub mod shell;

use mw_service::{Service, SpecError};

pub use config::GatewayConfig;
pub use shell::{Gateway, Report, SYS_KIND, SysForward, start};

/// The service name.
pub const SERVICE_NAME: &str = "mw/gateway";

/// The gateway's service: serves [`SYS_KIND`] with `sys`, admits
/// `config.allow_callers`, publishes everything below the cluster prefix
/// (`app/**` or `app/<ns>/**`, which makes it a Zenoh user) and subscribes
/// to the downlink filters.
pub fn service(config: &GatewayConfig, sys: SysForward) -> Result<Service, SpecError> {
    let publishes = format!("{}/**", config.map.cluster_prefix());
    let builder = Service::builder(SERVICE_NAME)
        .serve(SYS_KIND, sys.with_timeout(config.sys_timeout))
        .allow_callers(config.allow_callers.iter().cloned())
        .publishes(&publishes);
    config
        .downlink
        .iter()
        .fold(builder, |b, f| b.subscribes(&config::filter_text(f)))
        .build()
}
