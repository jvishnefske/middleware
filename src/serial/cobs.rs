//! Consistent Overhead Byte Stuffing.
//!
//! COBS removes every `0x00` from a frame so `0x00` can delimit frames on a
//! byte stream. Overhead is at most one byte per 254 bytes plus one.

/// Worst-case encoded length of `n` input bytes, excluding the delimiter.
pub const fn max_encoded_len(n: usize) -> usize {
    n + n / 254 + 1
}

/// Encodes `src` into `dst` (no trailing delimiter). Returns the encoded
/// length, or `None` when `dst` is too small.
pub fn encode(src: &[u8], dst: &mut [u8]) -> Option<usize> {
    let mut code_idx = 0;
    let mut out = 1;
    let mut code: u8 = 1;
    dst.first()?;
    for &b in src {
        if b == 0 {
            *dst.get_mut(code_idx)? = code;
            code_idx = out;
            out += 1;
            code = 1;
        } else {
            *dst.get_mut(out)? = b;
            out += 1;
            code += 1;
            if code == 0xFF {
                *dst.get_mut(code_idx)? = code;
                code_idx = out;
                out += 1;
                code = 1;
            }
        }
    }
    *dst.get_mut(code_idx)? = code;
    Some(out)
}

/// Decodes `buf` (one frame, delimiter removed) in place and returns the
/// decoded length. Returns `None` for a malformed encoding, including any
/// embedded zero byte.
pub fn decode_in_place(buf: &mut [u8]) -> Option<usize> {
    let len = buf.len();
    let mut r = 0;
    let mut w = 0;
    while r < len {
        let code = *buf.get(r)?;
        if code == 0 {
            return None;
        }
        r += 1;
        for _ in 1..code {
            let b = *buf.get(r)?;
            if b == 0 {
                return None;
            }
            *buf.get_mut(w)? = b;
            w += 1;
            r += 1;
        }
        if code != 0xFF && r < len {
            *buf.get_mut(w)? = 0;
            w += 1;
        }
    }
    Some(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(src: &[u8]) {
        let mut enc = [0u8; 1100];
        let n = encode(src, &mut enc).unwrap();
        assert!(n <= max_encoded_len(src.len()));
        assert!(!enc[..n].contains(&0));
        let m = decode_in_place(&mut enc[..n]).unwrap();
        assert_eq!(&enc[..m], src);
    }

    #[test]
    fn reference_vectors() {
        let mut enc = [0u8; 8];
        let n = encode(&[0x11, 0x22, 0x00, 0x33], &mut enc).unwrap();
        assert_eq!(&enc[..n], &[0x03, 0x11, 0x22, 0x02, 0x33]);
        let n = encode(&[0x00], &mut enc).unwrap();
        assert_eq!(&enc[..n], &[0x01, 0x01]);
        let n = encode(&[], &mut enc).unwrap();
        assert_eq!(&enc[..n], &[0x01]);
    }

    #[test]
    fn round_trips_including_long_runs() {
        round_trip(b"");
        round_trip(&[0, 0, 0]);
        round_trip(&[1u8; 254]);
        round_trip(&[1u8; 255]);
        let mut mixed = [0u8; 1000];
        for (i, b) in mixed.iter_mut().enumerate() {
            *b = (i % 7) as u8;
        }
        round_trip(&mixed);
    }

    #[test]
    fn rejects_malformed_and_small_buffers() {
        assert_eq!(decode_in_place(&mut [0x03, 0x11]), None);
        assert_eq!(decode_in_place(&mut [0x02, 0x00]), None);
        assert_eq!(decode_in_place(&mut [0x00]), None);
        assert_eq!(encode(&[1, 2, 3], &mut [0u8; 3]), None);
        assert_eq!(encode(&[], &mut []), None);
    }
}
