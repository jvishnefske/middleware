//! Transport-neutral message addresses.
//!
//! A [`Key`] is a `/`-separated path such as `sensor/temperature` or
//! `my.pkg.Thermostat/SetPoint`. Each transport maps it onto its own
//! addressing scheme:
//!
//! * Zenoh uses the key verbatim as a key expression.
//! * gRPC maps a two-segment key `Service/Method` to the path
//!   `/Service/Method` (see [`Key::service_method`]).
//! * BLE hashes the key into a 128-bit characteristic UUID and a 16-bit
//!   advertising id.
//!
//! Validation rules keep keys legal in all three: 1..=[`MAX_KEY_LEN`] bytes,
//! segments are non-empty and contain only ASCII letters, digits, `_`, `-`
//! and `.`. Wildcards are deliberately excluded because BLE and gRPC cannot
//! express them.

/// Longest accepted key, in bytes.
pub const MAX_KEY_LEN: usize = 128;

/// Why a string was rejected as a [`Key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyError {
    /// The string was empty.
    Empty,
    /// The string exceeded [`MAX_KEY_LEN`] bytes.
    TooLong,
    /// Two separators were adjacent, or the key started or ended with `/`.
    EmptySegment,
    /// A byte outside the allowed set appeared at this offset.
    InvalidByte(usize),
}

/// A validated address. Construct with [`Key::new`] or, for literals,
/// [`Key::from_static`], which rejects invalid keys at compile time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key<'a>(&'a str);

const fn allowed(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.'
}

const fn validate(s: &str) -> Result<(), KeyError> {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return Err(KeyError::Empty);
    }
    if bytes.len() > MAX_KEY_LEN {
        return Err(KeyError::TooLong);
    }
    let mut i = 0;
    let mut segment_len = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'/' {
            if segment_len == 0 {
                return Err(KeyError::EmptySegment);
            }
            segment_len = 0;
        } else if allowed(b) {
            segment_len += 1;
        } else {
            return Err(KeyError::InvalidByte(i));
        }
        i += 1;
    }
    if segment_len == 0 {
        return Err(KeyError::EmptySegment);
    }
    Ok(())
}

impl<'a> Key<'a> {
    /// Validates `s` and wraps it.
    pub const fn new(s: &'a str) -> Result<Self, KeyError> {
        match validate(s) {
            Ok(()) => Ok(Key(s)),
            Err(e) => Err(e),
        }
    }

    /// Wraps a literal, failing compilation when used in a `const` with an
    /// invalid key: `const T: Key<'static> = Key::from_static("a/b");`.
    pub const fn from_static(s: &'static str) -> Key<'static> {
        match validate(s) {
            Ok(()) => Key(s),
            Err(_) => panic!("invalid middleware key"),
        }
    }

    /// The key text.
    pub const fn as_str(&self) -> &'a str {
        self.0
    }

    /// The key bytes.
    pub const fn as_bytes(&self) -> &'a [u8] {
        self.0.as_bytes()
    }

    /// Iterates the `/`-separated segments.
    pub fn segments(&self) -> core::str::Split<'a, char> {
        self.0.split('/')
    }

    /// Splits a two-segment key into `(service, method)`, the shape gRPC
    /// needs. Returns `None` for any other segment count.
    pub fn service_method(&self) -> Option<(&'a str, &'a str)> {
        let (service, method) = self.0.split_once('/')?;
        if method.contains('/') {
            None
        } else {
            Some((service, method))
        }
    }
}

impl core::fmt::Display for Key<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.0)
    }
}

/// Which keys a subscription receives.
///
/// Keys themselves never contain wildcards (BLE and gRPC cannot express
/// them); a filter is the subscriber-side way to ask for a family of keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyFilter<'a> {
    /// Exactly this key.
    Exact(Key<'a>),
    /// This key and every key below it, segment-wise: `app` matches `app`,
    /// `app/x` and `app/x/y`, but not `apple`. Zenoh expresses it as
    /// `<prefix>/**` plus the prefix itself.
    Prefix(Key<'a>),
}

impl<'a> KeyFilter<'a> {
    /// True if `key` is selected by this filter.
    pub fn matches(&self, key: Key<'_>) -> bool {
        match self {
            KeyFilter::Exact(k) => k.as_str() == key.as_str(),
            KeyFilter::Prefix(p) => {
                let (p, k) = (p.as_bytes(), key.as_bytes());
                k.starts_with(p) && (k.len() == p.len() || k.get(p.len()) == Some(&b'/'))
            }
        }
    }

    /// The key the filter is built on.
    pub const fn key(&self) -> Key<'a> {
        match self {
            KeyFilter::Exact(k) | KeyFilter::Prefix(k) => *k,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_match_exact_and_segment_prefixes() {
        let app = Key::from_static("app");
        let exact = KeyFilter::Exact(Key::from_static("app/x"));
        let prefix = KeyFilter::Prefix(app);
        assert!(exact.matches(Key::from_static("app/x")));
        assert!(!exact.matches(Key::from_static("app/x/y")));
        for k in ["app", "app/x", "app/x/y"] {
            assert!(prefix.matches(Key::new(k).unwrap()), "{k}");
        }
        for k in ["apple", "ap", "other/app"] {
            assert!(!prefix.matches(Key::new(k).unwrap()), "{k}");
        }
        assert_eq!(prefix.key(), app);
    }

    #[test]
    fn accepts_paths_and_grpc_names() {
        assert!(Key::new("sensor/temperature").is_ok());
        assert!(Key::new("my.pkg.Thermostat/SetPoint").is_ok());
        assert!(Key::new("a").is_ok());
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(Key::new(""), Err(KeyError::Empty));
        assert_eq!(Key::new("/a"), Err(KeyError::EmptySegment));
        assert_eq!(Key::new("a/"), Err(KeyError::EmptySegment));
        assert_eq!(Key::new("a//b"), Err(KeyError::EmptySegment));
        assert_eq!(Key::new("a/*"), Err(KeyError::InvalidByte(2)));
        assert_eq!(Key::new("a b"), Err(KeyError::InvalidByte(1)));
        let long = [b'a'; MAX_KEY_LEN + 1];
        let long = core::str::from_utf8(&long).unwrap();
        assert_eq!(Key::new(long), Err(KeyError::TooLong));
    }

    #[test]
    fn service_method_requires_two_segments() {
        const K: Key<'static> = Key::from_static("pkg.Svc/Call");
        assert_eq!(K.service_method(), Some(("pkg.Svc", "Call")));
        assert_eq!(Key::from_static("a").service_method(), None);
        assert_eq!(Key::from_static("a/b/c").service_method(), None);
    }
}
