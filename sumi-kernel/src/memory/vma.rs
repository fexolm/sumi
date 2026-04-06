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
    pub fn remove_overlapping(
        &mut self,
        start: VirtualAddr,
        end: VirtualAddr,
    ) -> Vec<Vma> {
        let mut removed = Vec::new();
        let mut i = 0;
        while i < self.vmas.len() {
            let vma = &self.vmas[i];
            if vma.start.as_usize() < end.as_usize()
                && vma.end.as_usize() > start.as_usize()
            {
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
mod tests {
    use super::*;
    use sumi_abi::{address::VirtualAddr, arch::layout::PAGE_SIZE};

    fn make_vma(start: usize, end: usize) -> Vma {
        Vma {
            start: VirtualAddr::new(start),
            end: VirtualAddr::new(end),
            backing: MappingBacking::Anonymous,
        }
    }

    #[test]
    fn vma_insert_and_find() {
        let mut table = VmaTable::new();
        let start = 0x1000_0000;
        let end = start + PAGE_SIZE;
        table.insert(make_vma(start, end));

        let found = table.find(VirtualAddr::new(start));
        assert!(found.is_some(), "find at vma.start should return the VMA");
        assert_eq!(found.unwrap().start.as_usize(), start);

        let mid = table.find(VirtualAddr::new(start + PAGE_SIZE / 2));
        assert!(mid.is_some(), "find in the middle of the VMA should return it");
    }

    #[test]
    fn vma_find_returns_none_outside_range() {
        let mut table = VmaTable::new();
        let start = 0x1000_0000;
        let end = start + PAGE_SIZE;
        table.insert(make_vma(start, end));

        assert!(
            table.find(VirtualAddr::new(start - 1)).is_none(),
            "address before VMA must not be found"
        );
        assert!(
            table.find(VirtualAddr::new(end + PAGE_SIZE)).is_none(),
            "address after VMA must not be found"
        );
    }

    #[test]
    fn vma_find_adjacent_not_contained() {
        let mut table = VmaTable::new();
        let start = 0x2000_0000;
        let end = start + PAGE_SIZE;
        table.insert(make_vma(start, end));

        assert!(
            table.find(VirtualAddr::new(end)).is_none(),
            "address at exclusive end must not be found"
        );
    }

    #[test]
    fn vma_remove_by_start() {
        let mut table = VmaTable::new();
        let start = 0x3000_0000;
        let end = start + PAGE_SIZE;
        table.insert(make_vma(start, end));

        let removed = table.remove(VirtualAddr::new(start));
        assert!(removed.is_some(), "remove by start address should return the VMA");
        assert_eq!(removed.unwrap().start.as_usize(), start);

        assert!(
            table.find(VirtualAddr::new(start)).is_none(),
            "VMA must not be findable after removal"
        );
    }

    #[test]
    fn vma_remove_nonexistent() {
        let mut table = VmaTable::new();
        let result = table.remove(VirtualAddr::new(0xDEAD_0000));
        assert!(result.is_none(), "removing non-existent VMA must return None");
    }

    #[test]
    fn vma_remove_overlapping_single() {
        let mut table = VmaTable::new();
        let start = 0x4000_0000;
        let end = start + PAGE_SIZE;
        table.insert(make_vma(start, end));

        let removed = table.remove_overlapping(
            VirtualAddr::new(start),
            VirtualAddr::new(end),
        );
        assert_eq!(removed.len(), 1, "exactly one overlapping VMA must be removed");

        assert!(
            table.find(VirtualAddr::new(start)).is_none(),
            "VMA must not be findable after remove_overlapping"
        );
    }

    #[test]
    fn vma_remove_overlapping_none() {
        let mut table = VmaTable::new();
        let start = 0x5000_0000;
        let end = start + PAGE_SIZE;
        table.insert(make_vma(start, end));

        let removed = table.remove_overlapping(
            VirtualAddr::new(end),
            VirtualAddr::new(end + PAGE_SIZE),
        );
        assert!(
            removed.is_empty(),
            "remove_overlapping with non-overlapping range must return nothing"
        );

        assert!(
            table.find(VirtualAddr::new(start)).is_some(),
            "VMA must still be present when remove_overlapping found nothing"
        );
    }

    #[test]
    fn vma_remove_overlapping_multiple() {
        let mut table = VmaTable::new();
        let a_start = 0x6000_0000;
        let a_end = a_start + PAGE_SIZE;
        let b_start = a_end;
        let b_end = b_start + PAGE_SIZE;
        table.insert(make_vma(a_start, a_end));
        table.insert(make_vma(b_start, b_end));

        let removed = table.remove_overlapping(
            VirtualAddr::new(a_start),
            VirtualAddr::new(b_end),
        );
        assert_eq!(removed.len(), 2, "both overlapping VMAs must be removed");
    }

    #[test]
    fn vma_remove_overlapping_partial_overlap() {
        let mut table = VmaTable::new();
        let start = 0x7000_0000;
        let end = start + 2 * PAGE_SIZE;
        table.insert(make_vma(start, end));

        let removed = table.remove_overlapping(
            VirtualAddr::new(start + PAGE_SIZE),
            VirtualAddr::new(end + PAGE_SIZE),
        );
        assert_eq!(removed.len(), 1, "partially overlapping VMA must be removed");

        assert!(
            table.find(VirtualAddr::new(start)).is_none(),
            "removed VMA must not be findable"
        );
    }
}
