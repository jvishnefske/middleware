//! [`UnicastDnsSd`]: the tokio UDP shell around the pure parts of
//! [`dns::unicast`](super). See the parent module for the behaviour.

use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use core::time::Duration;
use std::io;

use heapless::{String, Vec};
use tokio::net::UdpSocket;
use tokio::time::Instant;

use super::answer::{
    AnswerError, Found, Lookup, Query, Verdict, address_answer, check_reply, srv_answer,
};
use super::map::{MAX_SRV_NAME_LEN, MapError, ServiceMap};
use super::resolv::ResolvConf;
use super::tracker::{Diff, Schedule, Tracker};
use crate::discovery::directory::MAX_KIND_LEN;
use crate::discovery::record::event_into;
use crate::discovery::{Browse, Event, RecordError, Service, ServiceKind};
use crate::dns::wire::{
    FLAG_RD, HEADER_LEN, MAX_MESSAGE_LEN, MAX_NAME_LEN, Message, MessageWriter, TYPE_A, TYPE_AAAA,
    TYPE_SRV, TextName, WireError,
};
use crate::key::{AsKey, KeyBuf, KeyFilter};

/// Endpoints tracked per browsed kind (pods of one headless Service).
pub const MAX_ENDPOINTS: usize = 16;
/// UDP payload size advertised with EDNS(0) (RFC 6891; the 2020 DNS flag
/// day value, which avoids IP fragmentation).
pub const EDNS_UDP_PAYLOAD: u16 = 1232;
/// Resolution rounds one [`Browse::next`] runs without an event before it
/// returns [`UnicastError::QuietLimit`].
pub const MAX_QUIET_ROUNDS: usize = 64;
/// Per-attempt reply timeout when neither the builder nor a resolver
/// configuration sets one.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);
/// Attempts per query when neither the builder nor a resolver configuration
/// sets them.
pub const DEFAULT_ATTEMPTS: u8 = 2;
/// The resolver configuration read when no nameserver is given.
pub const RESOLV_CONF_PATH: &str = "/etc/resolv.conf";

/// Largest query sent: header, name, type and class, and the OPT record.
const MAX_QUERY_LEN: usize = HEADER_LEN + MAX_NAME_LEN + 4 + 11;

/// A DNS query id from the operating system's CSPRNG (`getrandom`): 16
/// unpredictable bits per query, with no generator state kept in the
/// process for an observer of earlier ids to extrapolate from.
pub fn query_id() -> Result<u16, UnicastError> {
    let mut id = [0u8; 2];
    getrandom::fill(&mut id).map_err(|e| {
        UnicastError::Io(e.raw_os_error().map_or_else(
            || io::Error::other("getrandom failed"),
            io::Error::from_raw_os_error,
        ))
    })?;
    Ok(u16::from_ne_bytes(id))
}

/// Failure of a [`UnicastDnsSd`].
#[derive(Debug)]
#[non_exhaustive]
pub enum UnicastError {
    /// The socket, reading the resolver configuration, or the operating
    /// system's random number generator failed.
    Io(io::Error),
    /// A query could not be encoded or a reply decoded.
    Wire(WireError),
    /// No reply within the timeout on any attempt.
    Timeout,
    /// The reply has the TC bit: the answer does not fit UDP, and DNS over
    /// TCP is not supported.
    Truncated,
    /// The reply's response code is an error (SERVFAIL, REFUSED, ...), or a
    /// name in it could not be rendered.
    Answer(AnswerError),
    /// No nameserver was given and the resolver configuration names none.
    NoNameserver,
    /// [`Browse::browse`] selected no kind of the [`ServiceMap`].
    Unmapped,
    /// [`Browse::next`] before [`Browse::browse`].
    NotBrowsing,
    /// [`MAX_QUIET_ROUNDS`] resolutions in one [`Browse::next`] changed
    /// nothing; call it again.
    QuietLimit,
    /// The SRV name could not be built.
    Map(MapError),
    /// An event does not fit the caller's buffer; it is kept.
    Record(RecordError),
}

impl core::fmt::Display for UnicastError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            UnicastError::Io(e) => write!(f, "dns-sd socket error: {e}"),
            UnicastError::Wire(e) => write!(f, "malformed dns message: {e:?}"),
            UnicastError::Timeout => f.write_str("no reply from the nameserver"),
            UnicastError::Truncated => f.write_str("truncated reply (TCP fallback unsupported)"),
            UnicastError::Answer(e) => write!(f, "dns answer: {e:?}"),
            UnicastError::NoNameserver => f.write_str("no nameserver configured"),
            UnicastError::Unmapped => f.write_str("no mapped kind matches the filter"),
            UnicastError::NotBrowsing => f.write_str("next before browse"),
            UnicastError::QuietLimit => {
                write!(f, "{MAX_QUIET_ROUNDS} resolutions without a change")
            }
            UnicastError::Map(e) => write!(f, "service map: {e:?}"),
            UnicastError::Record(e) => write!(f, "discovery record: {e:?}"),
        }
    }
}

impl std::error::Error for UnicastError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            UnicastError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for UnicastError {
    fn from(e: io::Error) -> Self {
        UnicastError::Io(e)
    }
}

impl From<WireError> for UnicastError {
    fn from(e: WireError) -> Self {
        UnicastError::Wire(e)
    }
}

impl From<AnswerError> for UnicastError {
    fn from(e: AnswerError) -> Self {
        UnicastError::Answer(e)
    }
}

/// Configures a [`UnicastDnsSd`]; see the parent module. Built by value.
#[derive(Debug, Clone)]
pub struct UnicastDnsSdBuilder<const M: usize> {
    map: ServiceMap<M>,
    nameserver: Option<SocketAddr>,
    conf: Option<ResolvConf>,
    bind: Option<IpAddr>,
    timeout: Option<Duration>,
    attempts: Option<u8>,
    schedule: Schedule,
}

impl<const M: usize> UnicastDnsSdBuilder<M> {
    /// Sends queries to `nameserver` instead of the resolver
    /// configuration's first one.
    pub fn nameserver(self, nameserver: SocketAddr) -> Self {
        UnicastDnsSdBuilder {
            nameserver: Some(nameserver),
            ..self
        }
    }

    /// Uses `conf` instead of reading [`RESOLV_CONF_PATH`].
    pub fn resolv_conf(self, conf: ResolvConf) -> Self {
        UnicastDnsSdBuilder {
            conf: Some(conf),
            ..self
        }
    }

    /// The local address each query's socket binds, on a fresh ephemeral
    /// port (default: the unspecified address of the nameserver's family).
    pub fn bind(self, bind: IpAddr) -> Self {
        UnicastDnsSdBuilder {
            bind: Some(bind),
            ..self
        }
    }

    /// How long to wait for each attempt's reply.
    pub fn timeout(self, timeout: Duration) -> Self {
        UnicastDnsSdBuilder {
            timeout: Some(timeout),
            ..self
        }
    }

    /// How many times a query is sent, each with a fresh id (at least 1).
    pub fn attempts(self, attempts: u8) -> Self {
        UnicastDnsSdBuilder {
            attempts: Some(attempts.max(1)),
            ..self
        }
    }

    /// When to resolve again.
    pub fn schedule(self, schedule: Schedule) -> Self {
        UnicastDnsSdBuilder { schedule, ..self }
    }

    /// Resolves the configuration. Sockets are bound per query, so nothing
    /// is bound here. Reads
    /// [`RESOLV_CONF_PATH`] only if neither a nameserver nor a
    /// configuration was given. Timeout and attempts: the builder's, else
    /// the configuration's (when one was given or read), else
    /// [`DEFAULT_TIMEOUT`] and [`DEFAULT_ATTEMPTS`].
    pub async fn build(self) -> Result<UnicastDnsSd<M>, UnicastError> {
        let conf = match (self.conf, self.nameserver) {
            (Some(c), _) => Some(c),
            (None, Some(_)) => None,
            (None, None) => Some(ResolvConf::parse(&std::fs::read_to_string(
                RESOLV_CONF_PATH,
            )?)),
        };
        let server = self
            .nameserver
            .or_else(|| conf.as_ref()?.nameservers().first().copied())
            .ok_or(UnicastError::NoNameserver)?;
        let timeout = self
            .timeout
            .or(conf.as_ref().map(ResolvConf::timeout))
            .unwrap_or(DEFAULT_TIMEOUT);
        let attempts = self
            .attempts
            .or(conf.as_ref().map(ResolvConf::attempts))
            .unwrap_or(DEFAULT_ATTEMPTS)
            .max(1);
        let bind = self.bind.unwrap_or(match server.ip() {
            IpAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
            IpAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
        });
        Ok(UnicastDnsSd {
            bind,
            server,
            timeout,
            attempts,
            schedule: self.schedule,
            map: self.map,
            start: Instant::now(),
            kinds: Vec::new(),
            pending: Diff::default(),
            rejected: 0,
            rx: [0; MAX_MESSAGE_LEN + 1],
        })
    }
}

/// One browsed kind.
#[derive(Debug, Clone)]
struct Browsed {
    kind: KeyBuf<MAX_KIND_LEN>,
    qname: String<MAX_SRV_NAME_LEN>,
    tracker: Tracker<MAX_ENDPOINTS>,
}

/// Unicast DNS-SD over UDP; see the parent module.
#[derive(Debug)]
pub struct UnicastDnsSd<const M: usize = { super::map::DEFAULT_MAP_CAPACITY }> {
    /// Local address of each query's socket (port 0: ephemeral).
    bind: IpAddr,
    server: SocketAddr,
    timeout: Duration,
    attempts: u8,
    schedule: Schedule,
    map: ServiceMap<M>,
    start: Instant,
    kinds: Vec<Browsed, M>,
    /// Events of the last resolution not yet delivered.
    pending: Diff<MAX_ENDPOINTS>,
    rejected: usize,
    /// One byte over the largest message, to detect oversized datagrams.
    rx: [u8; MAX_MESSAGE_LEN + 1],
}

impl<const M: usize> UnicastDnsSd<M> {
    /// A builder browsing the kinds of `map`.
    pub fn builder(map: ServiceMap<M>) -> UnicastDnsSdBuilder<M> {
        UnicastDnsSdBuilder {
            map,
            nameserver: None,
            conf: None,
            bind: None,
            timeout: None,
            attempts: None,
            schedule: Schedule::DEFAULT,
        }
    }

    /// The local address query sockets bind (each on a fresh port).
    pub const fn bind_addr(&self) -> IpAddr {
        self.bind
    }

    /// Where queries go.
    pub const fn nameserver(&self) -> SocketAddr {
        self.server
    }

    /// The service map.
    pub const fn map(&self) -> &ServiceMap<M> {
        &self.map
    }

    /// Resolved services not reported so far because they did not fit
    /// (see [`Diff::rejected`]).
    pub const fn rejected(&self) -> usize {
        self.rejected
    }

    /// The endpoints currently known for `kind`, if it is browsed.
    pub fn endpoints(&self, kind: ServiceKind<'_>) -> Option<impl Iterator<Item = Service<'_>>> {
        self.kinds
            .iter()
            .find(|b| b.kind == kind.key())
            .map(|b| b.tracker.iter())
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Sends a `qtype` query for `name` with RD and EDNS(0) and waits for
    /// its reply, retrying up to the configured attempts; returns the
    /// reply's length in `self.rx`.
    ///
    /// Each attempt binds a new socket on a fresh ephemeral port, connects
    /// it to the nameserver (so the kernel drops datagrams from any other
    /// address) and uses a query id from the operating system's CSPRNG
    /// ([`query_id`]): an off-path attacker must guess about 32 bits (port
    /// and id) per attempt (RFC 5452 §9.2). Datagrams that are not the
    /// reply ([`check_reply`]) are skipped without ending the attempt; only
    /// the deadline ends it, so a flood cannot turn into an early timeout.
    /// An ICMP "port unreachable" (a refused `recv`) is skipped too, since
    /// it can be spoofed. Cancel safe: the socket of a dropped exchange is
    /// closed with it, so a late reply never reaches a later query.
    async fn exchange(&mut self, name: &str, qtype: u16) -> Result<usize, UnicastError> {
        let text = TextName::new(name)?;
        for _ in 0..self.attempts {
            let id = query_id()?;
            let mut out = [0u8; MAX_QUERY_LEN];
            let len = MessageWriter::new(&mut out, id, FLAG_RD)?
                .question(text, qtype, false)?
                .edns(EDNS_UDP_PAYLOAD)?
                .finish();
            let packet = out.get(..len).ok_or(WireError::BufferTooSmall)?;
            let socket = UdpSocket::bind(SocketAddr::new(self.bind, 0)).await?;
            socket.connect(self.server).await?;
            socket.send(packet).await?;
            let query = Query {
                server: self.server,
                id,
                name,
                qtype,
            };
            let deadline = Instant::now() + self.timeout;
            // Bounded by the deadline: every iteration awaits a datagram
            // or the deadline, and the deadline does not move.
            loop {
                let Ok(received) =
                    tokio::time::timeout_at(deadline, socket.recv(&mut self.rx)).await
                else {
                    break;
                };
                let len = match received {
                    Ok(len) => len,
                    Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => continue,
                    Err(e) => return Err(e.into()),
                };
                let packet = self.rx.get(..len).ok_or(WireError::Truncated)?;
                // Connected: the kernel only delivers the nameserver's
                // datagrams.
                match check_reply(&query, self.server, packet) {
                    Verdict::Accept(_) => return Ok(len),
                    Verdict::Truncated => return Err(UnicastError::Truncated),
                    Verdict::Ignore(_) => {}
                }
            }
        }
        Err(UnicastError::Timeout)
    }

    /// The reply of `len` bytes kept by [`Self::exchange`].
    fn reply(&self, len: usize) -> Result<Message<'_>, UnicastError> {
        Ok(Message::parse(
            self.rx.get(..len).ok_or(WireError::Truncated)?,
        )?)
    }

    /// Fills in the addresses of `found` with A, then AAAA, queries.
    async fn address(&mut self, found: Found) -> Result<Found, UnicastError> {
        let target: String<{ super::answer::MAX_TARGET_LEN }> = found
            .target()
            .try_into()
            .map_err(|_| WireError::InvalidName)?;
        for qtype in [TYPE_A, TYPE_AAAA] {
            let len = self.exchange(&target, qtype).await?;
            match address_answer(&self.reply(len)?, &target)? {
                Lookup::Found(a) => return Ok(found.with_addresses(a)),
                Lookup::NoSuchName => break,
                Lookup::NoData => {}
            }
        }
        Ok(found)
    }

    /// Resolves the SRV name `qname` and every target without glue.
    async fn resolve(&mut self, qname: &str) -> Result<Vec<Found, MAX_ENDPOINTS>, UnicastError> {
        let len = self.exchange(qname, TYPE_SRV).await?;
        let found = match srv_answer::<MAX_ENDPOINTS>(&self.reply(len)?, qname)? {
            Lookup::Found(set) => {
                self.rejected += set.overflow;
                set.found
            }
            Lookup::NoSuchName | Lookup::NoData => Vec::new(),
        };
        let mut resolved = Vec::new();
        // Bounded: at most MAX_ENDPOINTS targets, two queries each.
        for f in found {
            let f = if f.is_addressed() {
                f
            } else {
                self.address(f).await?
            };
            // Cannot fail: as many as `found` holds.
            let _ = resolved.push(f);
        }
        Ok(resolved)
    }

    /// Waits until the browsed kind due soonest is due, resolves it and
    /// queues its events. A failure keeps its endpoints and reschedules.
    async fn round(&mut self) -> Result<(), UnicastError> {
        let Some((i, due_ms)) = self
            .kinds
            .iter()
            .enumerate()
            .map(|(i, b)| (i, b.tracker.due_ms()))
            .min_by_key(|&(_, due)| due)
        else {
            return Err(UnicastError::NotBrowsing);
        };
        tokio::time::sleep_until(self.start + Duration::from_millis(due_ms)).await;
        let Some(b) = self.kinds.get(i) else {
            return Err(UnicastError::NotBrowsing);
        };
        let (kind, qname) = (b.kind, b.qname.clone());
        let result = self.resolve(&qname).await;
        let now_ms = self.now_ms();
        let schedule = self.schedule;
        let Some(b) = self.kinds.get_mut(i) else {
            return Err(UnicastError::NotBrowsing);
        };
        let tracker = core::mem::take(&mut b.tracker);
        match result {
            Ok(found) => {
                let services: Vec<Service<'_>, MAX_ENDPOINTS> = found
                    .iter()
                    .map(|f| f.service(ServiceKind::new(kind.as_key())))
                    .collect();
                let (tracker, diff) = tracker.update(&services, now_ms, &schedule);
                b.tracker = tracker;
                self.rejected += diff.rejected();
                self.pending = diff;
                Ok(())
            }
            Err(e) => {
                b.tracker = tracker.failed(now_ms, &schedule);
                Err(e)
            }
        }
    }

    /// Copies the front pending event into `buf` and drops it from the
    /// queue; kept if it does not fit.
    fn emit<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Event<'b>>, UnicastError> {
        let Some(event) = self.pending.front() else {
            return Ok(None);
        };
        let event = event_into(buf, &event).map_err(UnicastError::Record)?;
        self.pending = core::mem::take(&mut self.pending).advance();
        Ok(Some(event))
    }
}

impl<const M: usize> Browse for UnicastDnsSd<M> {
    type Error = UnicastError;

    /// Browses every mapped kind the filter selects (exact or prefix),
    /// forgetting earlier endpoints, and resolves the first one now; see
    /// the parent module.
    async fn browse(&mut self, filter: KeyFilter<'_>) -> Result<(), UnicastError> {
        let mut kinds = Vec::new();
        for kind in self.map.kinds().filter(|k| filter.matches(k.key())) {
            let mut name = [0u8; MAX_SRV_NAME_LEN];
            let qname = self
                .map
                .srv_name(kind, &mut name)
                .map_err(UnicastError::Map)?;
            let b = Browsed {
                kind: KeyBuf::from_key(kind.key())
                    .map_err(|_| UnicastError::Map(MapError::KindTooLong))?,
                qname: String::try_from(qname)
                    .map_err(|_| UnicastError::Map(MapError::BufferTooSmall))?,
                tracker: Tracker::new(),
            };
            // Cannot fail: the map holds at most M kinds.
            let _ = kinds.push(b);
        }
        if kinds.is_empty() {
            return Err(UnicastError::Unmapped);
        }
        self.kinds = kinds;
        self.pending = Diff::default();
        self.round().await
    }

    /// The next Up or Down; resolves kinds as they come due while none is
    /// queued, at most [`MAX_QUIET_ROUNDS`] times. Cancel safe.
    async fn next<'b>(&mut self, buf: &'b mut [u8]) -> Result<Event<'b>, UnicastError> {
        for _ in 0..MAX_QUIET_ROUNDS {
            if !self.pending.is_empty() {
                break;
            }
            self.round().await?;
        }
        self.emit(buf)?.ok_or(UnicastError::QuietLimit)
    }

    /// An event of a resolution already made, without querying.
    fn try_next<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Event<'b>>, UnicastError> {
        self.emit(buf)
    }
}
