//! The node's manifest (ABI 10): what the node offers payloads, read through
//! the `manifest` import.
//!
//! The bytes are the ones `sys/manifest` replies with, node id included, and
//! [`Manifest::decode`] reads them (the same decoder the host tools use, so a
//! payload sees the same records and the same forward compatibility: unknown
//! records are skipped).
//!
//! The import writes into guest memory, so the buffer is passed as
//! `&mut [u8]`; a buffer shorter than the manifest is refused and nothing is
//! written. [`MANIFEST_MAX`] bytes always hold one.

use crate::{AbiError, check, sys};
pub use middleware::loader::manifest::{MANIFEST_MAX, Manifest};

/// Copies the encoded manifest into `out` and returns its length.
///
/// Fails with [`AbiError::InvalidArgument`] when `out` is shorter than the
/// manifest (nothing is written) and [`AbiError::Unsupported`] when the
/// device has no manifest.
pub fn manifest_bytes(out: &mut [u8]) -> Result<usize, AbiError> {
    let n = check(sys::manifest(out))?;
    usize::try_from(n).map_err(|_| AbiError::Unsupported)
}

/// Reads and decodes the node's manifest.
///
/// Fails like [`manifest_bytes`], and with [`AbiError::Unsupported`] when
/// the bytes do not decode (a node speaking another manifest version).
pub fn manifest() -> Result<Manifest, AbiError> {
    let mut buf = [0u8; MANIFEST_MAX];
    let n = manifest_bytes(&mut buf)?;
    let bytes = buf.get(..n).ok_or(AbiError::Unsupported)?;
    Manifest::decode(bytes).map_err(|_| AbiError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::SenderId;

    #[test]
    fn host_stubs_have_no_manifest() {
        assert_eq!(manifest(), Err(AbiError::Unsupported));
        let mut out = [0u8; MANIFEST_MAX];
        assert_eq!(manifest_bytes(&mut out), Err(AbiError::Unsupported));
    }

    #[test]
    fn the_host_encoding_decodes_with_the_sdk_reader() {
        let m = Manifest::new(crate::ABI_VERSION).with_node(SenderId([1; 8]));
        let mut out = [0u8; MANIFEST_MAX];
        let n = m.encode(&mut out).expect("fits");
        let back = Manifest::decode(&out[..n]).expect("decodes");
        assert_eq!(back.abi, crate::ABI_VERSION);
        assert_eq!(back.node, Some(SenderId([1; 8])));
    }
}
