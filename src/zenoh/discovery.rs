//! [`ZenohDiscovery`]: service discovery over Zenoh liveliness.
//!
//! # Announcing
//!
//! [`Announce::announce`] declares, on the key expression
//! `@mw/svc/<kind>/<instance>` ([`wire::service_key`]):
//!
//! 1. a queryable answering every query with the service record, encoded
//!    as a version 1 discovery beacon
//!    ([`beacon::encode`]: endpoint, node, TXT, TTL); the record's node is
//!    the announced one, or this session's sender id when the service names
//!    none;
//! 2. then a liveliness token, so a browser that sees the token can
//!    already reach the queryable (declarations travel in order on a link).
//!
//! Announcing the same kind and instance again replaces both (the old token
//! goes first, so browsers see `Down` then `Up` with the new record).
//! [`Announce::withdraw`] undeclares the token, then the queryable;
//! withdrawing a service that is not announced does nothing. Zenoh keeps a
//! token alive as long as the session and its link are: no re-announcing
//! is needed, and a session that closes or loses its link withdraws its
//! tokens by itself. The record's TTL is passed through for the browser's
//! [`Directory`](crate::discovery::Directory) but Zenoh does not need it.
//!
//! The instance must be one plain Zenoh key chunk
//! ([`wire::is_valid_instance`]), otherwise [`ZenohError::InvalidInstance`].
//!
//! # Browsing
//!
//! [`Browse::browse`] declares a liveliness subscriber with history, so
//! tokens alive before it are reported too, on
//! `@mw/svc/<kind>/*` for [`KeyFilter::Exact`] (the instances of exactly
//! that kind) or `@mw/svc/<kind>/**` for [`KeyFilter::Prefix`] (that kind
//! and every kind below it); a later call replaces it. Then
//! [`Browse::next`]:
//!
//! * a token appearing (Zenoh `Put`) starts a fetch of the record from the
//!   announcer's queryable (a `get` bounded by the discovery timeout); its
//!   first decodable reply is [`Event::Up`] (or [`Event::Down`] for a TTL 0
//!   record). A fetch that ends without a record is
//!   [`ZenohError::NoReply`]; an undecodable record is
//!   [`ZenohError::Record`]. Either way the next call goes on with the next
//!   token change.
//! * a token disappearing (Zenoh `Delete`: withdrawn, session closed or
//!   link lost) is [`Event::Down`] at once.
//!
//! The fetch in progress is kept in the handle, so a cancelled `next`
//! resumes it, and [`Browse::try_next`] starts it without waiting and
//! reports the event once the record has arrived.

use std::collections::BTreeMap;
use std::time::Duration;

use ::zenoh::handlers::FifoChannelHandler;
use ::zenoh::liveliness::LivelinessToken;
use ::zenoh::query::{ConsolidationMode, QueryTarget, Queryable, Reply};
use ::zenoh::sample::{Sample, SampleKind};
use ::zenoh::{Session, Wait};

use crate::discovery::beacon::{self, MAX_BEACON_LEN};
use crate::discovery::record::event_into;
use crate::discovery::{Announce, Browse, Event, RecordError, Service, ServiceKind};
use crate::key::KeyFilter;

use super::{DEFAULT_TIMEOUT, ZenohError, sender_id_of, wire};

/// The declarations that keep one service announced.
#[derive(Debug)]
struct Announced {
    token: LivelinessToken,
    record: Queryable<()>,
}

/// A record fetch started for a token that appeared.
#[derive(Debug)]
struct Fetch {
    key: String,
    replies: FifoChannelHandler<Reply>,
}

/// Announces and browses services on one Zenoh session (see the module
/// docs).
#[derive(Debug)]
pub struct ZenohDiscovery {
    session: Session,
    timeout: Duration,
    announced: BTreeMap<String, Announced>,
    browser: Option<::zenoh::pubsub::Subscriber<FifoChannelHandler<Sample>>>,
    pending: Option<Fetch>,
}

impl ZenohDiscovery {
    /// A handle on `session` fetching records within [`DEFAULT_TIMEOUT`].
    pub fn new(session: Session) -> Self {
        Self {
            session,
            timeout: DEFAULT_TIMEOUT,
            announced: BTreeMap::new(),
            browser: None,
            pending: None,
        }
    }

    /// The same handle fetching records within `timeout`.
    pub fn with_timeout(self, timeout: Duration) -> Self {
        Self { timeout, ..self }
    }

    /// The services announced and not withdrawn, as their key expressions.
    pub fn announced(&self) -> impl Iterator<Item = &str> {
        self.announced.keys().map(String::as_str)
    }

    fn start_fetch(&self, key: &str) -> Result<Fetch, ZenohError> {
        let replies = self
            .session
            .get(key)
            .target(QueryTarget::All)
            .consolidation(ConsolidationMode::None)
            .timeout(self.timeout)
            .wait()?;
        Ok(Fetch {
            key: key.to_owned(),
            replies,
        })
    }

    /// Handles a token change: `Some(fetch)` to start for a `Put`, or the
    /// key of a `Delete`.
    fn on_token(&self, sample: &Sample) -> Result<TokenChange, ZenohError> {
        let key = sample.key_expr().as_str();
        match sample.kind() {
            SampleKind::Put => self.start_fetch(key).map(TokenChange::Fetch),
            SampleKind::Delete => Ok(TokenChange::Gone(key.to_owned())),
        }
    }
}

enum TokenChange {
    Fetch(Fetch),
    Gone(String),
}

/// A failure to copy an event into the caller's buffer, as a record error.
fn record_error(e: RecordError) -> ZenohError {
    ZenohError::Record(match e {
        RecordError::BufferTooSmall => beacon::BeaconError::BufferTooSmall,
        _ => beacon::BeaconError::Malformed,
    })
}

/// [`Event::Down`] for the service whose token `key` went away, in `buf`.
fn down_event<'b>(key: &str, buf: &'b mut [u8]) -> Result<Event<'b>, ZenohError> {
    let (kind, instance) =
        wire::parse_service_key(key).ok_or(ZenohError::Record(beacon::BeaconError::NotABeacon))?;
    event_into(buf, &Event::Down { kind, instance }).map_err(record_error)
}

/// The event of the record `payload` fetched from `key`, in `buf`.
fn up_event<'b>(key: &str, payload: &[u8], buf: &'b mut [u8]) -> Result<Event<'b>, ZenohError> {
    let (kind, instance) =
        wire::parse_service_key(key).ok_or(ZenohError::Record(beacon::BeaconError::NotABeacon))?;
    let service = beacon::decode(kind, payload).map_err(ZenohError::Record)?;
    if service.instance != instance {
        return Err(ZenohError::Record(beacon::BeaconError::Malformed));
    }
    event_into(buf, &Event::from_service(service)).map_err(record_error)
}

/// The payload of a successful record reply.
fn record_payload(reply: &Reply) -> Option<Vec<u8>> {
    reply
        .result()
        .ok()
        .map(|s| s.payload().to_bytes().into_owned())
}

impl Announce for ZenohDiscovery {
    type Error = ZenohError;

    /// Declares the record queryable, then the liveliness token (see the
    /// module docs). Replaces an earlier announcement of the same service.
    async fn announce(&mut self, service: &Service<'_>) -> Result<(), ZenohError> {
        let key = wire::service_key(service.kind, service.instance)
            .map_err(|_| ZenohError::InvalidInstance)?;
        let record = Service {
            node: service
                .node
                .or_else(|| Some(sender_id_of(self.session.zid()))),
            ..*service
        };
        let mut out = [0u8; MAX_BEACON_LEN];
        let n = beacon::encode(&record, &mut out).map_err(ZenohError::Record)?;
        let payload = out.get(..n).unwrap_or_default().to_vec();
        if let Some(old) = self.announced.remove(&key) {
            old.token.undeclare().await?;
            old.record.undeclare().await?;
        }
        let queryable = self
            .session
            .declare_queryable(key.as_str())
            .callback(move |q| {
                // A failed reply only means this querier misses the record;
                // its fetch then ends without one.
                let _ = q.reply(q.key_expr().clone(), payload.clone()).wait();
            })
            .await?;
        let token = self
            .session
            .liveliness()
            .declare_token(key.as_str())
            .await?;
        self.announced.insert(
            key,
            Announced {
                token,
                record: queryable,
            },
        );
        Ok(())
    }

    /// Undeclares the token (browsers see `Down`), then the queryable.
    async fn withdraw(&mut self, kind: ServiceKind<'_>, instance: &str) -> Result<(), ZenohError> {
        let key = wire::service_key(kind, instance).map_err(|_| ZenohError::InvalidInstance)?;
        if let Some(old) = self.announced.remove(&key) {
            old.token.undeclare().await?;
            old.record.undeclare().await?;
        }
        Ok(())
    }
}

impl Browse for ZenohDiscovery {
    type Error = ZenohError;

    /// Declares the liveliness subscriber with history for `filter`,
    /// replacing an earlier one and abandoning its fetch in progress.
    async fn browse(&mut self, filter: KeyFilter<'_>) -> Result<(), ZenohError> {
        let expr = match filter {
            KeyFilter::Exact(k) => format!("{}{k}/*", wire::SERVICE_PREFIX),
            KeyFilter::Prefix(k) => format!("{}{k}/**", wire::SERVICE_PREFIX),
        };
        self.browser = None;
        self.pending = None;
        let subscriber = self
            .session
            .liveliness()
            .declare_subscriber(expr)
            .history(true)
            .await?;
        self.browser = Some(subscriber);
        Ok(())
    }

    /// Waits for the next token change and reports it (see the module
    /// docs). Cancel safe: the token change and any fetch in progress stay
    /// in the handle.
    async fn next<'b>(&mut self, buf: &'b mut [u8]) -> Result<Event<'b>, ZenohError> {
        if self.pending.is_none() {
            let browser = self.browser.as_ref().ok_or(ZenohError::NotBrowsing)?;
            let sample = browser.recv_async().await.map_err(|_| ZenohError::Closed)?;
            match self.on_token(&sample)? {
                TokenChange::Fetch(f) => self.pending = Some(f),
                TokenChange::Gone(key) => return down_event(&key, buf),
            }
        }
        let Some(fetch) = self.pending.as_ref() else {
            return Err(ZenohError::NoReply);
        };
        // Bounded by the fetch timeout, after which Zenoh closes the
        // channel; error replies are skipped.
        let payload = loop {
            match fetch.replies.recv_async().await {
                Ok(reply) => {
                    if let Some(p) = record_payload(&reply) {
                        break Some(p);
                    }
                }
                Err(_) => break None,
            }
        };
        let key = self.pending.take().map(|f| f.key).unwrap_or_default();
        match payload {
            Some(p) => up_event(&key, &p, buf),
            None => Err(ZenohError::NoReply),
        }
    }

    /// Reports the next token change if it can without waiting: a `Delete`
    /// at once; a `Put` once its record has arrived (the fetch is started
    /// and kept; `Ok(None)` until then).
    fn try_next<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Event<'b>>, ZenohError> {
        if self.pending.is_none() {
            let browser = self.browser.as_ref().ok_or(ZenohError::NotBrowsing)?;
            let sample = match browser.try_recv() {
                Ok(Some(s)) => s,
                Ok(None) => return Ok(None),
                Err(_) => return Err(ZenohError::Closed),
            };
            match self.on_token(&sample)? {
                TokenChange::Fetch(f) => self.pending = Some(f),
                TokenChange::Gone(key) => return down_event(&key, buf).map(Some),
            }
        }
        let Some(fetch) = self.pending.as_ref() else {
            return Ok(None);
        };
        // Bounded: each iteration dequeues one already arrived reply.
        let payload = loop {
            match fetch.replies.try_recv() {
                Ok(Some(reply)) => {
                    if let Some(p) = record_payload(&reply) {
                        break Some(p);
                    }
                }
                Ok(None) => return Ok(None),
                Err(_) => break None,
            }
        };
        let key = self.pending.take().map(|f| f.key).unwrap_or_default();
        match payload {
            Some(p) => up_event(&key, &p, buf).map(Some),
            None => Err(ZenohError::NoReply),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::Endpoint;
    use crate::transport::SenderId;

    const KIND: ServiceKind<'static> = ServiceKind::from_static("cam/jpeg");

    #[test]
    fn records_and_goodbyes_decode_from_their_keys() {
        let s = Service {
            kind: KIND,
            instance: "unit-1",
            node: Some(SenderId([7; 8])),
            endpoint: Endpoint::Locator("tcp/127.0.0.1:7447"),
            txt: b"\x05ver=5",
            ttl_s: 60,
        };
        let mut out = [0u8; MAX_BEACON_LEN];
        let n = beacon::encode(&s, &mut out).expect("encode");
        let key = wire::service_key(KIND, "unit-1").expect("key");
        let mut buf = [0u8; 256];
        assert_eq!(up_event(&key, &out[..n], &mut buf).ok(), Some(Event::Up(s)));
        let mut buf = [0u8; 256];
        assert_eq!(
            down_event(&key, &mut buf).ok(),
            Some(Event::Down {
                kind: KIND,
                instance: "unit-1"
            })
        );
        // A record for another instance than its key is refused.
        let other = wire::service_key(KIND, "unit-2").expect("key");
        assert!(up_event(&other, &out[..n], &mut buf).is_err());
        // A TTL 0 record is a goodbye.
        let n = beacon::encode(&Service { ttl_s: 0, ..s }, &mut out).expect("encode");
        assert!(matches!(
            up_event(&key, &out[..n], &mut buf),
            Ok(Event::Down { .. })
        ));
        assert!(down_event("app/x", &mut buf).is_err());
        let mut tiny = [0u8; 4];
        assert!(matches!(
            down_event(&key, &mut tiny),
            Err(ZenohError::Record(beacon::BeaconError::BufferTooSmall))
        ));
    }
}
