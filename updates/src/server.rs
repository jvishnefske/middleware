//! `mw-updates`: the [`Store`] over Zenoh (UP5).
//!
//! * A queryable on `latest/**` answers [`wire::encode_latest`] of the
//!   stored releases (empty when none).
//! * A queryable on `img/**` answers one chunk, or an error reply when the
//!   image is not stored.
//! * A subscriber on `publish/**` checks and stores each publish
//!   ([`Store::publish`]) and then puts the new `latest`, so updaters
//!   subscribed to it look at once. A refused publish is logged and
//!   changes nothing; the publisher sees its release missing from
//!   `latest`.
//! * A subscriber on `status/**` logs each updater report.

use std::sync::Arc;

use crate::keys::{ROOT, UpdateKey};
use crate::store::Store;
use crate::wire::{decode_publish, encode_latest};

/// Why serving stopped.
pub type ServeError = Box<dyn std::error::Error + Send + Sync>;

/// One line on stderr (the binary's log; the cluster collects it).
fn log(line: &str) {
    eprintln!("mw-updates: {line}");
}

/// Serves `store` on `session` until the session closes.
pub async fn serve(session: zenoh::Session, store: Arc<Store>) -> Result<(), ServeError> {
    let latest = session
        .declare_queryable(format!("{ROOT}/latest/**"))
        .await?;
    let img = session.declare_queryable(format!("{ROOT}/img/**")).await?;
    let publish = session
        .declare_subscriber(format!("{ROOT}/publish/**"))
        .await?;
    let status = session
        .declare_subscriber(format!("{ROOT}/status/**"))
        .await?;
    log("serving");
    loop {
        tokio::select! {
            q = latest.recv_async() => {
                let q = q?;
                match UpdateKey::parse(q.key_expr().as_str()) {
                    Some(UpdateKey::Latest { channel, target }) => {
                        let body = encode_latest(&store.latest(&channel, &target));
                        q.reply(q.key_expr().clone(), body).await?;
                    }
                    _ => q.reply_err("not a latest key").await?,
                }
            }
            q = img.recv_async() => {
                let q = q?;
                let chunk = match UpdateKey::parse(q.key_expr().as_str()) {
                    Some(UpdateKey::Img { sha256, offset }) => store.chunk(&sha256, offset),
                    _ => None,
                };
                match chunk {
                    Some(c) => q.reply(q.key_expr().clone(), c).await?,
                    None => q.reply_err("no such image or offset").await?,
                }
            }
            s = publish.recv_async() => {
                let s = s?;
                let key = s.key_expr().as_str().to_owned();
                let Some(UpdateKey::Publish { channel, target }) = UpdateKey::parse(&key) else {
                    log(&format!("refused a publish on {key}: bad key"));
                    continue;
                };
                let bytes = s.payload().to_bytes();
                let Some((release, image)) = decode_publish(&bytes) else {
                    log(&format!("refused a publish on {key}: malformed"));
                    continue;
                };
                let (kind, version) = (release.kind(), release.version());
                match store.publish(&channel, &target, &release, image) {
                    Ok(()) => {
                        log(&format!(
                            "stored {} {} for {channel}/{target} ({} bytes)",
                            kind.name(), version.0, image.len()
                        ));
                        let key = UpdateKey::Latest { channel: channel.clone(), target: target.clone() };
                        session
                            .put(key.render(), encode_latest(&store.latest(&channel, &target)))
                            .await?;
                    }
                    Err(e) => log(&format!(
                        "refused {} {} for {channel}/{target}: {e}", kind.name(), version.0
                    )),
                }
            }
            s = status.recv_async() => {
                let s = s?;
                log(&format!(
                    "{}: {}",
                    s.key_expr().as_str(),
                    String::from_utf8_lossy(&s.payload().to_bytes())
                ));
            }
        }
    }
}
