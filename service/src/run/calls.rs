//! Downstream discovery: one background task per called kind keeping a
//! [`Directory`] of that kind's ready endpoints, and the per-task
//! [`Links`] the serve tasks lend to handlers.
//!
//! # Per called kind
//!
//! `UnicastDnsSd` (the `MW_SERVICE_MAP` route of the kind; nameserver from
//! [`RunOptions::nameserver`](super::RunOptions::nameserver) or
//! `/etc/resolv.conf`) → `MeshBrowse` (an endpoint is up only while its
//! health service says `SERVING` for the kind, and carries the node id of
//! its `mw-node` header) → a [`Directory`] of [`DIRECTORY_SLOTS`]. The
//! task ([`watch_kind`]) owns all three and publishes every change of the
//! directory as a new snapshot on a `tokio::sync::watch` channel.
//!
//! The directory mirrors `MeshBrowse`'s Up/Down events and is not expired
//! by TTL: `MeshBrowse` is the authority on readiness (it reports `Down`
//! when DNS stops listing a pod or its health watch reports anything but
//! `SERVING` or ends) and it does not repeat `Up` for an unchanged
//! endpoint, so a TTL expiry here would drop live endpoints. Freshness
//! comes from below: `UnicastDnsSd` re-resolves each kind at 80 % of the
//! smallest TTL it saw (its `Schedule`), which is when the cluster DNS's
//! answer can change. A browse error (no reply from the nameserver, too
//! many quiet rounds) is logged and retried after [`ERROR_BACKOFF`];
//! endpoints already known stay.
//!
//! # Ownership
//!
//! Each serve task owns its own [`Links`] ([`links_for_task`]): one
//! [`DownstreamClient`] per called kind (own channels, the run's node id
//! as client id) and a clone of the Zenoh transport (which shares the
//! session). Before each request it applies the newest directory snapshot
//! of each kind ([`refresh`]). Snapshots are values moved through a
//! channel; nothing is shared and locked.

use std::time::Duration;

use middleware::discovery::{Browse, Directory, ServiceKind};
use middleware::dns::unicast::UnicastDnsSd;
use middleware::grpc::mesh::{MeshBrowse, MeshClient, MeshDialer};
use middleware::key::{Key, KeyFilter};
use middleware::transport::SenderId;
use middleware::zenoh::ZenohTransport;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::options::RunOptions;
use crate::config::{Config, SERVICE_MAP_CAPACITY};
use crate::ctx::{DIRECTORY_SLOTS, DownstreamClient, Links};
use crate::error::ServiceError;

/// Pause after a failed browse before trying again.
pub const ERROR_BACKOFF: Duration = Duration::from_secs(1);

/// A [`Browse`] kept on the heap. `UnicastDnsSd` is large (its receive
/// buffer and per-kind trackers, about 150 KiB), so the runner boxes it
/// once when built rather than moving it through task futures and stack
/// frames.
#[derive(Debug)]
pub struct Boxed<B>(pub Box<B>);

impl<B: Browse> Browse for Boxed<B> {
    type Error = B::Error;

    async fn browse(&mut self, filter: KeyFilter<'_>) -> Result<(), B::Error> {
        self.0.browse(filter).await
    }

    async fn next<'b>(
        &mut self,
        buf: &'b mut [u8],
    ) -> Result<middleware::discovery::Event<'b>, B::Error> {
        self.0.next(buf).await
    }

    fn try_next<'b>(
        &mut self,
        buf: &'b mut [u8],
    ) -> Result<Option<middleware::discovery::Event<'b>>, B::Error> {
        self.0.try_next(buf)
    }
}

/// The snapshot type each called kind publishes.
pub type Snapshot = Directory<DIRECTORY_SLOTS>;

/// One called kind being discovered.
#[derive(Debug)]
pub struct Called {
    /// The kind.
    pub kind: String,
    /// Its directory snapshots.
    pub directory: watch::Receiver<Snapshot>,
    /// The task keeping them fresh (aborted at exit).
    pub task: JoinHandle<()>,
}

/// Starts discovery of every kind `calls` lists (module docs). Fails if a
/// browser cannot be built (no nameserver, unreadable `resolv.conf`).
pub async fn start(
    calls: &[String],
    config: &Config,
    options: &RunOptions,
) -> Result<Vec<Called>, ServiceError> {
    let mut out = Vec::with_capacity(calls.len());
    for kind in calls {
        if Key::new(kind).is_err() {
            return Err(ServiceError::Unmapped(kind.clone()));
        }
        let mut builder = UnicastDnsSd::<SERVICE_MAP_CAPACITY>::builder(config.service_map.clone());
        if let Some(ns) = options.nameserver {
            builder = builder.nameserver(ns);
        }
        if let Some(s) = options.dns_schedule {
            builder = builder.schedule(s);
        }
        let dns = Boxed(Box::new(Box::pin(builder.build()).await?));
        let browse = MeshBrowse::<_, DIRECTORY_SLOTS>::new(dns, options.dialer_or_default());
        let (tx, rx) = watch::channel(Snapshot::new());
        let owned = kind.clone();
        let task = tokio::spawn(async move {
            let key = match Key::new(&owned) {
                Ok(k) => k,
                Err(_) => return,
            };
            watch_kind(browse, key, tx).await;
        });
        out.push(Called {
            kind: kind.clone(),
            directory: rx,
            task,
        });
    }
    Ok(out)
}

/// Browses `key` with `browse` and publishes each change of the directory
/// on `tx` until every receiver is gone (module docs).
pub async fn watch_kind<B: Browse>(mut browse: B, key: Key<'_>, tx: watch::Sender<Snapshot>)
where
    B::Error: std::fmt::Debug,
{
    if let Err(e) = browse.browse(KeyFilter::Exact(key)).await {
        tracing::warn!(kind = key.as_str(), error = ?e, "first resolution failed; retrying");
    }
    let mut directory = Snapshot::new();
    let mut buf = vec![0u8; middleware::grpc::mesh::SCRATCH_LEN];
    let mut quiet = 0u32;
    // Each iteration takes one event or one error; it ends with the run.
    while !tx.is_closed() {
        match browse.next(&mut buf).await {
            Ok(event) => {
                quiet = 0;
                let (next, change) = directory.observe(&event, 0);
                directory = next;
                tracing::info!(
                    kind = key.as_str(),
                    instance = event.instance(),
                    up = matches!(event, middleware::discovery::Event::Up(_)),
                    change = ?change,
                    endpoints = directory.len(),
                    "endpoint"
                );
                tx.send_replace(directory.clone());
            }
            Err(e) => {
                // Logged once per streak, not on every retry.
                if quiet == 0 {
                    tracing::warn!(kind = key.as_str(), error = ?e, "discovery error; retrying");
                }
                quiet = quiet.saturating_add(1);
                tokio::time::sleep(ERROR_BACKOFF).await;
            }
        }
    }
}

/// The [`Links`] of one serve task: a client per called kind (identified
/// as `node`, dialling with `dialer`) and, when the run has a Zenoh
/// session, a clone of it with the declared publish filters.
pub fn links_for_task<'f>(
    called: &[Called],
    node: SenderId,
    dialer: &MeshDialer,
    zenoh: Option<&ZenohTransport>,
    publishes: impl IntoIterator<Item = KeyFilter<'f>>,
) -> Links {
    let links = called
        .iter()
        .fold(Links::new(), |links, c| match Key::new(&c.kind) {
            Ok(key) => {
                let kind = ServiceKind::new(key);
                let client: DownstreamClient = MeshClient::new(kind, dialer.clone())
                    .with_client_id(node)
                    .with_directory(c.directory.borrow().clone());
                links.with_client(kind, client)
            }
            Err(_) => links,
        });
    match zenoh {
        Some(z) => links.with_zenoh(z.clone(), publishes),
        None => links,
    }
}

/// `links` with the newest snapshot of every kind whose directory changed
/// since this receiver last looked.
pub fn refresh(links: Links, directories: &mut [(String, watch::Receiver<Snapshot>)]) -> Links {
    directories.iter_mut().fold(links, |links, (kind, rx)| {
        if rx.has_changed().unwrap_or(false) {
            let snapshot = rx.borrow_and_update().clone();
            links.with_directory(kind, snapshot)
        } else {
            links
        }
    })
}
