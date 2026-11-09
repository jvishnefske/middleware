//! The robot's imperative shell: [`Robot::run`] ticks every period until
//! shut down. Each tick
//!
//! 1. starts a new root span (random trace and span ids, sampled) with
//!    request id `<name>-<seq>`, as the runner does for an inbound request
//!    ([`server_trace`]);
//! 2. calls the configured kind through [`Ctx::client`] with the body of
//!    [`echo_body`], so the call carries that span as `traceparent`, the
//!    request id as `x-request-id` and a deadline of one period, through
//!    the kind's static endpoint (the local Envoy);
//! 3. folds the outcome into the [`RobotState`] and publishes its JSON on
//!    `app/robot/<name>/state` through [`Ctx::publisher`] (Zenoh, via the
//!    local Envoy's TCP listener to zenohd).
//!
//! The robot serves nothing, so it does not run the gRPC server of
//! `Service::run_with`; it uses the same declarations ([`Service`]), the
//! same configuration ([`Config`], with the robot defaults of
//! [`RobotConfig::runner_config`]) and the same per-request context.

use std::future::Future;
use std::time::{Duration, Instant};

use middleware::key::Key;
use middleware::transport::{Publisher, RequestMeta, Requester};
use middleware::zenoh::{SessionConfig, ZenohTransport};
use mw_service::{Config, Ctx, Links, Service, ServiceError, SpecError, server_trace};

use crate::config::RobotConfig;
use crate::telemetry::{EchoOutcome, RobotState, echo_body, echo_matches};

/// What a stopped robot did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RobotReport {
    /// The last state.
    pub state: RobotState,
    /// Telemetry publishes that failed.
    pub publish_failed: u64,
}

/// A robot: its settings and its declared service.
#[derive(Debug)]
pub struct Robot {
    config: RobotConfig,
    service: Service,
}

impl Robot {
    /// Declares `robot/<name>`: calls `config.call`, publishes
    /// `app/robot/<name>/**`.
    pub fn new(config: RobotConfig) -> Result<Self, SpecError> {
        let service = Service::builder(&config.service_name())
            .calls(&config.call)
            .publishes(&format!("app/robot/{}/**", config.name))
            .build()?;
        Ok(Robot { config, service })
    }

    /// The declared service.
    pub fn service(&self) -> &Service {
        &self.service
    }

    /// The settings.
    pub fn config(&self) -> &RobotConfig {
        &self.config
    }

    /// Runs with the runner configuration `runner` (robot defaults filled
    /// in) until `shutdown` completes. Fails at start if a called kind has
    /// no static endpoint or the Zenoh session cannot be opened.
    pub async fn run(
        self,
        runner: Config,
        shutdown: impl Future<Output = ()>,
    ) -> Result<RobotReport, ServiceError> {
        // Only the spec is needed from here on (the service serves
        // nothing; its handlers are not `Sync`, so keep none across awaits).
        let Robot { config, service } = self;
        let runner = config.runner_config(runner);
        let (spec, _) = service.into_parts();
        let mut links = Links::new();
        for kind in &spec.calls {
            let endpoint = runner
                .static_endpoint(kind)
                .ok_or_else(|| ServiceError::Unmapped(kind.clone()))?;
            let key = Key::new(kind).map_err(|_| ServiceError::Unmapped(kind.clone()))?;
            links = links.with_client(
                middleware::discovery::ServiceKind::new(key),
                endpoint.client(),
            );
        }
        let router = runner
            .zenoh_router
            .clone()
            .ok_or(ServiceError::Unimplemented(
                "a robot without MW_ZENOH_ROUTER",
            ))?;
        let zenoh = SessionConfig::new()
            .connect(router)
            .build()
            .map_err(ServiceError::Zenoh)?;
        let session = ZenohTransport::open(zenoh)
            .await
            .map_err(ServiceError::Zenoh)?;
        let mut links = links.with_zenoh(session, spec.publish_filters());
        let started = Instant::now();
        let mut report = RobotReport {
            state: RobotState::default(),
            publish_failed: 0,
        };
        let mut tick = tokio::time::interval(config.period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => break,
                _ = tick.tick() => {
                    report = tick_once(&config, &mut links, report, started).await;
                }
            }
        }
        Ok(report)
    }
}

/// One tick (module docs).
async fn tick_once(
    config: &RobotConfig,
    links: &mut Links,
    report: RobotReport,
    started: Instant,
) -> RobotReport {
    let seq = report.state.seq.saturating_add(1);
    let mut ids = [0u8; 24];
    // No randomness: no trace (all-zero ids are invalid), still a call.
    let _ = getrandom::fill(&mut ids);
    let (mut trace_id, mut span_id) = ([0u8; 16], [0u8; 8]);
    trace_id.copy_from_slice(&ids[..16]);
    span_id.copy_from_slice(&ids[16..]);
    let trace = server_trace(None, span_id, trace_id);
    let request_id = format!("{}-{seq}", config.name);
    let meta = RequestMeta::NONE.with_request_id_str(&request_id);
    let mut ctx = Ctx::new(meta, trace, links).with_deadline(Instant::now() + config.period);
    let outcome = call(&mut ctx, &config.call, seq).await;
    let uptime_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let state = report.state.after(outcome, uptime_ms);
    tracing::info!(seq, ?outcome, trace = ?trace.map(|t| t.to_string()), "tick");
    let json = state.to_json(&config.name, trace);
    let key = config.state_key();
    let published = match (ctx.publisher(), Key::new(&key)) {
        (Some(mut p), Ok(k)) => p.publish(k, json.as_bytes()).await.is_ok(),
        _ => false,
    };
    RobotReport {
        state,
        publish_failed: report.publish_failed.saturating_add(u64::from(!published)),
    }
}

/// One call of `kind` through the context: the echo of tick `seq`'s body.
async fn call(ctx: &mut Ctx<'_>, kind: &str, seq: u64) -> EchoOutcome {
    let (Some(mut down), Ok(key)) = (ctx.client(kind), Key::new(kind)) else {
        return EchoOutcome::Failed;
    };
    let body = echo_body(seq);
    let mut out = [0u8; 256];
    let t0 = Instant::now();
    match down.request(key, body.as_bytes(), &mut out).await {
        Ok(reply) if echo_matches(seq, reply) => {
            EchoOutcome::Ok(t0.elapsed().max(Duration::from_micros(1)))
        }
        _ => EchoOutcome::Failed,
    }
}
