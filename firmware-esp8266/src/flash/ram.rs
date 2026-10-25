//! RAM model of the ESP8266 SPI flash behind [`RawFlash`] (test builds
//! only).
//!
//! It backs exactly the payload region (`REGION_BASE..REGION_BASE +
//! REGION_SIZE`, absolute addresses) and behaves like NOR flash: memory
//! starts as `0x00` (not erased), `erase_sector` sets a sector to `0xFF`,
//! and `program` ANDs bits in. It panics on any access the real device
//! would mishandle: outside the region, a program crossing a 256-byte page
//! (SPI page program wraps), or a misaligned sector erase. Every operation
//! is recorded so tests can check how the driver drove the device.

use super::{DeviceError, PAGE, REGION_BASE, REGION_SIZE, RawFlash, SECTOR};

/// One recorded device operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// `read(addr, len)`.
    Read(u32, usize),
    /// `program(addr, len)`.
    Program(u32, usize),
    /// `erase_sector(addr)`.
    Erase(u32),
}

/// RAM NOR flash over the payload region.
pub struct RamFlash {
    mem: Vec<u8>,
    ops: Vec<Op>,
    fail: bool,
}

impl RamFlash {
    /// Unerased (all `0x00`) region.
    pub fn new() -> Self {
        RamFlash {
            mem: vec![0; REGION_SIZE as usize],
            ops: Vec::new(),
            fail: false,
        }
    }

    /// A device whose every operation fails.
    pub fn failing() -> Self {
        RamFlash {
            fail: true,
            ..Self::new()
        }
    }

    /// Operations performed so far, in order.
    pub fn ops(&self) -> &[Op] {
        &self.ops
    }

    /// Byte at absolute address `addr`.
    pub fn byte(&self, addr: u32) -> u8 {
        self.mem[self.index(addr, 1).start]
    }

    /// Overwrites the byte at absolute address `addr` (simulated corruption).
    pub fn corrupt(&mut self, addr: u32, value: u8) {
        let i = self.index(addr, 1).start;
        self.mem[i] = value;
    }

    fn index(&self, addr: u32, len: usize) -> core::ops::Range<usize> {
        assert!(addr >= REGION_BASE, "access below region: {addr:#x}");
        let start = (addr - REGION_BASE) as usize;
        assert!(
            start + len <= self.mem.len(),
            "access beyond region: {addr:#x}+{len}"
        );
        start..start + len
    }

    fn result(&self) -> Result<(), DeviceError> {
        if self.fail { Err(DeviceError) } else { Ok(()) }
    }
}

impl Default for RamFlash {
    fn default() -> Self {
        Self::new()
    }
}

impl RawFlash for RamFlash {
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), DeviceError> {
        self.ops.push(Op::Read(addr, buf.len()));
        self.result()?;
        let r = self.index(addr, buf.len());
        buf.copy_from_slice(&self.mem[r]);
        Ok(())
    }

    fn program(&mut self, addr: u32, data: &[u8]) -> Result<(), DeviceError> {
        self.ops.push(Op::Program(addr, data.len()));
        self.result()?;
        assert!(!data.is_empty(), "empty program at {addr:#x}");
        let first_page = addr / PAGE;
        let last_page = (addr + data.len() as u32 - 1) / PAGE;
        assert_eq!(
            first_page,
            last_page,
            "program crosses a page: {addr:#x}+{}",
            data.len()
        );
        let r = self.index(addr, data.len());
        for (cell, &b) in self.mem[r].iter_mut().zip(data) {
            *cell &= b;
        }
        Ok(())
    }

    fn erase_sector(&mut self, addr: u32) -> Result<(), DeviceError> {
        self.ops.push(Op::Erase(addr));
        self.result()?;
        assert_eq!(addr % SECTOR, 0, "misaligned erase at {addr:#x}");
        let r = self.index(addr, SECTOR as usize);
        self.mem[r].fill(0xFF);
        Ok(())
    }
}
