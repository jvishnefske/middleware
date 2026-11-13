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
//! | `gw/sys` update requests from `MW_GATEWAY_UPDATERS` | device requests | signed payload uploads, signed start/stop, RP2350 firmware updates, under the device's over-the-air rules ([`update`]); nobody by default |
//!
//! The rules are in [`bridge`] and [`update`] (pure), the tasks in
//! [`shell`], the settings in [`config`], the serial port and in-memory
//! streams in [`io`], the remote update client in [`remote`] (binary
//! `mw-update`).
//!
//! # Embedding
//!
//! A process that drives the bridge itself (over any byte stream wrapped
//! in an [`io::ChannelIo`], an in-memory [`io::pair`] in tests) calls
//! [`start_with_store`] (the configured timeouts,
//! [`GatewayConfig::shell_options`], and a [`state::ControlStore`] for
//! accepted start/stop counters) with [`GatewayConfig::bridge_with_floor`]
//! and [`state::floor_at_start`] (the persisted counter and the clock;
//! [`start`] and [`start_with`] have no store, so signed start/stop are
//! refused there),
//! answers `gw/sys` with the returned [`SysForward`] (as a
//! `mw-service` handler, through [`service`], or over gRPC with
//! [`serve_sys`]), and ends with [`Gateway::stop`].
//!
//! # Running
//!
//! The binary reads [`config::GatewayConfig`] (`MW_GATEWAY_*`) and the
//! runner's `mw_service::Config` (`MW_ZENOH_ROUTER` is required: the
//! gateway's Zenoh session connects there). It opens the serial port,
//! starts the bridge and runs the service. (Until the `mw-service`
//! runner serves, MS6, `run_with` returns `Unimplemented` and the binary
//! exits with that error.) The gateway speaks only the
//! serial link: an ESP-NOW path (through a radio bridge device) is not
//! implemented.
#![forbid(unsafe_code)]

pub mod bridge;
pub mod config;
pub mod io;
pub mod remote;
pub mod shell;
pub mod state;
pub mod update;

use mw_service::{Service, SpecError};

pub use config::GatewayConfig;
pub use shell::{
    Gateway, Report, SYS_KIND, ShellOptions, SysForward, serve_sys, start, start_with,
    start_with_store,
};

/// The service name.
pub const SERVICE_NAME: &str = "mw/gateway";

/// The gateway's service: serves [`SYS_KIND`] with `sys`, admits
/// `config.allow_callers` and the updaters, publishes everything below the cluster prefix
/// (`app/**` or `app/<ns>/**`, which makes it a Zenoh user) and subscribes
/// to the downlink filters.
pub fn service(config: &GatewayConfig, sys: SysForward) -> Result<Service, SpecError> {
    let publishes = format!("{}/**", config.map.cluster_prefix());
    let builder = Service::builder(SERVICE_NAME)
        .serve(
            SYS_KIND,
            sys.with_timeout(config.sys_timeout)
                .with_update_timeout(config.update_timeout),
        )
        .allow_callers(config.admitted_callers())
        .publishes(&publishes);
    config
        .downlink
        .iter()
        .fold(builder, |b, f| b.subscribes(&config::filter_text(f)))
        .build()
}
