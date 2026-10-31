//! [`Lifecycle`]: the pure state machine of a running service, from start
//! to exit, including graceful drain on termination.
//!
//! ```text
//!            Ready                 Terminate               Tick ≥ since + grace
//! Starting ────────▶ Serving ───────────────▶ Draining ─────────────────────▶ Stopped
//!    │                                           │  Terminate (again)            ▲
//!    │ Terminate                                 └───────────────────────────────┤
//!    └───────────────────────────────────────────────────────────────────────────┤
//!                         Failed (from any state but Stopped) ───────────────────┘
//! ```
//!
//! The runner (the imperative shell) feeds [`Event`]s with the current
//! [`Monotonic`] time and performs the returned [`Action`]s in order:
//!
//! * `Starting` + `Ready` (every serve loop is up) → `Serving`, health
//!   `SERVING`.
//! * `Serving` + `Terminate` (SIGTERM) → `Draining`, health `NOT_SERVING`:
//!   the kubelet's readiness probe and every `MeshBrowse` watching this pod
//!   take it out of rotation, while calls already routed here are still
//!   answered.
//! * `Draining` + `Tick` once `drain_grace` has passed → `Stopped`: stop
//!   accepting, then exit cleanly. [`Lifecycle::next_deadline`] says when
//!   the runner should tick.
//! * A second `Terminate` while draining, or `Terminate` before ever
//!   serving (no endpoint routes here yet), stops at once.
//! * `Failed` (a serve loop or the server died) → `Stopped` with health
//!   `NOT_SERVING` and [`Outcome::Failed`].
//! * Anything else changes nothing and asks for nothing; `Stopped` is final.
//!
//! The drain grace must stay below the pod's
//! `terminationGracePeriodSeconds`, or the kubelet kills the process
//! before it exits on its own (the manifests derive one from the other).

use middleware::grpc::ServingStatus;
use std::time::Duration;

/// A point on a monotonic clock, as the time since an arbitrary origin
/// (the runner uses its start). Only differences are meaningful.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Monotonic(Duration);

impl Monotonic {
    /// The origin.
    pub const ZERO: Monotonic = Monotonic(Duration::ZERO);

    /// The point `d` after the origin.
    pub const fn from_origin(d: Duration) -> Self {
        Monotonic(d)
    }

    /// Time since the origin.
    pub const fn since_origin(self) -> Duration {
        self.0
    }

    /// Time from `earlier` to `self`, zero if `earlier` is later.
    pub fn since(self, earlier: Monotonic) -> Duration {
        self.0.saturating_sub(earlier.0)
    }

    /// The point `d` after `self`, saturating.
    pub fn plus(self, d: Duration) -> Monotonic {
        Monotonic(self.0.saturating_add(d))
    }
}

/// Where a service is in its life (module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lifecycle {
    /// Binding and starting serve loops; health `NOT_SERVING`.
    #[default]
    Starting,
    /// Answering requests; health `SERVING`.
    Serving,
    /// Terminating: health `NOT_SERVING`, still answering, until
    /// `drain_grace` after `since`.
    Draining {
        /// When termination was requested.
        since: Monotonic,
        /// How long to keep answering.
        drain_grace: Duration,
    },
    /// Done; the process exits.
    Stopped,
}

/// What happened, as the runner tells the state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Every serve loop is ready to take requests.
    Ready,
    /// Termination requested (SIGTERM, SIGINT, or a test's shutdown),
    /// draining for `drain_grace` if serving.
    Terminate {
        /// How long to keep answering after `NOT_SERVING`.
        drain_grace: Duration,
    },
    /// Time passed (the runner ticks at [`Lifecycle::next_deadline`]).
    Tick,
    /// A serve loop or the server failed.
    Failed,
}

/// How the service ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Drained or stopped on request.
    Clean,
    /// Stopped because something failed.
    Failed,
}

/// What the runner must do, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Set the overall health status (`grpc.health.v1`, service `""`,
    /// and every served kind).
    SetHealth(ServingStatus),
    /// Stop accepting connections and requests; finish the ones in
    /// flight.
    StopAccepting,
    /// Return from the run with this outcome.
    Exit(Outcome),
}

impl Lifecycle {
    /// The state after `event` at `now`, and the actions to perform. Pure.
    pub fn on(self, event: Event, now: Monotonic) -> (Self, Vec<Action>) {
        use Action::{Exit, SetHealth, StopAccepting};
        let stop = |outcome| vec![StopAccepting, Exit(outcome)];
        match (self, event) {
            (Lifecycle::Stopped, _) => (Lifecycle::Stopped, Vec::new()),
            (_, Event::Failed) => (
                Lifecycle::Stopped,
                vec![
                    SetHealth(ServingStatus::NotServing),
                    StopAccepting,
                    Exit(Outcome::Failed),
                ],
            ),
            (Lifecycle::Starting, Event::Ready) => {
                (Lifecycle::Serving, vec![SetHealth(ServingStatus::Serving)])
            }
            (Lifecycle::Starting, Event::Terminate { .. }) => {
                (Lifecycle::Stopped, stop(Outcome::Clean))
            }
            (Lifecycle::Serving, Event::Terminate { drain_grace }) => (
                Lifecycle::Draining {
                    since: now,
                    drain_grace,
                },
                vec![SetHealth(ServingStatus::NotServing)],
            ),
            (Lifecycle::Draining { .. }, Event::Terminate { .. }) => {
                (Lifecycle::Stopped, stop(Outcome::Clean))
            }
            (Lifecycle::Draining { since, drain_grace }, Event::Tick)
                if now.since(since) >= drain_grace =>
            {
                (Lifecycle::Stopped, stop(Outcome::Clean))
            }
            (state, _) => (state, Vec::new()),
        }
    }

    /// When the runner must next send [`Event::Tick`]: the end of the drain
    /// while draining, otherwise never (`None`). Pure.
    pub fn next_deadline(&self) -> Option<Monotonic> {
        match self {
            Lifecycle::Draining { since, drain_grace } => Some(since.plus(*drain_grace)),
            _ => None,
        }
    }

    /// The health status this state reports.
    pub const fn health(&self) -> ServingStatus {
        match self {
            Lifecycle::Serving => ServingStatus::Serving,
            _ => ServingStatus::NotServing,
        }
    }

    /// Whether the service has stopped.
    pub const fn is_stopped(&self) -> bool {
        matches!(self, Lifecycle::Stopped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Action::{Exit, SetHealth, StopAccepting};

    const GRACE: Duration = Duration::from_secs(5);

    fn at(ms: u64) -> Monotonic {
        Monotonic::from_origin(Duration::from_millis(ms))
    }

    #[test]
    fn m3_sigterm_reports_not_serving_then_drains_for_the_grace_then_stops() {
        let s = Lifecycle::default();
        assert_eq!(s.health(), ServingStatus::NotServing);
        let (s, a) = s.on(Event::Ready, at(10));
        assert_eq!(
            (s, a),
            (Lifecycle::Serving, vec![SetHealth(ServingStatus::Serving)])
        );
        assert_eq!(s.health(), ServingStatus::Serving);
        assert_eq!(s.next_deadline(), None);

        let (s, a) = s.on(Event::Terminate { drain_grace: GRACE }, at(1_000));
        assert_eq!(
            s,
            Lifecycle::Draining {
                since: at(1_000),
                drain_grace: GRACE
            }
        );
        assert_eq!(a, vec![SetHealth(ServingStatus::NotServing)]);
        assert_eq!(s.next_deadline(), Some(at(6_000)));

        // Still draining just before the grace ends; requests are answered.
        let (s, a) = s.on(Event::Tick, at(5_999));
        assert!(a.is_empty());
        assert!(matches!(s, Lifecycle::Draining { .. }));
        // A late Ready changes nothing.
        let (s, a) = s.on(Event::Ready, at(5_999));
        assert!(a.is_empty());

        let (s, a) = s.on(Event::Tick, at(6_000));
        assert_eq!(s, Lifecycle::Stopped);
        assert_eq!(a, vec![StopAccepting, Exit(Outcome::Clean)]);
        assert!(s.is_stopped());

        // Stopped is final.
        for e in [
            Event::Ready,
            Event::Tick,
            Event::Failed,
            Event::Terminate { drain_grace: GRACE },
        ] {
            assert_eq!(s.on(e, at(9_999)), (Lifecycle::Stopped, Vec::new()));
        }
    }

    #[test]
    fn m3_second_terminate_or_terminate_before_serving_stops_at_once() {
        let (draining, _) = Lifecycle::Serving.on(Event::Terminate { drain_grace: GRACE }, at(0));
        assert_eq!(
            draining.on(Event::Terminate { drain_grace: GRACE }, at(1)),
            (
                Lifecycle::Stopped,
                vec![StopAccepting, Exit(Outcome::Clean)]
            )
        );
        assert_eq!(
            Lifecycle::Starting.on(Event::Terminate { drain_grace: GRACE }, at(0)),
            (
                Lifecycle::Stopped,
                vec![StopAccepting, Exit(Outcome::Clean)]
            )
        );
        // A zero grace stops on the first tick.
        let (s, _) = Lifecycle::Serving.on(
            Event::Terminate {
                drain_grace: Duration::ZERO,
            },
            at(7),
        );
        assert_eq!(s.on(Event::Tick, at(7)).0, Lifecycle::Stopped);
    }

    #[test]
    fn m3_failure_stops_from_every_live_state() {
        let failed = vec![
            SetHealth(ServingStatus::NotServing),
            StopAccepting,
            Exit(Outcome::Failed),
        ];
        for s in [
            Lifecycle::Starting,
            Lifecycle::Serving,
            Lifecycle::Draining {
                since: at(0),
                drain_grace: GRACE,
            },
        ] {
            assert_eq!(
                s.on(Event::Failed, at(1)),
                (Lifecycle::Stopped, failed.clone())
            );
        }
        // Ticks outside draining do nothing.
        assert_eq!(
            Lifecycle::Serving.on(Event::Tick, at(1)),
            (Lifecycle::Serving, Vec::new())
        );
        assert_eq!(
            Lifecycle::Starting.on(Event::Tick, at(1)),
            (Lifecycle::Starting, Vec::new())
        );
    }

    #[test]
    fn m3_monotonic_arithmetic_saturates() {
        assert_eq!(at(5).since(at(9)), Duration::ZERO);
        assert_eq!(at(9).since(at(5)), Duration::from_millis(4));
        assert_eq!(
            Monotonic::from_origin(Duration::MAX)
                .plus(GRACE)
                .since_origin(),
            Duration::MAX
        );
        assert_eq!(Monotonic::ZERO, at(0));
    }
}
