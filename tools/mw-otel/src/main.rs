//! `mw-otel-bridge`: a Zenoh node that exports what the boards on its
//! network say, over OTLP.
//!
//! * subscribes (default [`DEFAULT_SUBSCRIPTIONS`]) through the middleware
//!   Zenoh backend built with `zenoh-otel`, so every delivered sample is a
//!   `mw deliver <key>` span and counts in `mw.zenoh.delivered`,
//!   `mw.zenoh.lost` (sequence gaps) and `mw.zenoh.bytes`;
//! * turns each `sys/log` line into an OTLP log record (target
//!   `mw.device`, attributes `mw.sender`, `mw.key`, `mw.seq`);
//! * asks `sys/info` of every node each `--info-interval-s` (a Gather) and
//!   reports `mw.node.up` per node (1 answered recently, 0 stale), with
//!   chip, firmware and payload status as attributes, logging nodes that
//!   appear, change payload status or go away.
//!
//! The boards connect to it (`--listen tcp/0.0.0.0:7447`, then on each board
//! `mwctl request sys/zenoh/endpoint tcp/<this host>:7447`), or it joins
//! an existing router (`--connect`). Exporter endpoints come from the
//! standard `OTEL_*` variables (`mw_otel` crate docs).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use middleware::loader::keys;
use middleware::transport::{Gather, RequestOptions};
use middleware::zenoh::{SessionConfig, ZenohError, ZenohTransport};
use middleware::{Flow, Subscribe, Subscriber};
use mw_otel::{
    DEFAULT_SUBSCRIPTIONS, NodeEvent, Nodes, Signals, log_text, parse_filter, sender_hex,
};
use opentelemetry::global;
use tracing::{info, warn};

#[derive(Parser)]
#[command(
    version,
    about = "Export the boards' sys/log, traffic and sys/info over OTLP"
)]
struct Args {
    /// Zenoh endpoint to listen on, e.g. tcp/0.0.0.0:7447; repeatable.
    #[arg(long, env = "MW_ZENOH_LISTEN", value_delimiter = ',')]
    listen: Vec<String>,
    /// Zenoh endpoint (router or peer) to connect to; repeatable.
    #[arg(long, env = "MW_ZENOH_CONNECT", value_delimiter = ',')]
    connect: Vec<String>,
    /// A key (`sys/log`) or key prefix (`app/**`) to subscribe to;
    /// repeatable. Default: sys/log, app/**, imu/stat, mic/stat, mic/level.
    #[arg(long = "sub")]
    subs: Vec<String>,
    /// Seconds between sys/info rounds (0: none).
    #[arg(long, default_value_t = 15)]
    info_interval_s: u64,
    /// The OTLP `service.name`.
    #[arg(long, env = "OTEL_SERVICE_NAME", default_value = "mw-otel-bridge")]
    service_name: String,
    /// Put the bridge's trace context on its sys/info queries (attachment
    /// kind 0x12). Boards older than middleware 0.6 then see no requester.
    #[arg(long)]
    trace_context: bool,
    #[arg(long)]
    no_traces: bool,
    #[arg(long)]
    no_metrics: bool,
    #[arg(long)]
    no_logs: bool,
}

/// The largest sample the bridge accepts (a whole JPEG frame fits).
const SAMPLE_BUF: usize = 256 * 1024;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args = Args::parse();
    let telemetry = match mw_otel::init(
        &args.service_name,
        Signals {
            traces: !args.no_traces,
            metrics: !args.no_metrics,
            logs: !args.no_logs,
        },
    ) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("mw-otel-bridge: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let code = match run(args).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e}");
            std::process::ExitCode::FAILURE
        }
    };
    telemetry.shutdown();
    code
}

async fn run(args: Args) -> Result<(), String> {
    let subs: Vec<String> = if args.subs.is_empty() {
        DEFAULT_SUBSCRIPTIONS
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        args.subs.clone()
    };
    let filters = subs
        .iter()
        .map(|s| parse_filter(s))
        .collect::<Result<Vec<_>, _>>()?;
    if args.listen.is_empty() && args.connect.is_empty() {
        return Err("give --listen and/or --connect: with neither the bridge hears nobody".into());
    }
    let mut config = SessionConfig::new();
    for l in &args.listen {
        config = config.listen(l.clone());
    }
    for c in &args.connect {
        config = config.connect(c.clone());
    }
    let config = config.build().map_err(|e| format!("zenoh config: {e}"))?;
    let transport = ZenohTransport::open(config)
        .await
        .map_err(|e| format!("zenoh open: {e}"))?
        .with_timeout(Duration::from_secs(2))
        .with_trace_context(args.trace_context);
    info!(
        sender = %sender_hex(transport.sender_id()),
        listen = ?args.listen,
        connect = ?args.connect,
        subscriptions = ?subs,
        "mw-otel-bridge up"
    );

    let mut tasks = tokio::task::JoinSet::new();
    for (text, filter) in subs.iter().zip(filters) {
        let mut t = transport.clone();
        let mut sub = t
            .subscribe(filter.as_filter())
            .await
            .map_err(|e| format!("subscribe {text}: {e}"))?;
        let text = text.clone();
        tasks.spawn(async move {
            let mut buf = vec![0u8; SAMPLE_BUF];
            loop {
                match sub.recv(&mut buf).await {
                    Ok(s) => {
                        if s.key == keys::LOG {
                            let sender = s.meta.sender.map(sender_hex).unwrap_or_default();
                            let line = log_text(s.payload);
                            let seq = s.meta.seq.map(i64::from).unwrap_or(-1);
                            tracing::event!(
                                target: "mw.device",
                                tracing::Level::INFO,
                                mw.sender = %sender,
                                mw.key = %s.key,
                                mw.seq = seq,
                                "{line}"
                            );
                        }
                    }
                    Err(ZenohError::Closed) => {
                        warn!("subscription {text} closed");
                        return;
                    }
                    Err(e) => warn!("subscription {text}: {e}"),
                }
            }
        });
    }

    if args.info_interval_s > 0 {
        let interval = Duration::from_secs(args.info_interval_s);
        let nodes = Arc::new(Mutex::new(Nodes::default()));
        let observed = nodes.clone();
        let fresh = interval * 3;
        // Kept alive for the life of the bridge: dropping it unregisters.
        let _up = global::meter("mw-otel-bridge")
            .u64_observable_gauge("mw.node.up")
            .with_description("1 if the node answered sys/info recently, else 0")
            .with_callback(move |o| {
                let nodes = observed.lock().unwrap_or_else(|p| p.into_inner());
                for (v, attrs) in nodes.observations(Instant::now(), fresh) {
                    o.observe(v, &attrs);
                }
            })
            .build();
        let mut t = transport.clone();
        tasks.spawn(async move {
            let _up = _up;
            let mut tick = tokio::time::interval(interval);
            let mut buf = vec![0u8; 512];
            loop {
                tick.tick().await;
                let mut answers = Vec::new();
                let r = t
                    .request_each(
                        keys::INFO,
                        &[],
                        &mut buf,
                        &RequestOptions::DEFAULT.with_timeout(Duration::from_secs(2)),
                        64,
                        |reply| {
                            if let (Some(sender), Ok(bytes)) = (reply.sender, reply.result) {
                                answers.push((sender, bytes.to_vec()));
                            }
                            Flow::Continue
                        },
                    )
                    .await;
                if let Err(e) = r {
                    warn!("sys/info round: {e}");
                }
                let now = Instant::now();
                let mut n = nodes.lock().unwrap_or_else(|p| p.into_inner());
                let mut events = Vec::new();
                for (sender, bytes) in &answers {
                    match n.record(*sender, bytes, now) {
                        Some(Some(e)) => events.push(e),
                        Some(None) => {}
                        None => {
                            warn!(mw.sender = %sender_hex(*sender), "undecodable sys/info answer")
                        }
                    }
                }
                events.extend(n.expire(now, interval * 10));
                drop(n);
                for e in events {
                    match e {
                        NodeEvent::Up(id) => info!(mw.sender = %sender_hex(id), "node up"),
                        NodeEvent::StatusChanged(id, from, to) => {
                            info!(mw.sender = %sender_hex(id), from, to, "payload status changed")
                        }
                        NodeEvent::Down(id) => warn!(mw.sender = %sender_hex(id), "node gone"),
                    }
                }
            }
        });
    }

    tokio::select! {
        _ = tokio::signal::ctrl_c() => info!("shutting down"),
        Some(r) = tasks.join_next() => {
            if let Err(e) = r {
                return Err(format!("a bridge task failed: {e}"));
            }
        }
    }
    Ok(())
}
