use sumi_abi::arch::layout::{DAX_SLOT_COUNT, PAGE_SIZE};

const BITMAP_U64S: usize = DAX_SLOT_COUNT.div_ceil(64);

#[derive(Debug)]
pub enum DaxError {
    InvalidSlotCount,
    WindowExhausted,
}

/// Bitmap-based allocator for 2MB slots in the DAX window.
/// Bit = 1 means allocated, bit = 0 means free.
/// All slots start free (bitmap zeroed).
pub struct DaxAllocator {
    bitmap: [u64; BITMAP_U64S],
}

#[allow(clippy::new_without_default)] // const fn new() for static init
impl DaxAllocator {
    pub const fn new() -> Self {
        Self {
            bitmap: [0u64; BITMAP_U64S],
        }
    }

    /// Allocate `count` contiguous slots. Returns the byte offset from DAX_WINDOW_BASE.
    pub fn alloc(&mut self, count: usize) -> Result<usize, DaxError> {
        if count == 0 {
            return Err(DaxError::InvalidSlotCount);
        }

        let mut run_start = 0usize;
        let mut run_len = 0usize;

        for slot in 0..DAX_SLOT_COUNT {
            if self.is_allocated(slot) {
                run_len = 0;
            } else {
                if run_len == 0 {
                    run_start = slot;
                }
                run_len += 1;
                if run_len == count {
                    self.mark(run_start, count, true);
                    return Ok(run_start * PAGE_SIZE);
                }
            }
        }

        Err(DaxError::WindowExhausted)
    }

    /// Free `count` contiguous slots starting at byte `offset` from DAX_WINDOW_BASE.
    /// Out-of-bounds offsets are silently ignored.
    pub fn free(&mut self, offset: usize, count: usize) {
        let start_slot = offset / PAGE_SIZE;
        if start_slot
            .checked_add(count)
            .is_none_or(|sum| sum > DAX_SLOT_COUNT)
        {
            return;
        }
        self.mark(start_slot, count, false);
    }

    fn is_allocated(&self, slot: usize) -> bool {
        let word = slot / 64;
        let bit = slot % 64;
        (self.bitmap[word] & (1u64 << bit)) != 0
    }

    fn mark(&mut self, start_slot: usize, count: usize, allocated: bool) {
        for slot in start_slot..start_slot + count {
            let word = slot / 64;
            let bit = slot % 64;
            if allocated {
                self.bitmap[word] |= 1u64 << bit;
            } else {
                self.bitmap[word] &= !(1u64 << bit);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sumi_abi::arch::layout::{DAX_SLOT_COUNT, PAGE_SIZE};

    #[test]
    fn dax_alloc_single() {
        let mut dax = DaxAllocator::new();
        let offset0 = dax.alloc(1).expect("first alloc should succeed");
        assert_eq!(offset0, 0, "first slot should be at offset 0");

        let offset1 = dax.alloc(1).expect("second alloc should succeed");
        assert_eq!(
            offset1, PAGE_SIZE,
            "second slot should be at offset PAGE_SIZE"
        );
    }

    #[test]
    fn dax_alloc_contiguous() {
        let mut dax = DaxAllocator::new();
        let count = 8;
        let offset = dax.alloc(count).expect("contiguous alloc should succeed");
        assert_eq!(offset, 0, "contiguous block should start at offset 0");

        // Next single alloc must start immediately after the block.
        let next = dax.alloc(1).expect("alloc after block should succeed");
        assert_eq!(next, count * PAGE_SIZE, "next slot should follow the block");
    }

    #[test]
    fn dax_alloc_and_free_reuses_offset() {
        let mut dax = DaxAllocator::new();
        let offset = dax.alloc(1).expect("alloc should succeed");
        dax.free(offset, 1);
        let reused = dax.alloc(1).expect("re-alloc after free should succeed");
        assert_eq!(reused, offset, "freed offset must be reused on next alloc");
    }

    #[test]
    fn dax_alloc_zero_returns_error() {
        let mut dax = DaxAllocator::new();
        let result = dax.alloc(0);
        assert!(
            matches!(result, Err(DaxError::InvalidSlotCount)),
            "alloc(0) must return InvalidSlotCount"
        );
    }

    #[test]
    fn dax_alloc_exhaustion() {
        let mut dax = DaxAllocator::new();
        // Fill every slot.
        dax.alloc(DAX_SLOT_COUNT)
            .expect("filling all slots should succeed");
        let result = dax.alloc(1);
        assert!(
            matches!(result, Err(DaxError::WindowExhausted)),
            "alloc when full must return WindowExhausted"
        );
    }

    #[test]
    fn dax_free_and_reuse_middle_block() {
        let mut dax = DaxAllocator::new();
        // Allocate three single slots so slots 0, 1, 2 are occupied.
        let a = dax.alloc(1).unwrap(); // slot 0
        let b = dax.alloc(1).unwrap(); // slot 1
        let c = dax.alloc(1).unwrap(); // slot 2

        // Free the middle slot.
        dax.free(b, 1);

        // A single-slot alloc should reuse slot 1 (the lowest free slot).
        let reused = dax
            .alloc(1)
            .expect("alloc after middle free should succeed");
        assert_eq!(reused, b, "middle slot must be reused");

        // Cleanup to satisfy the borrow checker; freeing a and c is fine.
        dax.free(a, 1);
        dax.free(c, 1);
    }

    #[test]
    fn dax_alloc_too_large_returns_error() {
        let mut dax = DaxAllocator::new();
        let result = dax.alloc(DAX_SLOT_COUNT + 1);
        assert!(
            matches!(result, Err(DaxError::WindowExhausted)),
            "alloc(DAX_SLOT_COUNT + 1) must return WindowExhausted"
        );
    }

    #[test]
    fn dax_free_out_of_bounds_does_not_panic() {
        let mut dax = DaxAllocator::new();
        // An offset that points past the window; the implementation must not panic.
        let out_of_bounds_offset = DAX_SLOT_COUNT * PAGE_SIZE;
        // free() with an out-of-bounds offset should silently return (guarded by
        // the checked_add + bounds check inside free()).
        dax.free(out_of_bounds_offset, 1);
    }

    #[test]
    fn dax_alloc_crosses_u64_word_boundary() {
        let mut dax = DaxAllocator::new();
        // Fill the first 63 slots so the next run starts at slot 63 and spans
        // the u64 word boundary (slot 63 = word 0 bit 63, slot 64 = word 1 bit 0).
        dax.alloc(63).expect("filling 63 slots should succeed");

        // Allocate 2 slots that straddle the word boundary.
        let offset = dax
            .alloc(2)
            .expect("cross-word-boundary alloc should succeed");
        assert_eq!(
            offset,
            63 * PAGE_SIZE,
            "contiguous run should start at slot 63"
        );

        // Verify both slots are marked by trying to alloc the range again after
        // freeing and re-checking via a controlled alloc sequence.
        dax.free(offset, 2);
        let reused = dax.alloc(2).expect("re-alloc should succeed");
        assert_eq!(reused, offset, "freed cross-boundary slots must be reused");
    }
}
