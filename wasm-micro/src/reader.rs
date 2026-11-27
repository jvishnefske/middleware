//! A bounds-checked cursor over the module image.

use crate::Error;

#[derive(Clone, Copy)]
pub(crate) struct Reader<'a> {
    pub b: &'a [u8],
    pub pos: usize,
}

impl<'a> Reader<'a> {
    pub fn u8(&mut self) -> Result<u8, Error> {
        let v = *self.b.get(self.pos).ok_or(Error::Malformed)?;
        self.pos += 1;
        Ok(v)
    }

    /// LEB128, at most ten bytes. Unsigned values are zero-extended and
    /// signed ones sign-extended to 64 bits; callers truncate.
    pub fn leb(&mut self, signed: bool) -> Result<u64, Error> {
        let mut v = 0u64;
        let mut shift = 0u32;
        loop {
            let b = self.u8()?;
            v |= u64::from(b & 0x7F) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                if signed && b & 0x40 != 0 && shift < 64 {
                    v |= !0u64 << shift;
                }
                return Ok(v);
            }
            if shift >= 64 {
                return Err(Error::Malformed);
            }
        }
    }

    pub fn u32(&mut self) -> Result<u32, Error> {
        Ok(self.leb(false)? as u32)
    }

    /// A vector length: no more than the bytes left, since every element
    /// takes at least one.
    pub fn count(&mut self) -> Result<u32, Error> {
        let n = self.u32()?;
        if n as usize > self.b.len().saturating_sub(self.pos) {
            return Err(Error::Malformed);
        }
        Ok(n)
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.pos.checked_add(n).ok_or(Error::Malformed)?;
        let s = self.b.get(self.pos..end).ok_or(Error::Malformed)?;
        self.pos = end;
        Ok(s)
    }

    pub fn skip(&mut self, n: usize) -> Result<(), Error> {
        self.bytes(n).map(|_| ())
    }

    /// A length-prefixed byte string (names, data segments).
    pub fn name(&mut self) -> Result<&'a [u8], Error> {
        let n = self.u32()? as usize;
        self.bytes(n)
    }

    /// `n` (at most 8) little-endian bytes.
    pub fn le(&mut self, n: usize) -> Result<u64, Error> {
        let mut v = 0u64;
        for (i, b) in self.bytes(n)?.iter().take(8).enumerate() {
            v |= u64::from(*b) << (8 * i);
        }
        Ok(v)
    }

    /// Skips a block type and reports whether it was a type index.
    pub fn blocktype(&mut self) -> Result<Option<u32>, Error> {
        let b = *self.b.get(self.pos).ok_or(Error::Malformed)?;
        // One byte with bit 6 set and bit 7 clear is a negative s33:
        // 0x40 (empty) or a value type.
        if b & 0xC0 == 0x40 {
            self.pos += 1;
            return Ok(if b == 0x40 { None } else { Some(u32::MAX) });
        }
        self.u32().map(Some)
    }
}
