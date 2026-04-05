use sumi_abi::address::VirtualAddr;

pub const MAX_VMAS: usize = 256;

#[derive(Debug)]
pub enum VmaError {
    TableFull,
    NotFound,
}

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
    vmas: [Option<Vma>; MAX_VMAS],
}

impl VmaTable {
    pub const fn new() -> Self {
        // Option<Vma> is not Copy, but None is valid for const init via this pattern.
        Self {
            vmas: [const { None }; MAX_VMAS],
        }
    }

    /// Insert a VMA, returning its slot index on success.
    pub fn insert(&mut self, vma: Vma) -> Result<usize, VmaError> {
        for (i, slot) in self.vmas.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(vma);
                return Ok(i);
            }
        }
        Err(VmaError::TableFull)
    }

    /// Remove the VMA whose start equals `start`. Returns the removed VMA.
    pub fn remove(&mut self, start: VirtualAddr) -> Option<Vma> {
        for slot in &mut self.vmas {
            if let Some(vma) = slot {
                if vma.start == start {
                    return slot.take();
                }
            }
        }
        None
    }

    /// Find the first VMA that contains `addr` (start <= addr < end).
    pub fn find(&self, addr: VirtualAddr) -> Option<&Vma> {
        for slot in &self.vmas {
            if let Some(vma) = slot {
                if vma.start.as_usize() <= addr.as_usize()
                    && addr.as_usize() < vma.end.as_usize()
                {
                    return Some(vma);
                }
            }
        }
        None
    }

    /// Remove all VMAs that overlap with [start, end). Returns up to 4 removed VMAs.
    pub fn remove_overlapping(
        &mut self,
        start: VirtualAddr,
        end: VirtualAddr,
    ) -> [Option<Vma>; 4] {
        let mut result: [Option<Vma>; 4] = [const { None }; 4];
        let mut count = 0;

        for slot in &mut self.vmas {
            if count >= 4 {
                break;
            }
            if let Some(vma) = slot {
                // Overlaps if vma.start < end && vma.end > start
                if vma.start.as_usize() < end.as_usize()
                    && vma.end.as_usize() > start.as_usize()
                {
                    result[count] = slot.take();
                    count += 1;
                }
            }
        }

        result
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
        table.insert(make_vma(start, end)).expect("insert should succeed");

        // Address at start of range.
        let found = table.find(VirtualAddr::new(start));
        assert!(found.is_some(), "find at vma.start should return the VMA");
        assert_eq!(found.unwrap().start.as_usize(), start);

        // Address in the middle of the range.
        let mid = table.find(VirtualAddr::new(start + PAGE_SIZE / 2));
        assert!(mid.is_some(), "find in the middle of the VMA should return it");
    }

    #[test]
    fn vma_find_returns_none_outside_range() {
        let mut table = VmaTable::new();
        let start = 0x1000_0000;
        let end = start + PAGE_SIZE;
        table.insert(make_vma(start, end)).unwrap();

        // Completely before.
        assert!(
            table.find(VirtualAddr::new(start - 1)).is_none(),
            "address before VMA must not be found"
        );
        // Completely after.
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
        table.insert(make_vma(start, end)).unwrap();

        // end is exclusive, so exactly end must not be found.
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
        table.insert(make_vma(start, end)).unwrap();

        let removed = table.remove(VirtualAddr::new(start));
        assert!(removed.is_some(), "remove by start address should return the VMA");
        assert_eq!(removed.unwrap().start.as_usize(), start);

        // Subsequent find must return None.
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
    fn vma_table_full() {
        let mut table = VmaTable::new();
        // Fill all MAX_VMAS slots.
        for i in 0..MAX_VMAS {
            let start = (i + 1) * PAGE_SIZE;
            let end = start + PAGE_SIZE;
            table.insert(make_vma(start, end)).expect("insert should succeed up to MAX_VMAS");
        }
        // The (MAX_VMAS + 1)-th insert must fail.
        let start = (MAX_VMAS + 1) * PAGE_SIZE;
        let result = table.insert(make_vma(start, start + PAGE_SIZE));
        assert!(
            matches!(result, Err(VmaError::TableFull)),
            "inserting beyond MAX_VMAS must return TableFull"
        );
    }

    #[test]
    fn vma_remove_overlapping_single() {
        let mut table = VmaTable::new();
        let start = 0x4000_0000;
        let end = start + PAGE_SIZE;
        table.insert(make_vma(start, end)).unwrap();

        // Query range that fully contains the VMA.
        let removed = table.remove_overlapping(
            VirtualAddr::new(start),
            VirtualAddr::new(end),
        );
        assert!(removed[0].is_some(), "overlapping VMA must be removed");
        assert!(removed[1].is_none(), "only one VMA should be removed");

        // Table must now be empty for that range.
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
        table.insert(make_vma(start, end)).unwrap();

        // Query range that does not overlap the VMA.
        let removed = table.remove_overlapping(
            VirtualAddr::new(end),
            VirtualAddr::new(end + PAGE_SIZE),
        );
        assert!(
            removed[0].is_none(),
            "remove_overlapping with non-overlapping range must return nothing"
        );

        // Original VMA must still be present.
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
        table.insert(make_vma(a_start, a_end)).unwrap();
        table.insert(make_vma(b_start, b_end)).unwrap();

        // Range covers both VMAs.
        let removed = table.remove_overlapping(
            VirtualAddr::new(a_start),
            VirtualAddr::new(b_end),
        );
        let count = removed.iter().filter(|v| v.is_some()).count();
        assert_eq!(count, 2, "both overlapping VMAs must be removed");
    }

    #[test]
    fn vma_remove_overlapping_partial_overlap() {
        let mut table = VmaTable::new();
        // VMA spans [0x7000_0000, 0x7000_0000 + 2*PAGE_SIZE).
        let start = 0x7000_0000;
        let end = start + 2 * PAGE_SIZE;
        table.insert(make_vma(start, end)).unwrap();

        // Query range overlaps only the tail half of the VMA.
        let removed = table.remove_overlapping(
            VirtualAddr::new(start + PAGE_SIZE),
            VirtualAddr::new(end + PAGE_SIZE),
        );
        assert!(
            removed[0].is_some(),
            "partially overlapping VMA must be removed"
        );
        // The VMA should no longer be in the table.
        assert!(
            table.find(VirtualAddr::new(start)).is_none(),
            "removed VMA must not be findable"
        );
    }
}
