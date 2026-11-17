//! [`MemSectors`]: a [`Sectors`] backend over a [`Disk`] in memory, with
//! power loss injected at a chosen store. For tests (here and in the
//! end-to-end tests) and for anything that wants the node without a state
//! directory.
//!
//! The backend borrows its disk, so after the flash (and the store, the
//! service and the controller around it) is dropped, the disk holds
//! exactly what was durable, ready to be opened again as after a reboot.

use std::collections::BTreeMap;
use std::io;

use super::{SectorBytes, Sectors};

/// The durable sectors: index to bytes; a missing index is erased.
pub type Disk = BTreeMap<u32, Box<SectorBytes>>;

/// A [`Sectors`] backend over a borrowed [`Disk`].
#[derive(Debug)]
pub struct MemSectors<'d> {
    disk: &'d mut Disk,
    stores: usize,
    cut: Option<usize>,
}

impl<'d> MemSectors<'d> {
    /// The backend over `disk`, with no fault armed.
    pub fn new(disk: &'d mut Disk) -> Self {
        MemSectors {
            disk,
            stores: 0,
            cut: None,
        }
    }

    /// The same backend losing power after `n` successful stores: the
    /// `n`-th store from now (counting from 0) and every later one fail
    /// and change nothing, as a store is atomic.
    pub fn cut_after(self, n: usize) -> Self {
        MemSectors {
            cut: Some(n),
            stores: 0,
            ..self
        }
    }

    /// Stores attempted so far (failed ones included).
    pub fn stores(&self) -> usize {
        self.stores
    }

    /// The durable bytes of sector `index`, `None` when erased.
    pub fn durable(&self, index: u32) -> Option<&SectorBytes> {
        self.disk.get(&index).map(|b| &**b)
    }
}

impl Sectors for MemSectors<'_> {
    fn load(&mut self, count: u32) -> io::Result<Vec<(u32, Box<SectorBytes>)>> {
        Ok(self
            .disk
            .range(..count)
            .map(|(i, b)| (*i, b.clone()))
            .collect())
    }

    fn store(&mut self, index: u32, data: Option<&SectorBytes>) -> io::Result<()> {
        let n = self.stores;
        self.stores += 1;
        if self.cut.is_some_and(|cut| n >= cut) {
            return Err(io::Error::other("power lost"));
        }
        match data {
            Some(d) => self.disk.insert(index, Box::new(*d)),
            None => self.disk.remove(&index),
        };
        Ok(())
    }
}
