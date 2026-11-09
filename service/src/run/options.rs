//! [`RunOptions`]: what an embedding or a test sets about a run beyond
//! the deployment [`Config`](crate::Config): a listener it already bound,
//! where discovery queries go, how downstream endpoints are dialled, the
//! stop bound, and whether a sidecar fronts the server.
//!
//! [`Service::run_with`](crate::Service::run_with) uses
//! [`RunOptions::default`]: bind `0.0.0.0:<port>`, nameserver from
//! `/etc/resolv.conf`, [`default_dialer`], [`DEFAULT_STOP_TIMEOUT`],
//! callers authenticated by the Istio sidecar.

use std::net::SocketAddr;
use std::time::Duration;

use middleware::dns::unicast::Schedule;
use middleware::grpc::mesh::MeshDialer;
use tokio::net::TcpListener;

/// How long stopping may take once the drain grace is over: in-flight
/// requests finishing, then the trace flush. The pod's
/// `terminationGracePeriodSeconds` must exceed the drain grace plus this
/// plus [`FLUSH_TIMEOUT`].
pub const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Bound on flushing buffered spans to the OTLP collector at exit.
pub const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// TCP connect timeout of the [`default_dialer`].
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// How downstream endpoints are dialled unless [`RunOptions::dialer`] says
/// otherwise: straight to each endpoint's authority (the pod IP and port
/// that DNS-SD resolved), with a [`CONNECT_TIMEOUT`].
///
/// Direct is what works with Istio's traffic capture (istio-cni or the
/// init container): iptables redirects every outbound connection of the
/// pod to the sidecar's outbound listener, which originates mTLS to the
/// destination it was headed for. Dialling the sidecar port explicitly
/// ([`MeshDialer::via_sidecar`]) instead reaches that listener without an
/// original destination, so it is only for sidecars that run as an
/// explicit proxy.
pub fn default_dialer() -> MeshDialer {
    MeshDialer::direct().with_connect_timeout(CONNECT_TIMEOUT)
}

/// Settings of one run (module docs). Built by value.
#[derive(Debug, Default)]
pub struct RunOptions {
    pub(crate) listener: Option<TcpListener>,
    pub(crate) nameserver: Option<SocketAddr>,
    pub(crate) dns_schedule: Option<Schedule>,
    pub(crate) dialer: Option<MeshDialer>,
    pub(crate) stop_timeout: Option<Duration>,
    pub(crate) without_sidecar: bool,
}

impl RunOptions {
    /// The defaults (module docs).
    pub fn new() -> Self {
        Self::default()
    }

    /// Serves on `listener` instead of binding `0.0.0.0:<port>` (tests bind
    /// `127.0.0.1:0` and read the port back).
    pub fn listener(self, listener: TcpListener) -> Self {
        RunOptions {
            listener: Some(listener),
            ..self
        }
    }

    /// Sends discovery queries to `nameserver` instead of the first
    /// nameserver of `/etc/resolv.conf`.
    pub fn nameserver(self, nameserver: SocketAddr) -> Self {
        RunOptions {
            nameserver: Some(nameserver),
            ..self
        }
    }

    /// Re-resolves called kinds on `schedule` instead of
    /// [`Schedule::DEFAULT`].
    pub fn dns_schedule(self, schedule: Schedule) -> Self {
        RunOptions {
            dns_schedule: Some(schedule),
            ..self
        }
    }

    /// Dials downstream endpoints with `dialer` instead of
    /// [`default_dialer`].
    pub fn dialer(self, dialer: MeshDialer) -> Self {
        RunOptions {
            dialer: Some(dialer),
            ..self
        }
    }

    /// Bounds stopping by `timeout` instead of [`DEFAULT_STOP_TIMEOUT`].
    pub fn stop_timeout(self, timeout: Duration) -> Self {
        RunOptions {
            stop_timeout: Some(timeout),
            ..self
        }
    }

    /// No sidecar fronts the server (tests on loopback, a process outside
    /// the mesh): `RequestMeta::principal` is always `None` (an
    /// `x-forwarded-client-cert` header is not trusted) and the in-process
    /// `allow_callers` check is off. Inside the mesh never use this: the
    /// check is the second line behind the AuthorizationPolicy.
    pub fn without_sidecar(self) -> Self {
        RunOptions {
            without_sidecar: true,
            ..self
        }
    }

    pub(crate) fn stop_timeout_or_default(&self) -> Duration {
        self.stop_timeout.unwrap_or(DEFAULT_STOP_TIMEOUT)
    }

    pub(crate) fn dialer_or_default(&self) -> MeshDialer {
        self.dialer.clone().unwrap_or_else(default_dialer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m6_options_default_and_override() {
        let o = RunOptions::new();
        assert!(o.listener.is_none() && o.nameserver.is_none() && !o.without_sidecar);
        assert_eq!(o.stop_timeout_or_default(), DEFAULT_STOP_TIMEOUT);
        assert_eq!(o.dialer_or_default(), default_dialer());
        let ns: SocketAddr = "127.0.0.1:5353".parse().unwrap();
        let o = RunOptions::new()
            .nameserver(ns)
            .dns_schedule(Schedule::new(10, 20))
            .dialer(MeshDialer::via_sidecar("127.0.0.1:15001"))
            .stop_timeout(Duration::from_millis(300))
            .without_sidecar();
        assert_eq!(o.nameserver, Some(ns));
        assert_eq!(o.dns_schedule, Some(Schedule::new(10, 20)));
        assert_eq!(
            o.dialer_or_default(),
            MeshDialer::via_sidecar("127.0.0.1:15001")
        );
        assert_eq!(o.stop_timeout_or_default(), Duration::from_millis(300));
        assert!(o.without_sidecar);
    }
}
