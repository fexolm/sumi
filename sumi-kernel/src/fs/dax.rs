use sumi_abi::arch::layout::{DAX_SLOT_COUNT, PAGE_SIZE};

const BITMAP_U64S: usize = DAX_SLOT_COUNT.div_ceil(64);

#[derive(Debug, PartialEq, Eq)]
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
#[path = "dax_test.rs"]
mod dax_test;
