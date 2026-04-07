use crate::memory::errors::{MemoryError, Result};
use sumi_abi::{
    address::PhysicalAddr,
    arch::layout::{KERNEL_STACK, PAGE_SIZE},
    layout::MAX_GUEST_MEMORY,
};

const PALLOC_FIRST_PAGE: PhysicalAddr = KERNEL_STACK;

const PAGE_COUNT: usize = MAX_GUEST_MEMORY.div_ceil(PAGE_SIZE);
const BITMAP_SIZE: usize = PAGE_COUNT.div_ceil(64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub used_pages: usize,
    pub used_bytes: usize,
    pub peak_memory_usage: usize,
    pub allocatable_limit_pages: usize,
    pub allocatable_limit_bytes: usize,
}

#[repr(align(4096))]
#[repr(C)]
struct PageAllocatorImpl {
    bitmap: [u64; BITMAP_SIZE],
    peak_memory_usage: usize,
}

impl PageAllocatorImpl {
    const fn reserved_pages() -> usize {
        PALLOC_FIRST_PAGE.as_usize() / PAGE_SIZE
    }

    const fn new() -> Self {
        let mut bitmap = [0; BITMAP_SIZE];
        let mut page = 0;
        let reserved_pages = Self::reserved_pages();

        while page < reserved_pages {
            let word = page / 64;
            let bit = page % 64;
            bitmap[word] |= 1 << bit;
            page += 1;
        }

        Self {
            bitmap,
            peak_memory_usage: 0,
        }
    }

    fn alloc(&mut self, pages: usize) -> Result<PhysicalAddr> {
        if pages == 0 {
            return Err(MemoryError::InvalidPageCount { pages });
        }

        let search_limit = PAGE_COUNT;
        if pages > search_limit {
            return Err(MemoryError::OutOfMemory);
        }

        let mut run_start = 0;
        let mut run_len = 0;

        for page in 0..search_limit {
            if self.is_page_used(page) {
                run_len = 0;
                continue;
            }

            if run_len == 0 {
                run_start = page;
            }

            run_len += 1;
            if run_len == pages {
                self.mark_pages(run_start, pages, true);
                let reserved_pages = Self::reserved_pages();
                let footprint_pages = (run_start + pages).saturating_sub(reserved_pages);
                self.peak_memory_usage = self.peak_memory_usage.max(footprint_pages * PAGE_SIZE);
                return Ok(PhysicalAddr::new(run_start * PAGE_SIZE));
            }
        }

        Err(MemoryError::OutOfMemory)
    }

    fn free(&mut self, addr: PhysicalAddr) -> Result<()> {
        let page_index = addr.as_usize() / PAGE_SIZE;
        self.mark_pages(page_index, 1, false);
        Ok(())
    }

    fn is_page_used(&self, page_index: usize) -> bool {
        let word_index = page_index / 64;
        let bit_index = page_index % 64;
        (self.bitmap[word_index] & (1 << bit_index)) != 0
    }

    fn mark_pages(&mut self, start_page: usize, pages: usize, used: bool) {
        for page in start_page..start_page + pages {
            let word = page / 64;
            let bit = page % 64;
            if used {
                self.bitmap[word] |= 1 << bit;
            } else {
                self.bitmap[word] &= !(1 << bit);
            }
        }
    }

    fn used_pages(&self) -> usize {
        let mut used = 0usize;
        for &word in &self.bitmap {
            used += word.count_ones() as usize;
        }

        used.saturating_sub(Self::reserved_pages())
    }

    fn stats(&self) -> Stats {
        let used_pages = self.used_pages();
        let alloc_limit_pages = PAGE_COUNT.saturating_sub(Self::reserved_pages());
        Stats {
            used_pages,
            used_bytes: used_pages * PAGE_SIZE,
            peak_memory_usage: self.peak_memory_usage,
            allocatable_limit_pages: alloc_limit_pages,
            allocatable_limit_bytes: alloc_limit_pages * PAGE_SIZE,
        }
    }
}

pub struct PageAllocator(spin::Mutex<PageAllocatorImpl>);

#[allow(clippy::new_without_default)] // const fn for static init
impl PageAllocator {
    pub const fn new() -> Self {
        Self(spin::Mutex::new(PageAllocatorImpl::new()))
    }

    pub fn alloc(&self, pages: usize) -> Result<PhysicalAddr> {
        self.0.lock().alloc(pages)
    }

    pub fn free(&self, addr: PhysicalAddr) -> Result<()> {
        self.0.lock().free(addr)
    }

    pub fn get_stats(&self) -> Stats {
        self.0.lock().stats()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::errors::MemoryError;

    #[test]
    fn basic_alloc_and_free() {
        let allocator = Box::new(PageAllocator::new());
        let first_page = PALLOC_FIRST_PAGE.as_usize();
        let addr1 = allocator.alloc(1).unwrap();
        let addr2 = allocator.alloc(1).unwrap();
        assert_eq!(addr1, PhysicalAddr::new(first_page));
        assert_eq!(addr2, PhysicalAddr::new(first_page + PAGE_SIZE));
        allocator.free(addr1).unwrap();
        let addr3 = allocator.alloc(1).unwrap();
        assert_eq!(addr3, PhysicalAddr::new(first_page));
    }

    #[test]
    fn multi_page_contiguous_alloc() {
        let allocator = Box::new(PageAllocator::new());
        let first_page = PALLOC_FIRST_PAGE.as_usize();
        let addr = allocator.alloc(4).unwrap();
        assert_eq!(addr, PhysicalAddr::new(first_page));
        // Next single alloc should start after the 4-page block
        let addr2 = allocator.alloc(1).unwrap();
        assert_eq!(addr2, PhysicalAddr::new(first_page + 4 * PAGE_SIZE));
    }

    #[test]
    fn alloc_zero_pages_returns_error() {
        let allocator = Box::new(PageAllocator::new());
        let result = allocator.alloc(0);
        assert!(matches!(
            result,
            Err(MemoryError::InvalidPageCount { pages: 0 })
        ));
    }

    #[test]
    fn stats_track_usage() {
        let allocator = Box::new(PageAllocator::new());
        let before = allocator.get_stats();
        assert_eq!(before.used_pages, 0);

        let addr = allocator.alloc(2).unwrap();
        let after_alloc = allocator.get_stats();
        assert_eq!(after_alloc.used_pages, 2);
        assert_eq!(after_alloc.used_bytes, 2 * PAGE_SIZE);

        allocator.free(addr).unwrap();
        // free() only frees 1 page at a time
        let after_free = allocator.get_stats();
        assert_eq!(after_free.used_pages, 1);
    }

    #[test]
    fn free_and_reuse() {
        let allocator = Box::new(PageAllocator::new());
        let first_page = PALLOC_FIRST_PAGE.as_usize();
        let a = allocator.alloc(1).unwrap();
        let b = allocator.alloc(1).unwrap();
        allocator.free(a).unwrap();
        allocator.free(b).unwrap();
        // After freeing both, re-alloc should reuse the first page
        let c = allocator.alloc(1).unwrap();
        assert_eq!(c, PhysicalAddr::new(first_page));
    }
}
