//! The updates channel (design.md "Updates channel", UP4–UP8).
//!
//! Releases are signed once, by the root key (the private half never
//! leaves its HSM), as [`SoftwareCert`](middleware::loader::release)s.
//! Devices are provisioned once with the root's public key (the existing
//! `sys/trust/claim`) and accept a release on their own: the cert names
//! the chip, not the device. So a server can hand the same bytes to every
//! device without holding a key, and a device that fetched them can tell a
//! good release from a forged one.
//!
//! # Key space (Zenoh)
//!
//! | key | who | what |
//! |-----|-----|------|
//! | `@mw/updates/latest/<channel>/<target>` | server answers, updaters query and subscribe, the server puts after a publish | the newest release of each kind ([`wire::encode_latest`]) |
//! | `@mw/updates/img/<sha256 hex>/<offset>` | server answers, updaters query | up to [`wire::CHUNK`] image bytes at `offset` |
//! | `@mw/updates/publish/<channel>/<target>` | publishers put, the server subscribes | `[cert][sig]` and the image ([`wire::encode_publish`]) |
//! | `@mw/updates/status/<node>` | updaters put, the server subscribes | one JSON report per device and kind ([`updater::Report`]) |
//!
//! Each prefix is one ACL grant ([`router`]): an updater cannot publish,
//! a publisher cannot answer, and nobody but the server declares a
//! queryable.
//!
//! # Parts
//!
//! * [`keys`], [`wire`] — the key space and the payloads (pure).
//! * [`store`] — the server's releases on disk; every publish is checked
//!   against the root public key, the image hash and the version floor.
//! * [`router`] — the updates zenohd's config: `tcp/` and `ws/` listeners,
//!   user/password authentication, the ACL.
//! * [`server`] — `mw-updates`: the store over Zenoh.
//! * [`client`] — a session to the channel: over `wss://` through
//!   [`middleware::zenoh::wsbridge`] (Traefik ends TLS), or a direct
//!   locator.
//! * [`updater`] — `mw-updater`: what to install on which device (pure)
//!   and the installs over `mwctl::device::Device`; [`run`] — one pass
//!   over every device.
//! * [`config`] — the binaries' environment.
#![forbid(unsafe_code)]

pub mod client;
pub mod config;
pub mod keys;
pub mod publish;
pub mod router;
pub mod run;
pub mod server;
pub mod store;
pub mod updater;
pub mod wire;

/// Hex of `bytes`, lower case.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Bytes of the hex string `s`; `None` for odd length or a non-hex digit.
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// The root public key from its hex form (32 bytes Ed25519, 33 bytes
/// compressed P-256), as `mwctl key pub` prints it.
pub fn parse_root_key(s: &str) -> Option<middleware::loader::trust::PublicKey> {
    let key = middleware::loader::trust::PublicKey::from_bytes(&unhex(s)?)?;
    middleware::loader::trust::is_valid_key(&key).then_some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        assert_eq!(hex(&[0, 0xab, 0x10]), "00ab10");
        assert_eq!(unhex("00ab10"), Some(vec![0, 0xab, 0x10]));
        assert_eq!(unhex("0"), None);
        assert_eq!(unhex("zz"), None);
        let ed = ed25519_dalek::SigningKey::from_bytes(&[3; 32])
            .verifying_key()
            .to_bytes();
        assert!(parse_root_key(&hex(&ed)).is_some());
        assert!(parse_root_key("00").is_none());
    }
}
