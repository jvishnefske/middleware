//! DNS-SD announcement of the node (RFC 6763), through the middleware's
//! [`Announce`] trait: [`MdnsAnnouncer`] (multicast DNS, RFC 6762) puts the
//! node on the local link, so a browser finds it by its service type.
//!
//! The service kind is [`KIND`] (`mw/node`, the DNS-SD type `_mw-node._tcp`),
//! the instance is the node id in hex, and the endpoint is the node's Zenoh
//! listener at `<host>.local:<port>`. The announcer is driven by its own
//! task; on shutdown the task sends the goodbye (TTL 0) and ends.

use middleware::Key;
use middleware::discovery::{Announce, Endpoint, Service, ServiceKind};
use middleware::mdns::announce::MdnsAnnouncer;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// The service kind this node announces.
pub const KIND: &str = "mw/node";
/// Seconds an announcement stays valid (RFC 6762 recommends 120 for SRV).
const TTL_S: u32 = 120;

/// A running announcement: stop it with [`Announcement::stop`], which waits
/// for the goodbye.
pub struct Announcement {
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl Announcement {
    /// Sends the goodbye and waits for the announcer task to end.
    pub async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let _ = self.task.await;
    }
}

/// Starts announcing the node `instance` at `host.local:port` on the local
/// link. Fails if the multicast socket cannot be opened or the first
/// announcement cannot be sent; the node keeps running without DNS-SD then,
/// and the caller reports why.
pub async fn start(host: String, instance: String, port: u16) -> Result<Announcement, String> {
    // Validates the kind once, before anything is sent.
    let _ = kind()?;
    let mut announcer = MdnsAnnouncer::builder()
        .host(&format!("{host}.local"))
        .build()
        .await
        .map_err(|e| format!("mdns socket: {e:?}"))?;
    {
        let kind = kind()?;
        let service = service(kind, &instance, &host, port);
        announcer
            .announce(&service)
            .await
            .map_err(|e| format!("dns-sd announce: {e:?}"))?;
    }
    let (stop, mut stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                r = announcer.step() => {
                    if let Err(e) = r {
                        eprintln!("mw-posix-node: dns-sd: {e:?}; no longer announcing");
                        return;
                    }
                }
                _ = &mut stopped => {
                    let kind = match kind() {
                        Ok(k) => k,
                        Err(_) => return,
                    };
                    if let Err(e) = announcer.withdraw(kind, &instance).await {
                        eprintln!("mw-posix-node: dns-sd goodbye: {e:?}");
                    }
                    return;
                }
            }
        }
    });
    Ok(Announcement { stop: Some(stop), task })
}

fn kind() -> Result<ServiceKind<'static>, String> {
    Key::new(KIND)
        .map(ServiceKind::new)
        .map_err(|e| format!("dns-sd kind {KIND:?}: {e:?}"))
}

fn service<'a>(kind: ServiceKind<'a>, instance: &'a str, host: &'a str, port: u16) -> Service<'a> {
    Service {
        kind,
        instance,
        node: None,
        endpoint: Endpoint::Host { name: host, port },
        txt: &[],
        ttl_s: TTL_S,
    }
}
