use alloc::vec::Vec;
use sumi_abi::address::VirtualAddr;

#[derive(Debug)]
pub enum MappingBacking {
    Anonymous,
    Dax {
        dax_offset: usize,
        fuse_fh: u64,
        fuse_nodeid: u64,
        file_offset: u64,
    },
    PrivateFile {
        fuse_fh: u64,
        fuse_nodeid: u64,
    },
}

#[derive(Debug)]
pub struct Vma {
    pub start: VirtualAddr,
    /// Exclusive end address.
    pub end: VirtualAddr,
    pub backing: MappingBacking,
}

pub struct VmaTable {
    vmas: Vec<Vma>,
}

impl Default for VmaTable {
    fn default() -> Self {
        Self::new()
    }
}

impl VmaTable {
    pub const fn new() -> Self {
        Self { vmas: Vec::new() }
    }

    /// Insert a VMA.
    pub fn insert(&mut self, vma: Vma) {
        self.vmas.push(vma);
    }

    /// Remove the VMA whose start equals `start`. Returns the removed VMA.
    pub fn remove(&mut self, start: VirtualAddr) -> Option<Vma> {
        let pos = self.vmas.iter().position(|v| v.start == start)?;
        Some(self.vmas.swap_remove(pos))
    }

    /// Find the first VMA that contains `addr` (start <= addr < end).
    pub fn find(&self, addr: VirtualAddr) -> Option<&Vma> {
        self.vmas.iter().find(|vma| {
            vma.start.as_usize() <= addr.as_usize() && addr.as_usize() < vma.end.as_usize()
        })
    }

    /// Remove all VMAs that overlap with [start, end).
    pub fn remove_overlapping(&mut self, start: VirtualAddr, end: VirtualAddr) -> Vec<Vma> {
        let mut removed = Vec::new();
        let mut i = 0;
        while i < self.vmas.len() {
            let vma = &self.vmas[i];
            if vma.start.as_usize() < end.as_usize() && vma.end.as_usize() > start.as_usize() {
                removed.push(self.vmas.swap_remove(i));
                // Don't increment i — swap_remove moved the last element here.
            } else {
                i += 1;
            }
        }
        removed
    }
}

#[cfg(test)]
#[path = "vma_test.rs"]
mod vma_test;
