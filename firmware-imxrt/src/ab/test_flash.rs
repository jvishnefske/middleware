//! NOR flash models for the host tests (test builds only).
//!
//! [`BlockFlash`] behaves like the HyperFlash behind the driver: 256 KiB
//! erase blocks to `0xFF`, programs that only clear bits, 2-byte (one
//! HyperFlash word) read and write alignment. It stores only the blocks
//! that were written, so a model of the whole map (slot B ends at 12 MiB)
//! is cheap to clone.
//!
//! Power loss is injected with [`BlockFlash::cut_at`]: the erase or write
//! with that index (counting from 0 after the call) is torn, half of its
//! bytes applied, and it and every later erase or write fail until
//! [`BlockFlash::restore`] (the pattern of firmware-rp's `ram_flash`).
//!
//! [`SmallEraseFlash`] only has the wrong erase size, for the geometry
//! checks.

extern crate std;

use crate::layout::BLOCK;
use embedded_storage::nor_flash::{ErrorType, NorFlash, NorFlashErrorKind, ReadNorFlash};
use std::vec;
use std::vec::Vec;

/// An erase or write, as logged (offsets from the start of flash).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// `erase(from, to)`.
    Erase(u32, u32),
    /// `write(at, len)`.
    Write(u32, usize),
}

/// Read and write granule of the model.
pub const GRANULE: usize = 2;

/// Sparse block-erased NOR flash with power-cut injection.
#[derive(Debug, Clone)]
pub struct BlockFlash {
    blocks: Vec<Option<Vec<u8>>>,
    size: usize,
    /// Erase calls made (including torn ones).
    pub erases: usize,
    /// Write calls made (including torn ones).
    pub writes: usize,
    /// Every erase and write, in order (including torn ones).
    pub log: Vec<Op>,
    ops: usize,
    cut: Option<usize>,
}

impl BlockFlash {
    /// `size` bytes of erased flash.
    pub fn erased(size: usize) -> Self {
        BlockFlash {
            blocks: vec![None; size.div_ceil(BLOCK as usize)],
            size,
            erases: 0,
            writes: 0,
            log: Vec::new(),
            ops: 0,
            cut: None,
        }
    }

    /// Arms power loss at the `op`-th erase or write from now.
    pub fn cut_at(&mut self, op: usize) {
        self.ops = 0;
        self.cut = Some(op);
    }

    /// Power is back: no more faults.
    pub fn restore(&mut self) {
        self.cut = None;
    }

    /// Erases and writes since the last [`BlockFlash::cut_at`] (or ever).
    pub fn ops(&self) -> usize {
        self.ops
    }

    /// Whether power has been cut.
    pub fn is_cut(&self) -> bool {
        self.cut.is_some_and(|c| self.ops > c)
    }

    /// `len` bytes at `at` (test helper, no alignment rule).
    pub fn bytes(&self, at: usize, len: usize) -> Vec<u8> {
        (at..at + len).map(|i| self.byte(i)).collect()
    }

    /// Programs `data` at `at` without counting an operation (factory
    /// flashing of a test fixture); the bytes must be erased.
    pub fn program(&mut self, at: usize, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            let p = self.byte_mut(at + i);
            *p &= b;
        }
    }

    fn byte(&self, i: usize) -> u8 {
        let b = BLOCK as usize;
        match self.blocks.get(i / b) {
            Some(Some(block)) => block[i % b],
            _ => 0xFF,
        }
    }

    fn byte_mut(&mut self, i: usize) -> &mut u8 {
        let b = BLOCK as usize;
        let block = self.blocks[i / b].get_or_insert_with(|| vec![0xFF; b]);
        &mut block[i % b]
    }

    fn admit(&mut self, len: usize) -> Result<usize, usize> {
        let op = self.ops;
        self.ops += 1;
        match self.cut {
            Some(cut) if op == cut => Err(len / 2),
            Some(cut) if op > cut => Err(0),
            _ => Ok(len),
        }
    }

    fn check(&self, offset: u32, len: usize, align: usize) -> Result<(), NorFlashErrorKind> {
        if !(offset as usize).is_multiple_of(align) || !len.is_multiple_of(align) {
            return Err(NorFlashErrorKind::NotAligned);
        }
        if offset as usize + len > self.size {
            return Err(NorFlashErrorKind::OutOfBounds);
        }
        Ok(())
    }
}

impl ErrorType for BlockFlash {
    type Error = NorFlashErrorKind;
}

impl ReadNorFlash for BlockFlash {
    const READ_SIZE: usize = GRANULE;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        self.check(offset, bytes.len(), GRANULE)?;
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = self.byte(offset as usize + i);
        }
        Ok(())
    }

    fn capacity(&self) -> usize {
        self.size
    }
}

impl NorFlash for BlockFlash {
    const WRITE_SIZE: usize = GRANULE;
    const ERASE_SIZE: usize = BLOCK as usize;

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        self.check(offset, bytes.len(), GRANULE)?;
        self.writes += 1;
        self.log.push(Op::Write(offset, bytes.len()));
        let admitted = self.admit(bytes.len());
        let n = admitted.unwrap_or_else(|n| n);
        for (i, &b) in bytes.iter().enumerate().take(n) {
            *self.byte_mut(offset as usize + i) &= b;
        }
        admitted.map(|_| ()).map_err(|_| NorFlashErrorKind::Other)
    }

    fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        if from > to {
            return Err(NorFlashErrorKind::NotAligned);
        }
        self.check(from, (to - from) as usize, BLOCK as usize)?;
        self.erases += 1;
        self.log.push(Op::Erase(from, to));
        let len = (to - from) as usize;
        let admitted = self.admit(len);
        let n = admitted.unwrap_or_else(|n| n);
        // Applied: the first `n` bytes (all of them unless torn).
        let b = BLOCK as usize;
        let start = from as usize;
        let mut at = start;
        while at < start + n {
            let k = at / b;
            let upto = ((k + 1) * b).min(start + n);
            if at.is_multiple_of(b) && upto - at == b {
                self.blocks[k] = None;
            } else if let Some(block) = self.blocks[k].as_mut() {
                block[at % b..at % b + (upto - at)].fill(0xFF);
            }
            at = upto;
        }
        admitted.map(|_| ()).map_err(|_| NorFlashErrorKind::Other)
    }
}

/// A flash whose erase size (4 KiB) is not the HyperFlash's.
#[derive(Debug, Default)]
pub struct SmallEraseFlash;

impl ErrorType for SmallEraseFlash {
    type Error = NorFlashErrorKind;
}

impl ReadNorFlash for SmallEraseFlash {
    const READ_SIZE: usize = 1;

    fn read(&mut self, _offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        bytes.fill(0xFF);
        Ok(())
    }

    fn capacity(&self) -> usize {
        crate::layout::FLASH_SIZE as usize
    }
}

impl NorFlash for SmallEraseFlash {
    const WRITE_SIZE: usize = 1;
    const ERASE_SIZE: usize = 4096;

    fn write(&mut self, _offset: u32, _bytes: &[u8]) -> Result<(), Self::Error> {
        Ok(())
    }

    fn erase(&mut self, _from: u32, _to: u32) -> Result<(), Self::Error> {
        Ok(())
    }
}
