//! The updates key space (crate docs).

/// The prefix of every key.
pub const ROOT: &str = "@mw/updates";

/// What a key under [`ROOT`] names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateKey {
    /// `latest/<channel>/<target>`.
    Latest {
        /// The channel.
        channel: String,
        /// The target chip.
        target: String,
    },
    /// `img/<sha256 hex>/<offset>`.
    Img {
        /// The image hash.
        sha256: [u8; 32],
        /// Byte offset.
        offset: u32,
    },
    /// `publish/<channel>/<target>`.
    Publish {
        /// The channel.
        channel: String,
        /// The target chip.
        target: String,
    },
    /// `status/<node>`.
    Status {
        /// Who reports.
        node: String,
    },
}

/// Whether `s` may be a channel, target or node chunk: 1 to 32 bytes of
/// `a-z 0-9 . _ -`, so it is one literal key chunk (no `/`, no wildcard,
/// no `@`) and a release cert name ([`middleware::loader::release::NAME_MAX`]).
pub fn valid_name(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}

impl UpdateKey {
    /// The key expression.
    pub fn render(&self) -> String {
        match self {
            UpdateKey::Latest { channel, target } => format!("{ROOT}/latest/{channel}/{target}"),
            UpdateKey::Img { sha256, offset } => {
                format!("{ROOT}/img/{}/{offset}", crate::hex(sha256))
            }
            UpdateKey::Publish { channel, target } => {
                format!("{ROOT}/publish/{channel}/{target}")
            }
            UpdateKey::Status { node } => format!("{ROOT}/status/{node}"),
        }
    }

    /// Parses a concrete key; `None` for anything else (wildcards, other
    /// prefixes, bad names).
    pub fn parse(key: &str) -> Option<Self> {
        let rest = key.strip_prefix(ROOT)?.strip_prefix('/')?;
        let parts: Vec<&str> = rest.split('/').collect();
        let named = |c: &str, t: &str| (valid_name(c) && valid_name(t)).then_some(());
        match parts.as_slice() {
            ["latest", c, t] => named(c, t).map(|_| UpdateKey::Latest {
                channel: (*c).to_owned(),
                target: (*t).to_owned(),
            }),
            ["publish", c, t] => named(c, t).map(|_| UpdateKey::Publish {
                channel: (*c).to_owned(),
                target: (*t).to_owned(),
            }),
            ["img", sha, text] => {
                let bytes = crate::unhex(sha)?;
                let sha256 = <[u8; 32]>::try_from(bytes.as_slice()).ok()?;
                // Canonical digits only: one key per chunk.
                let offset: u32 = text.parse().ok()?;
                (offset.to_string() == *text).then_some(UpdateKey::Img { sha256, offset })
            }
            ["status", n] if valid_name(n) => Some(UpdateKey::Status {
                node: (*n).to_owned(),
            }),
            _ => None,
        }
    }
}

/// The subscription of every `latest` key of `channel`.
pub fn latest_of_channel(channel: &str) -> String {
    format!("{ROOT}/latest/{channel}/*")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn up4_keys_render_and_parse() {
        let all = [
            UpdateKey::Latest {
                channel: "stable".into(),
                target: "rp2350".into(),
            },
            UpdateKey::Img {
                sha256: [0xab; 32],
                offset: 65536,
            },
            UpdateKey::Publish {
                channel: "test".into(),
                target: "linux-x86_64".into(),
            },
            UpdateKey::Status {
                node: "pi-3".into(),
            },
        ];
        for k in all {
            assert_eq!(UpdateKey::parse(&k.render()), Some(k.clone()), "{k:?}");
        }
        for bad in [
            "@mw/updates/latest/stable",
            "@mw/updates/latest/stable/*",
            "@mw/updates/latest/Stable/rp2350",
            "@mw/updates/latest/a/b/c",
            "@mw/updates/img/00/0",
            "@mw/updates/img/abababababababababababababababababababababababababababababababab/01",
            "@mw/updates/status/a/b",
            "@mw/other/latest/a/b",
            "app/updates/latest/a/b",
        ] {
            assert_eq!(UpdateKey::parse(bad), None, "{bad}");
        }
        assert!(valid_name("linux-aarch64"));
        assert!(!valid_name(""));
        assert!(!valid_name(&"a".repeat(33)));
        assert!(!valid_name("a/b"));
        assert!(!valid_name("**"));
    }
}
