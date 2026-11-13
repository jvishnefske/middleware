//! The robot's pure core: its state, how one tick changes it, and the
//! telemetry it publishes.
//!
//! [`RobotState::after`] folds the outcome of one tick's call to the
//! cluster ([`EchoOutcome`]) into the next state; [`RobotState::to_json`]
//! renders the `app/robot/<name>/state` payload, one JSON object:
//!
//! ```text
//! {"robot":"r1","seq":3,"uptime_ms":3000,"echo_ok":3,"echo_failed":0,"rtt_us":812,"trace":"00-<32 hex>-<16 hex>-01"}
//! ```
//!
//! `rtt_us` is the last successful call's round trip (`null` before the
//! first); `trace` is the tick's span (`null` without one), the same
//! trace id the called service's handler sees, so a published state can
//! be found in Tempo. The robot name is a DNS label (validated by
//! [`crate::config`]), so it needs no JSON escaping.

use middleware::transport::TraceParent;
use std::time::Duration;

/// What one tick's call to the cluster did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EchoOutcome {
    /// Answered with the request echoed, after this round trip.
    Ok(Duration),
    /// Failed, timed out, or answered with something else.
    Failed,
}

/// The robot's state after some ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RobotState {
    /// Ticks done.
    pub seq: u64,
    /// Milliseconds since start at the last tick.
    pub uptime_ms: u64,
    /// Calls answered correctly.
    pub echo_ok: u64,
    /// Calls that failed.
    pub echo_failed: u64,
    /// The last successful round trip, in microseconds.
    pub rtt_us: Option<u64>,
}

impl RobotState {
    /// The state after a tick at `uptime_ms` whose call had `outcome`.
    pub fn after(self, outcome: EchoOutcome, uptime_ms: u64) -> Self {
        let seq = self.seq.saturating_add(1);
        match outcome {
            EchoOutcome::Ok(rtt) => RobotState {
                seq,
                uptime_ms,
                echo_ok: self.echo_ok.saturating_add(1),
                rtt_us: Some(u64::try_from(rtt.as_micros()).unwrap_or(u64::MAX)),
                ..self
            },
            EchoOutcome::Failed => RobotState {
                seq,
                uptime_ms,
                echo_failed: self.echo_failed.saturating_add(1),
                ..self
            },
        }
    }

    /// The telemetry payload (module docs).
    pub fn to_json(&self, robot: &str, trace: Option<TraceParent>) -> String {
        let rtt = self
            .rtt_us
            .map_or_else(|| "null".to_owned(), |r| r.to_string());
        let trace = trace.map_or_else(|| "null".to_owned(), |t| format!("\"{t}\""));
        format!(
            "{{\"robot\":\"{robot}\",\"seq\":{},\"uptime_ms\":{},\"echo_ok\":{},\"echo_failed\":{},\"rtt_us\":{rtt},\"trace\":{trace}}}",
            self.seq, self.uptime_ms, self.echo_ok, self.echo_failed
        )
    }
}

/// The request body of tick `seq`: `tick <seq>`. The echo service
/// returns it unchanged, which [`echo_matches`] checks.
pub fn echo_body(seq: u64) -> String {
    format!("tick {seq}")
}

/// Whether `reply` is the echo of tick `seq`'s body.
pub fn echo_matches(seq: u64, reply: &[u8]) -> bool {
    reply == echo_body(seq).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TP: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[test]
    fn v1_state_counts_ticks_and_outcomes() {
        let s = RobotState::default()
            .after(EchoOutcome::Ok(Duration::from_micros(800)), 1000)
            .after(EchoOutcome::Failed, 2000);
        assert_eq!(
            s,
            RobotState {
                seq: 2,
                uptime_ms: 2000,
                echo_ok: 1,
                echo_failed: 1,
                rtt_us: Some(800)
            }
        );
    }

    #[test]
    fn v1_telemetry_is_one_json_object_with_the_trace() {
        let s = RobotState::default().after(EchoOutcome::Ok(Duration::from_micros(812)), 3000);
        assert_eq!(
            s.to_json("r1", TraceParent::parse(TP)),
            format!(
                "{{\"robot\":\"r1\",\"seq\":1,\"uptime_ms\":3000,\"echo_ok\":1,\"echo_failed\":0,\"rtt_us\":812,\"trace\":\"{TP}\"}}"
            )
        );
        assert_eq!(
            RobotState::default().to_json("r1", None),
            "{\"robot\":\"r1\",\"seq\":0,\"uptime_ms\":0,\"echo_ok\":0,\"echo_failed\":0,\"rtt_us\":null,\"trace\":null}"
        );
        assert!(echo_matches(4, echo_body(4).as_bytes()));
        assert!(!echo_matches(4, b"tick 5"));
    }
}
