//! Unicast DNS-SD (feature `dns-sd`): service discovery through an ordinary
//! DNS resolver (RFC 6763 over RFC 1035), for networks where multicast does
//! not reach, such as a Kubernetes cluster, where the cluster DNS (CoreDNS)
//! publishes an SRV record per named Service port.
//!
//! [`UnicastDnsSd`] implements [`Browse`](crate::discovery::Browse); it
//! does not announce (Kubernetes publishes the records from the Service and
//! its endpoints).
//!
//! # Parts
//!
//! A functional core, pure and heap free, and a thin tokio shell:
//!
//! * [`resolv`] — [`ResolvConf::parse`]: `nameserver`, `search`, `domain`
//!   and `options ndots/timeout/attempts` from `resolv.conf` text, and the
//!   pod's namespace and cluster domain derived from the search list;
//! * [`map`] — [`ServiceMap`]: which Kubernetes Service, namespace and port
//!   name serve each [`ServiceKind`](crate::discovery::ServiceKind), and the
//!   SRV name `_<port>._tcp.<service>.<namespace>.svc.<cluster-domain>`;
//! * [`answer`] — matching a datagram to the outstanding query
//!   ([`check_reply`]), and the endpoints of an SRV reply ([`srv_answer`],
//!   with A/AAAA glue) or the addresses of an A/AAAA reply
//!   ([`address_answer`]);
//! * [`tracker`] — [`Tracker`]: the endpoints of one kind as last shown, the
//!   Up/Down [`Diff`] a new answer makes, and when to resolve again
//!   ([`Schedule`]: 80 % of the smallest TTL, clamped);
//! * [`socket`] — [`UnicastDnsSd`], the UDP shell.
//!
//! # Queries and replies
//!
//! Every query goes to one nameserver: the one given to
//! [`UnicastDnsSdBuilder::nameserver`], else the first of the resolver
//! configuration (given, or read from [`RESOLV_CONF_PATH`] at build). It
//! asks one question with recursion desired, carries an EDNS(0) OPT record
//! advertising [`EDNS_UDP_PAYLOAD`] bytes, and a fresh id from a xorshift
//! generator seeded per process from the std hasher's random keys. A
//! reply is accepted only from the nameserver's address, with the query's
//! id and question ([`check_reply`]); anything else is skipped, at most
//! [`MAX_STRAY_DATAGRAMS`] per attempt. Each attempt waits at most the
//! timeout; after the last attempt the query fails with
//! [`UnicastError::Timeout`]. A reply with the TC bit fails with
//! [`UnicastError::Truncated`]: there is no TCP fallback, so an answer must
//! fit [`EDNS_UDP_PAYLOAD`] bytes (and [`MAX_RECORDS`] records, about 30
//! pods with address glue).
//!
//! # Resolving a kind
//!
//! 1. Query SRV for the kind's name in the [`ServiceMap`]. NXDOMAIN or no
//!    SRV is an empty set (a headless Service without ready pods); another
//!    error code fails the resolution ([`UnicastError::Answer`]).
//! 2. Each SRV is one endpoint (one per pod of a headless Service), named by
//!    its target's first label. An endpoint without A/AAAA glue in the reply
//!    is resolved with an A query, then AAAA if A has no data.
//! 3. An endpoint with an address is [`Endpoint::Socket`] (IPv4 preferred),
//!    else [`Endpoint::Host`] with the SRV target; no node id; TXT only if
//!    the server adds a TXT record for the target.
//! 4. The [`Tracker`] turns the set into Up/Down events against what was
//!    shown before and schedules the next resolution.
//!
//! A resolution that fails (timeout, truncation, error code) changes no
//! endpoint and is retried after the [`Schedule`]'s error interval; the
//! error is returned from the call that ran it.
//!
//! # Browse
//!
//! * [`Browse::browse`] selects every kind of the map the filter matches,
//!   exact or prefix ([`UnicastError::Unmapped`] if none), forgets the
//!   endpoints of an earlier browse, and resolves the first kind before
//!   returning (so its query is sent now, as the trait asks; a failure is
//!   returned but the browse stands and is retried); other kinds are due at
//!   once.
//! * [`Browse::next`] returns the next queued event; while none is queued it
//!   sleeps until the kind due soonest is due and resolves it, at most
//!   [`MAX_QUIET_ROUNDS`] times ([`UnicastError::QuietLimit`]). Events of
//!   one resolution are delivered before the next resolution runs. An event
//!   too large for the buffer is [`UnicastError::Record`] and kept.
//! * [`Browse::try_next`] returns a queued event; it never queries.
//!
//! Cancel safety: a resolution's events are queued in one step after all
//! of its queries completed, and an event leaves the queue only once copied,
//! so a dropped `next` loses none; a dropped resolution is re-run at its
//! due time and a late reply to it is skipped by id.
//!
//! # Composing
//!
//! [`UnicastDnsSd`] is an ordinary [`Browse`](crate::discovery::Browse)
//! that owns its state and spawns no task, so anything generic over
//! `B: Browse` can wrap it, and its events can feed a
//! [`Directory`](crate::discovery::Directory): the tracker re-reports each
//! unchanged endpoint once 80 % of the TTL it was last reported with has
//! passed, so the directory keeps live endpoints and expires those the
//! cluster DNS stops answering for.
//!
//! [`Browse::browse`]: crate::discovery::Browse::browse
//! [`Browse::next`]: crate::discovery::Browse::next
//! [`Browse::try_next`]: crate::discovery::Browse::try_next
//! [`Endpoint::Socket`]: crate::discovery::Endpoint::Socket
//! [`Endpoint::Host`]: crate::discovery::Endpoint::Host
//! [`MAX_RECORDS`]: super::wire::MAX_RECORDS

pub mod answer;
pub mod map;
pub mod resolv;
pub mod socket;
pub mod tracker;

pub use answer::{
    Addresses, AnswerError, Found, Ignored, Lookup, Query, SrvSet, Verdict, address_answer,
    check_reply, srv_answer,
};
pub use map::{DEFAULT_CLUSTER_DOMAIN, MapError, ServiceMap, ServiceRef};
pub use resolv::ResolvConf;
pub use socket::{
    DEFAULT_ATTEMPTS, DEFAULT_TIMEOUT, EDNS_UDP_PAYLOAD, MAX_ENDPOINTS, MAX_QUIET_ROUNDS,
    MAX_STRAY_DATAGRAMS, RESOLV_CONF_PATH, UnicastDnsSd, UnicastDnsSdBuilder, UnicastError,
};
pub use tracker::{Diff, Schedule, Tracker};
