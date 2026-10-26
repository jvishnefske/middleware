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

/// Anything that can lend a validated [`Key`]: a borrowed [`Key`] or an
/// owned [`KeyBuf`]. Lets [`Filter`] and other key holders work with either.
pub trait AsKey {
    /// The key, borrowed from `self`.
    fn as_key(&self) -> Key<'_>;
}

impl AsKey for Key<'_> {
    fn as_key(&self) -> Key<'_> {
        *self
    }
}

/// An owned, validated key of at most `N` bytes (and at most
/// [`MAX_KEY_LEN`]), stored inline: no heap, `Copy`, fixed size.
///
/// For state that must outlive the buffer a [`Key`] borrows from, such as a
/// learned subscription or a filter kept across `recv` calls. Construct with
/// [`KeyBuf::new`] or [`KeyBuf::from_key`]; borrow it back with
/// [`AsKey::as_key`].
#[derive(Clone, Copy)]
pub struct KeyBuf<const N: usize> {
    len: u8,
    bytes: [u8; N],
}

impl<const N: usize> KeyBuf<N> {
    /// Compile-time check that `N` fits the `u8` length and the key limit.
    const CAPACITY_OK: () = assert!(N <= MAX_KEY_LEN && N <= u8::MAX as usize, "KeyBuf capacity");

    /// Validates `s` and copies it in. [`KeyError::TooLong`] if it is
    /// longer than `N`.
    pub const fn new(s: &str) -> Result<Self, KeyError> {
        match Key::new(s) {
            Ok(k) => Self::from_key(k),
            Err(e) => Err(e),
        }
    }

    /// Copies an already-validated key. [`KeyError::TooLong`] if it is
    /// longer than `N`.
    pub const fn from_key(key: Key<'_>) -> Result<Self, KeyError> {
        let () = Self::CAPACITY_OK;
        let src = key.as_bytes();
        if src.len() > N {
            return Err(KeyError::TooLong);
        }
        let mut bytes = [0u8; N];
        let mut i = 0;
        while i < src.len() {
            bytes[i] = src[i];
            i += 1;
        }
        // Lossless: src.len() <= N <= u8::MAX (CAPACITY_OK).
        Ok(KeyBuf { len: src.len() as u8, bytes })
    }

    /// The key text.
    pub fn as_str(&self) -> &str {
        self.as_key().as_str()
    }
}

impl<const N: usize> AsKey for KeyBuf<N> {
    fn as_key(&self) -> Key<'_> {
        let used = self.bytes.get(..usize::from(self.len)).unwrap_or(&[]);
        // The bytes were copied from a validated key, so they are valid UTF-8
        // and a valid key; the fallbacks are unreachable.
        let s = core::str::from_utf8(used).unwrap_or("");
        Key::new(s).unwrap_or(Key::from_static("_"))
    }
}

impl<const N: usize> PartialEq for KeyBuf<N> {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl<const N: usize> Eq for KeyBuf<N> {}

impl<const N: usize> core::hash::Hash for KeyBuf<N> {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.as_key().hash(state);
    }
}

impl<const N: usize> PartialEq<Key<'_>> for KeyBuf<N> {
    fn eq(&self, other: &Key<'_>) -> bool {
        self.as_str() == other.as_str()
    }
}

impl<const N: usize> core::fmt::Debug for KeyBuf<N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("KeyBuf").field(&self.as_str()).finish()
    }
}

impl<const N: usize> core::fmt::Display for KeyBuf<N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'a, const N: usize> TryFrom<Key<'a>> for KeyBuf<N> {
    type Error = KeyError;
    fn try_from(key: Key<'a>) -> Result<Self, KeyError> {
        Self::from_key(key)
    }
}

/// Which keys a subscription receives, over any key holder `K` ([`Key`] or
/// [`KeyBuf`]). [`KeyFilter`] is the borrowed form every transport takes;
/// [`KeyFilterBuf`] is the owned form for filters that are stored.
///
/// Keys themselves never contain wildcards (BLE and gRPC cannot express
/// them); a filter is the subscriber-side way to ask for a family of keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Filter<K> {
    /// Exactly this key.
    Exact(K),
    /// This key and every key below it, segment-wise: `app` matches `app`,
    /// `app/x` and `app/x/y`, but not `apple`. Zenoh expresses it as
    /// `<prefix>/**` plus the prefix itself.
    Prefix(K),
}

/// A filter over a borrowed [`Key`]; what
/// [`Subscribe::subscribe`](crate::transport::Subscribe::subscribe) takes.
pub type KeyFilter<'a> = Filter<Key<'a>>;

/// A filter that owns its key, for subscribers that store filters.
pub type KeyFilterBuf<const N: usize> = Filter<KeyBuf<N>>;

impl<K: AsKey> Filter<K> {
    /// True if `key` is selected by this filter.
    pub fn matches(&self, key: Key<'_>) -> bool {
        match self {
            Filter::Exact(k) => k.as_key().as_str() == key.as_str(),
            Filter::Prefix(p) => {
                let p = p.as_key();
                let (p, k) = (p.as_bytes(), key.as_bytes());
                k.starts_with(p) && (k.len() == p.len() || k.get(p.len()) == Some(&b'/'))
            }
        }
    }

    /// The same filter borrowing its key.
    pub fn as_filter(&self) -> KeyFilter<'_> {
        match self {
            Filter::Exact(k) => Filter::Exact(k.as_key()),
            Filter::Prefix(k) => Filter::Prefix(k.as_key()),
        }
    }

    /// An owned copy, [`KeyError::TooLong`] if the key exceeds `N` bytes.
    pub fn to_buf<const N: usize>(&self) -> Result<KeyFilterBuf<N>, KeyError> {
        Ok(match self {
            Filter::Exact(k) => Filter::Exact(KeyBuf::from_key(k.as_key())?),
            Filter::Prefix(k) => Filter::Prefix(KeyBuf::from_key(k.as_key())?),
        })
    }
}

impl<'a> KeyFilter<'a> {
    /// The key the filter is built on.
    pub const fn key(&self) -> Key<'a> {
        match self {
            Filter::Exact(k) | Filter::Prefix(k) => *k,
        }
    }
}

impl<const N: usize> KeyFilterBuf<N> {
    /// The key the filter is built on, borrowed from the filter.
    pub fn key(&self) -> Key<'_> {
        match self {
            Filter::Exact(k) | Filter::Prefix(k) => k.as_key(),
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
    fn k1_key_buf_owns_a_validated_key() {
        let k: KeyBuf<16> = KeyBuf::new("app/hello").unwrap();
        assert_eq!(k.as_str(), "app/hello");
        assert_eq!(k, Key::from_static("app/hello"));
        assert_eq!(k.as_key(), Key::from_static("app/hello"));
        assert_eq!(KeyBuf::<4>::new("app/hello"), Err(KeyError::TooLong));
        assert_eq!(KeyBuf::<16>::new("a b"), Err(KeyError::InvalidByte(1)));
        assert_eq!(KeyBuf::<16>::new(""), Err(KeyError::Empty));
        const C: KeyBuf<8> = match KeyBuf::new("a/b") {
            Ok(k) => k,
            Err(_) => panic!(),
        };
        assert_eq!(C.as_str(), "a/b");
        assert_eq!(KeyBuf::<16>::try_from(Key::from_static("x")).unwrap().as_str(), "x");
    }

    #[test]
    fn k2_owned_and_borrowed_filters_agree() {
        let borrowed = KeyFilter::Prefix(Key::from_static("app"));
        let owned: KeyFilterBuf<8> = borrowed.to_buf().unwrap();
        for k in ["app", "app/x", "apple", "other"] {
            let k = Key::new(k).unwrap();
            assert_eq!(owned.matches(k), borrowed.matches(k), "{k}");
        }
        assert_eq!(owned.as_filter(), borrowed);
        assert_eq!(owned.key(), Key::from_static("app"));
        let exact: KeyFilterBuf<8> = KeyFilter::Exact(Key::from_static("app/x")).to_buf().unwrap();
        assert!(exact.matches(Key::from_static("app/x")));
        assert!(!exact.matches(Key::from_static("app/x/y")));
        assert_eq!(
            KeyFilter::Exact(Key::from_static("app/long")).to_buf::<4>(),
            Err(KeyError::TooLong)
        );
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
