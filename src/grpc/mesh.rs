//! Discovered endpoints in an Istio mesh (feature `istio`): requests and
//! gathers routed to what a [`Directory`] holds ([`MeshClient`]), and a
//! browser that reports an endpoint up only while its health service says
//! `SERVING` ([`MeshBrowse`]).
//!
//! # Endpoints and the sidecar
//!
//! An endpoint is called at its *authority* ([`endpoint_authority`]):
//! `name:port` for an [`Endpoint::Host`] (a trailing dot of a DNS name is
//! dropped), `ip:port` for an [`Endpoint::Socket`]. Locators and local
//! endpoints have none and are skipped. A [`MeshDialer`] turns an authority
//! into a lazily connecting channel ([`MeshChannelBuilder`]): through the
//! sidecar when one is set (TCP to the sidecar, the authority as
//! `:authority`, so Envoy routes and originates mTLS), otherwise straight
//! to the authority.
//!
//! # Composing with DNS-SD
//!
//! `MeshBrowse<B>` wraps any [`Browse`]: the unicast DNS-SD browser
//! (`dns::unicast`, feature `dns-sd`) resolving the Kubernetes Service, the
//! mDNS socket, or the in-process bus in tests. `B` says where endpoints
//! are; `MeshBrowse` adds whether each is ready, so a [`Directory`] fed from
//! it (and a [`MeshClient`] using that directory) only holds endpoints that
//! answer. A typical stack:
//! `MeshBrowse::<UnicastDnsSd, 32>::new(dns, MeshDialer::via_sidecar("127.0.0.1:15001"))`,
//! its events observed into a `Directory`, handed to
//! [`MeshClient::with_directory`].

use super::GrpcError;
use super::client::{GrpcTransport, method_path};
use super::health::{HealthClient, HealthWatch, ServingStatus, health_name};
use super::istio::MeshChannelBuilder;
use crate::discovery::record::event_into;
use crate::discovery::{
    Browse, Directory, Endpoint, Event, RecordError, ServiceKind, ServiceRecord,
};
use crate::key::{Key, KeyFilter};
use crate::transport::{
    Flow, Gather, HandlerError, Reply, RequestOptions, Requester, SenderId, Target, copy_into,
};
use bytes::Bytes;
use core::future::{Future, poll_fn};
use core::pin::pin;
use core::task::{Context, Poll, Waker};
use core::time::Duration;
use http::uri::PathAndQuery;
use tonic::transport::Channel;

/// The bound on a [`MeshClient`] call or gather when
/// `RequestOptions::timeout` is `None`, and on opening a health watch in
/// [`MeshBrowse`].
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// The authority an endpoint is called at (module docs); `None` for
/// locators, local endpoints, port 0 and empty host names. Pure.
pub fn endpoint_authority(endpoint: &Endpoint<'_>) -> Option<String> {
    match endpoint {
        Endpoint::Socket(a) if a.port() != 0 => Some(a.to_string()),
        Endpoint::Host { name, port } if *port != 0 => {
            let name = name.strip_suffix('.').unwrap_or(name);
            (!name.is_empty()).then(|| format!("{name}:{port}"))
        }
        _ => None,
    }
}

/// How channels to endpoints are made: through a sidecar or directly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MeshDialer {
    sidecar: Option<String>,
    connect_timeout: Option<Duration>,
}

impl MeshDialer {
    /// Connects straight to each endpoint's authority (outside a mesh, or
    /// when the sidecar intercepts outbound traffic transparently).
    pub fn direct() -> Self {
        Self::default()
    }

    /// Connects every channel to the sidecar at `addr` (`host:port`), with
    /// the endpoint's authority as `:authority`.
    pub fn via_sidecar(addr: impl Into<String>) -> Self {
        MeshDialer {
            sidecar: Some(addr.into()),
            connect_timeout: None,
        }
    }

    /// Returns this dialer with a TCP connect timeout on every channel.
    pub fn with_connect_timeout(self, d: Duration) -> Self {
        MeshDialer {
            connect_timeout: Some(d),
            ..self
        }
    }

    /// The channel builder for `authority`; pure.
    pub fn builder(&self, authority: &str) -> MeshChannelBuilder {
        let b = MeshChannelBuilder::new(authority);
        let b = match &self.sidecar {
            Some(s) => b.via_sidecar(s.clone()),
            None => b,
        };
        match self.connect_timeout {
            Some(d) => b.connect_timeout(d),
            None => b,
        }
    }

    /// A channel to `authority` that connects on first use (needs a tokio
    /// runtime); [`GrpcError::InvalidHeader`] if it is not a valid
    /// authority.
    pub fn channel(&self, authority: &str) -> Result<Channel, GrpcError> {
        Ok(self.builder(authority).connect_lazy()?)
    }
}

/// The endpoints of `kind` in `directory` that `target` selects, as
/// (node, authority), in slot order. Pure.
pub fn targets<const N: usize>(
    directory: &Directory<N>,
    kind: ServiceKind<'_>,
    target: Target,
) -> Vec<(Option<SenderId>, String)> {
    directory
        .lookup(kind)
        .filter(|s| match target {
            Target::Any => true,
            Target::Node(id) => s.node == Some(id),
        })
        .filter_map(|s| endpoint_authority(&s.endpoint).map(|a| (s.node, a)))
        .collect()
}

/// A client for the services of one kind that a [`Directory`] holds,
/// implementing [`Requester`] and [`Gather`] over gRPC.
///
/// * [`Target::Any`] — requests go to the kind's endpoints in turn (round
///   robin over the directory's slot order).
/// * [`Target::Node`] — to the endpoint whose record names that node; with
///   none, the call fails at once with status `UNAVAILABLE`
///   ([`HandlerError::Unavailable`] through [`GrpcError::handler_error`]),
///   as it does when the kind has no endpoint at all.
/// * Every call carries the client id ([`MeshClient::client_id`]) in
///   `mw-client` and its timeout in `grpc-timeout`; with
///   `RequestOptions::timeout` `None` the bound is [`DEFAULT_TIMEOUT`].
///
/// Channels are made by the [`MeshDialer`] and cached by authority, at most
/// `N` (one per directory slot); a new one evicts first a channel whose
/// endpoint left the directory, else the oldest.
#[derive(Debug)]
pub struct MeshClient<const N: usize> {
    kind: String,
    directory: Directory<N>,
    dialer: MeshDialer,
    client_id: SenderId,
    channels: Vec<(String, Channel)>,
    turn: usize,
}

impl<const N: usize> MeshClient<N> {
    /// A client for `kind` with an empty directory and a fresh client id.
    pub fn new(kind: ServiceKind<'_>, dialer: MeshDialer) -> Self {
        MeshClient {
            kind: kind.as_str().to_owned(),
            directory: Directory::new(),
            dialer,
            client_id: super::client::fresh_client_id(),
            channels: Vec::new(),
            turn: 0,
        }
    }

    /// Returns this client routing to `directory` from now on.
    pub fn with_directory(self, directory: Directory<N>) -> Self {
        MeshClient { directory, ..self }
    }

    /// Returns this client identified as `id`.
    pub fn with_client_id(self, id: SenderId) -> Self {
        MeshClient {
            client_id: id,
            ..self
        }
    }

    /// The directory routed to.
    pub fn directory(&self) -> &Directory<N> {
        &self.directory
    }

    /// The id sent in `mw-client`.
    pub fn client_id(&self) -> SenderId {
        self.client_id
    }

    fn targets(&self, target: Target) -> Result<Vec<(Option<SenderId>, String)>, GrpcError> {
        let key = Key::new(&self.kind).map_err(|_| GrpcError::InvalidHeader)?;
        Ok(targets(&self.directory, ServiceKind::new(key), target))
    }

    /// A transport on the cached (or a new) channel to `authority`.
    fn transport(&mut self, authority: &str) -> Result<GrpcTransport<Channel>, GrpcError> {
        let channel = match self.channels.iter().find(|(a, _)| a == authority) {
            Some((_, c)) => c.clone(),
            None => {
                let c = self.dialer.channel(authority)?;
                if self.channels.len() >= N.max(1) {
                    let live = self.targets(Target::Any)?;
                    let stale = self
                        .channels
                        .iter()
                        .position(|(a, _)| !live.iter().any(|(_, l)| l == a))
                        .unwrap_or(0);
                    self.channels.remove(stale);
                }
                self.channels.push((authority.to_owned(), c.clone()));
                c
            }
        };
        Ok(GrpcTransport::new(channel).with_client_id(self.client_id))
    }

    fn no_endpoint(&self, target: Target) -> GrpcError {
        let message = match target {
            Target::Any => format!("no endpoint of {}", self.kind),
            Target::Node(id) => format!("no endpoint of {} on node {:02x?}", self.kind, id.0),
        };
        GrpcError::Status {
            code: tonic::Code::Unavailable,
            message,
        }
    }
}

impl<const N: usize> Requester for MeshClient<N> {
    type Error = GrpcError;

    /// One unary call on `/Service/Method` to the endpoint `options.target`
    /// selects (see [`MeshClient`]).
    async fn request_with<'b>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        response: &'b mut [u8],
        options: &RequestOptions,
    ) -> Result<&'b [u8], GrpcError> {
        let path = method_path(key)?;
        let targets = self.targets(options.target)?;
        let turn = self.turn;
        self.turn = self.turn.wrapping_add(1);
        let pick = match targets.len() {
            0 => None,
            n => targets.get(turn % n),
        };
        let (_, authority) = pick.ok_or_else(|| self.no_endpoint(options.target))?;
        let mut t = self.transport(authority)?;
        let timeout = options.timeout.unwrap_or(DEFAULT_TIMEOUT);
        let reply = t
            .call(path, Bytes::copy_from_slice(request), Some(timeout))
            .await?;
        copy_into(response, &reply)
            .map(|r| &*r)
            .ok_or(GrpcError::BufferTooSmall {
                needed: reply.len(),
            })
    }
}

/// One gather call; a transport that could not be built fails here.
async fn gather_one(
    transport: Result<GrpcTransport<Channel>, GrpcError>,
    path: PathAndQuery,
    body: Bytes,
    timeout: Duration,
) -> Result<Bytes, GrpcError> {
    transport?.call(path, body, Some(timeout)).await
}

impl<const N: usize> Gather for MeshClient<N> {
    type Error = GrpcError;

    /// Calls every endpoint `options.target` selects concurrently (one
    /// unary call each, through the dialer), and passes each reply to
    /// `on_reply` as it arrives, with [`Reply::sender`] the endpoint's
    /// node. A call that fails is a reply too: its status mapped to a
    /// [`HandlerError`] (a connection failure is `Unavailable`). Returns
    /// after `max_replies` replies, [`Flow::Stop`], every endpoint
    /// answering, or the timeout (default [`DEFAULT_TIMEOUT`]), whichever
    /// comes first; calls still running are dropped (cancelled). No
    /// endpoint, or `max_replies` 0, returns 0 at once.
    async fn request_each<F>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        buf: &mut [u8],
        options: &RequestOptions,
        max_replies: usize,
        mut on_reply: F,
    ) -> Result<usize, GrpcError>
    where
        F: FnMut(Reply<'_>) -> Flow,
    {
        let path = method_path(key)?;
        if max_replies == 0 {
            return Ok(0);
        }
        let timeout = options.timeout.unwrap_or(DEFAULT_TIMEOUT);
        // Started before the calls, so it fires before their own timers.
        let mut expired = pin!(tokio::time::sleep(timeout));
        let body = Bytes::copy_from_slice(request);
        let mut calls = Vec::new();
        for (node, authority) in self.targets(options.target)? {
            let t = self.transport(&authority);
            calls.push((
                node,
                Some(Box::pin(gather_one(t, path.clone(), body.clone(), timeout))),
            ));
        }
        let mut seen = 0;
        // Each iteration finishes one call or ends the gather.
        while seen < max_replies && calls.iter().any(|(_, c)| c.is_some()) {
            let done = poll_fn(|cx| {
                if expired.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(None);
                }
                for (node, call) in calls.iter_mut() {
                    if let Some(f) = call.as_mut()
                        && let Poll::Ready(r) = f.as_mut().poll(cx)
                    {
                        *call = None;
                        return Poll::Ready(Some((*node, r)));
                    }
                }
                Poll::Pending
            })
            .await;
            let Some((sender, outcome)) = done else { break };
            let result = match &outcome {
                Ok(bytes) => copy_into(buf, bytes)
                    .map(|b| &*b)
                    .ok_or(HandlerError::BufferTooSmall),
                Err(e) => Err(e.handler_error().unwrap_or(HandlerError::Unavailable)),
            };
            seen += 1;
            if on_reply(Reply { sender, result }) == Flow::Stop {
                break;
            }
        }
        Ok(seen)
    }
}

/// Failure of [`MeshBrowse`].
#[derive(Debug)]
pub enum MeshBrowseError<E> {
    /// The wrapped browser failed.
    Browse(E),
    /// The event did not fit the caller's buffer; it is kept for the next
    /// call.
    Record(RecordError),
}

/// Health watch of one endpoint.
#[derive(Debug)]
enum WatchState {
    /// To be opened by the next `next`.
    Due,
    /// Open (boxed: a stream is large next to the other states).
    Open(Box<HealthWatch>),
    /// Failed or ended; reopened when the browser reports the endpoint
    /// again.
    Ended,
}

/// One tracked endpoint.
#[derive(Debug)]
struct Entry {
    record: ServiceRecord,
    authority: String,
    watch: WatchState,
    /// The last status the watch reported (`None` before the first).
    status: Option<ServingStatus>,
    /// `Up` was reported and no `Down` since.
    announced: bool,
    /// The record changed since `Up` was reported.
    dirty: bool,
    /// The browser reported it `Down`.
    gone: bool,
}

impl Entry {
    /// Whether the endpoint should be reported up.
    fn wanted(&self) -> bool {
        !self.gone
            && matches!(self.watch, WatchState::Open(_))
            && self.status == Some(ServingStatus::Serving)
    }

    fn is(&self, kind: ServiceKind<'_>, instance: &str) -> bool {
        self.record.is(kind, instance)
    }
}

/// What the table owes its caller next: `(index, up)` for the first entry
/// whose reported state differs from the wanted one. Pure.
fn next_emission(entries: &[Entry]) -> Option<(usize, bool)> {
    entries.iter().enumerate().find_map(|(i, e)| {
        let want = e.wanted();
        if want && (!e.announced || e.dirty) {
            Some((i, true))
        } else if !want && e.announced {
            Some((i, false))
        } else {
            None
        }
    })
}

/// Applies an event of the wrapped browser to the table (at most `cap`
/// entries).
fn observe(entries: &mut Vec<Entry>, event: &Event<'_>, cap: usize) {
    let s = match event {
        Event::Up(s) if s.ttl_s > 0 => s,
        _ => {
            if let Some(e) = entries
                .iter_mut()
                .find(|e| e.is(event.kind(), event.instance()))
            {
                e.gone = true;
                e.watch = WatchState::Ended;
                e.status = None;
            }
            return;
        }
    };
    let held = entries.len();
    let found = entries.iter_mut().find(|e| e.is(s.kind, s.instance));
    let (Some(authority), Some(record)) = (endpoint_authority(&s.endpoint), ServiceRecord::new(s))
    else {
        // Moved somewhere it cannot be called, or out of bounds: down.
        if let Some(e) = found {
            e.gone = true;
            e.watch = WatchState::Ended;
        }
        return;
    };
    match found {
        Some(e) => {
            e.gone = false;
            if e.authority != authority {
                e.authority = authority;
                e.watch = WatchState::Due;
                e.status = None;
            } else if matches!(e.watch, WatchState::Ended) {
                e.watch = WatchState::Due;
            }
            if e.record != record {
                e.record = record;
                e.dirty = true;
            }
        }
        None if held < cap => entries.push(Entry {
            record,
            authority,
            watch: WatchState::Due,
            status: None,
            announced: false,
            dirty: false,
            gone: false,
        }),
        None => {}
    }
}

/// Polls every open watch once; the first ready one, with its index.
fn poll_watches(
    entries: &mut [Entry],
    cx: &mut Context<'_>,
) -> Poll<(usize, Result<ServingStatus, GrpcError>)> {
    for (i, e) in entries.iter_mut().enumerate() {
        if let WatchState::Open(w) = &mut e.watch
            && let Poll::Ready(r) = w.poll_status(cx)
        {
            return Poll::Ready((i, r));
        }
    }
    Poll::Pending
}

/// Records what watch `i` reported.
fn apply_status(entries: &mut [Entry], i: usize, r: Result<ServingStatus, GrpcError>) {
    if let Some(e) = entries.get_mut(i) {
        match r {
            Ok(s) => e.status = Some(s),
            Err(_) => {
                e.watch = WatchState::Ended;
                e.status = None;
            }
        }
    }
}

/// Room for one event of the wrapped browser.
pub const SCRATCH_LEN: usize = 1024;

/// A [`Browse`] that reports a service `Up` only while its endpoint's
/// `grpc.health.v1.Health` says `SERVING` for the kind ([`health_name`]).
///
/// For each `Up` from the wrapped browser `B` it opens a health `Watch` on
/// the endpoint (through the [`MeshDialer`]) and reports:
///
/// * `Up` (with `B`'s record) when the watch reports `SERVING`, and again
///   when `B` changes the record while it is up;
/// * `Down` when the watch reports anything else, when the watch fails or
///   ends (the server went away), and when `B` reports `Down`.
///
/// Endpoints without an authority ([`endpoint_authority`]) are never
/// reported. An endpoint whose watch ended is watched again when `B`
/// reports it `Up` again (a DNS-SD re-query). At most `N` endpoints are
/// tracked; further ones are ignored until a slot frees.
///
/// [`next`](Browse::next) opens due watches before it waits (each bounded
/// by [`DEFAULT_TIMEOUT`]); [`try_next`](Browse::try_next) cannot open
/// them, so an endpoint first seen by `try_next` is reported once a later
/// `next` has opened its watch. Both are cancel safe: an event is only
/// taken off the table once it has been copied into the caller's buffer.
#[derive(Debug)]
pub struct MeshBrowse<B, const N: usize = 16> {
    inner: B,
    dialer: MeshDialer,
    entries: Vec<Entry>,
    scratch: Vec<u8>,
}

impl<B: Browse, const N: usize> MeshBrowse<B, N> {
    /// Wraps `inner`, dialing endpoints with `dialer`.
    pub fn new(inner: B, dialer: MeshDialer) -> Self {
        MeshBrowse {
            inner,
            dialer,
            entries: Vec::new(),
            scratch: vec![0; SCRATCH_LEN],
        }
    }

    /// The wrapped browser.
    pub fn inner(&self) -> &B {
        &self.inner
    }

    /// Endpoints tracked (up, waiting for health, or owed a `Down`).
    pub fn tracked(&self) -> usize {
        self.entries.len()
    }

    /// Copies emission `(i, up)` into `buf`, then records it as reported.
    fn emit<'b>(
        &mut self,
        i: usize,
        up: bool,
        buf: &'b mut [u8],
    ) -> Result<Event<'b>, MeshBrowseError<B::Error>> {
        let Some(e) = self.entries.get_mut(i) else {
            return Err(MeshBrowseError::Record(RecordError::BufferTooSmall));
        };
        let service = e.record.service();
        let event = if up {
            Event::Up(service)
        } else {
            Event::Down {
                kind: service.kind,
                instance: service.instance,
            }
        };
        let out = event_into(buf, &event).map_err(MeshBrowseError::Record)?;
        e.announced = up;
        e.dirty = false;
        if !up && e.gone {
            self.entries.remove(i);
        }
        Ok(out)
    }

    /// Forgets entries that are gone and owe nothing.
    fn collect(&mut self) {
        self.entries.retain(|e| !e.gone || e.announced);
    }

    /// Opens the watch of every entry that is due one.
    async fn open_due(&mut self) {
        for e in self.entries.iter_mut() {
            if !matches!(e.watch, WatchState::Due) {
                continue;
            }
            let name = health_name(e.record.service().kind).to_owned();
            let opened = match self.dialer.channel(&e.authority) {
                Ok(channel) => {
                    let mut client = HealthClient::new(channel);
                    tokio::time::timeout(DEFAULT_TIMEOUT, client.watch(&name))
                        .await
                        .ok()
                        .and_then(Result::ok)
                }
                Err(_) => None,
            };
            e.status = None;
            e.watch = opened.map_or(WatchState::Ended, |w| WatchState::Open(Box::new(w)));
        }
    }
}

impl<B: Browse, const N: usize> Browse for MeshBrowse<B, N> {
    type Error = MeshBrowseError<B::Error>;

    /// Browses `filter` with the wrapped browser; tracked endpoints stay.
    async fn browse(&mut self, filter: KeyFilter<'_>) -> Result<(), Self::Error> {
        self.inner
            .browse(filter)
            .await
            .map_err(MeshBrowseError::Browse)
    }

    async fn next<'b>(&mut self, buf: &'b mut [u8]) -> Result<Event<'b>, Self::Error> {
        // Each iteration applies one browser event or watch status, or
        // returns; it runs until something is owed to the caller.
        loop {
            if let Some((i, up)) = next_emission(&self.entries) {
                return self.emit(i, up, buf);
            }
            self.collect();
            self.open_due().await;
            if let Some((i, up)) = next_emission(&self.entries) {
                return self.emit(i, up, buf);
            }
            let entries = &mut self.entries;
            tokio::select! {
                event = self.inner.next(&mut self.scratch) => {
                    let event = event.map_err(MeshBrowseError::Browse)?;
                    observe(entries, &event, N);
                }
                (i, r) = poll_fn(|cx| poll_watches(entries, cx)) => {
                    apply_status(entries, i, r);
                }
            }
        }
    }

    fn try_next<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Event<'b>>, Self::Error> {
        if let Some((i, up)) = next_emission(&self.entries) {
            return self.emit(i, up, buf).map(Some);
        }
        if let Some(event) = self
            .inner
            .try_next(&mut self.scratch)
            .map_err(MeshBrowseError::Browse)?
        {
            observe(&mut self.entries, &event, N);
        }
        let mut cx = Context::from_waker(Waker::noop());
        for i in 0..self.entries.len() {
            let ready = match self.entries.get_mut(i).map(|e| &mut e.watch) {
                Some(WatchState::Open(w)) => match w.poll_status(&mut cx) {
                    Poll::Ready(r) => Some(r),
                    Poll::Pending => None,
                },
                _ => None,
            };
            if let Some(r) = ready {
                apply_status(&mut self.entries, i, r);
            }
        }
        match next_emission(&self.entries) {
            Some((i, up)) => self.emit(i, up, buf).map(Some),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::Service;

    const KIND: ServiceKind<'static> = ServiceKind::from_static("cam/jpeg");

    fn svc(instance: &'static str, node: u8, endpoint: Endpoint<'static>) -> Service<'static> {
        Service {
            kind: KIND,
            instance,
            node: Some(SenderId([node; 8])),
            endpoint,
            txt: b"",
            ttl_s: 60,
        }
    }

    #[test]
    fn i7_endpoint_authorities() {
        let sock = Endpoint::Socket("127.0.0.1:50051".parse().unwrap());
        assert_eq!(
            endpoint_authority(&sock).as_deref(),
            Some("127.0.0.1:50051")
        );
        let v6 = Endpoint::Socket("[::1]:7".parse().unwrap());
        assert_eq!(endpoint_authority(&v6).as_deref(), Some("[::1]:7"));
        let host = Endpoint::Host {
            name: "cam.lab.svc.cluster.local.",
            port: 9090,
        };
        assert_eq!(
            endpoint_authority(&host).as_deref(),
            Some("cam.lab.svc.cluster.local:9090")
        );
        for none in [
            Endpoint::Host { name: "", port: 1 },
            Endpoint::Host { name: "a", port: 0 },
            Endpoint::Socket("127.0.0.1:0".parse().unwrap()),
            Endpoint::Locator("tcp/10.0.0.2:7447"),
            Endpoint::Local,
        ] {
            assert_eq!(endpoint_authority(&none), None, "{none:?}");
        }
        let b =
            MeshDialer::via_sidecar("127.0.0.1:15001").builder("cam.lab.svc.cluster.local:9090");
        assert_eq!(b.authority(), "cam.lab.svc.cluster.local:9090");
        assert_eq!(
            b.endpoint().unwrap().uri().to_string(),
            "http://127.0.0.1:15001/"
        );
    }

    #[test]
    fn i7_targets_select_by_kind_and_node() {
        let d = Directory::<4>::new();
        let a = svc("a", 1, Endpoint::Socket("127.0.0.1:1001".parse().unwrap()));
        let b = svc(
            "b",
            2,
            Endpoint::Host {
                name: "b.ns.svc.cluster.local",
                port: 80,
            },
        );
        let c = svc("c", 3, Endpoint::Local);
        let other = Service {
            kind: ServiceKind::from_static("other/kind"),
            ..svc("o", 4, Endpoint::Socket("127.0.0.1:1004".parse().unwrap()))
        };
        let (d, _) = d.observe(&Event::Up(a), 0);
        let (d, _) = d.observe(&Event::Up(b), 0);
        let (d, _) = d.observe(&Event::Up(c), 0);
        let (d, _) = d.observe(&Event::Up(other), 0);
        let all = targets(&d, KIND, Target::Any);
        assert_eq!(
            all,
            [
                (Some(SenderId([1; 8])), "127.0.0.1:1001".to_owned()),
                (
                    Some(SenderId([2; 8])),
                    "b.ns.svc.cluster.local:80".to_owned()
                ),
            ]
        );
        let two = targets(&d, KIND, Target::Node(SenderId([2; 8])));
        assert_eq!(two.len(), 1);
        assert!(
            targets(&d, KIND, Target::Node(SenderId([3; 8]))).is_empty(),
            "local"
        );
        assert!(targets(&d, KIND, Target::Node(SenderId([9; 8]))).is_empty());
    }

    #[tokio::test]
    async fn i7_node_target_without_endpoint_is_unavailable_at_once() {
        let mut c = MeshClient::<4>::new(KIND, MeshDialer::direct());
        let mut buf = [0u8; 8];
        let opts = RequestOptions::DEFAULT.with_target(Target::Node(SenderId([7; 8])));
        let e = c
            .request_with(Key::from_static("cam/jpeg"), b"", &mut buf, &opts)
            .await
            .unwrap_err();
        assert_eq!(e.handler_error(), Some(HandlerError::Unavailable));
        let n = c
            .request_each(Key::from_static("s/m"), b"", &mut buf, &opts, 4, |_| {
                Flow::Continue
            })
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    fn entry(instance: &'static str) -> Entry {
        let s = svc(
            instance,
            1,
            Endpoint::Socket("127.0.0.1:1".parse().unwrap()),
        );
        Entry {
            record: ServiceRecord::new(&s).unwrap(),
            authority: "127.0.0.1:1".to_owned(),
            watch: WatchState::Due,
            status: None,
            announced: false,
            dirty: false,
            gone: false,
        }
    }

    #[test]
    fn i8_table_reports_up_only_while_serving_and_bounds_entries() {
        let mut t = Vec::new();
        let up = Event::Up(svc(
            "a",
            1,
            Endpoint::Socket("127.0.0.1:1".parse().unwrap()),
        ));
        observe(&mut t, &up, 2);
        assert_eq!(t.len(), 1);
        assert!(matches!(t[0].watch, WatchState::Due));
        // No watch yet: nothing owed.
        assert_eq!(next_emission(&t), None);
        // Down from the browser before any Up was reported: forgotten.
        observe(
            &mut t,
            &Event::Down {
                kind: KIND,
                instance: "a",
            },
            2,
        );
        assert!(t[0].gone && next_emission(&t).is_none());
        // Bounded: a third endpoint is ignored.
        let mut t = vec![entry("a"), entry("b")];
        let c = Event::Up(svc(
            "c",
            3,
            Endpoint::Socket("127.0.0.1:3".parse().unwrap()),
        ));
        observe(&mut t, &c, 2);
        assert_eq!(t.len(), 2);
        // A watch that ended is due again when the browser reports it.
        t[0].watch = WatchState::Ended;
        observe(
            &mut t,
            &Event::Up(svc(
                "a",
                1,
                Endpoint::Socket("127.0.0.1:1".parse().unwrap()),
            )),
            2,
        );
        assert!(matches!(t[0].watch, WatchState::Due));
        // Moving the endpoint re-watches; a changed record is dirty.
        observe(
            &mut t,
            &Event::Up(svc(
                "a",
                1,
                Endpoint::Socket("127.0.0.1:9".parse().unwrap()),
            )),
            2,
        );
        assert_eq!((t[0].authority.as_str(), t[0].dirty), ("127.0.0.1:9", true));
        // An announced entry whose status drops is owed a Down.
        t[1].announced = true;
        t[1].status = Some(ServingStatus::NotServing);
        assert_eq!(next_emission(&t), Some((1, false)));
        // A TTL-0 Up is a goodbye.
        let bye = Service {
            ttl_s: 0,
            ..svc("b", 1, Endpoint::Local)
        };
        observe(&mut t, &Event::Up(bye), 2);
        assert!(t[1].gone);
        // Moved to a locator: down.
        observe(&mut t, &Event::Up(svc("a", 1, Endpoint::Locator("x"))), 2);
        assert!(t[0].gone);
    }

    #[test]
    fn i8_status_errors_end_the_watch() {
        let mut t = vec![entry("a")];
        apply_status(&mut t, 0, Ok(ServingStatus::Serving));
        assert_eq!(t[0].status, Some(ServingStatus::Serving));
        assert!(!t[0].wanted(), "no open watch");
        apply_status(&mut t, 0, Err(GrpcError::StreamEnded));
        assert!(matches!(t[0].watch, WatchState::Ended));
        assert_eq!(t[0].status, None);
        apply_status(&mut t, 5, Ok(ServingStatus::Serving)); // stale index: ignored
    }
}
