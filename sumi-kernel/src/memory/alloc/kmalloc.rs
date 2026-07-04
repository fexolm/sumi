use core::{
    mem::{align_of, size_of},
    ptr::write_bytes,
};

use crate::memory::{
    alloc::palloc::PageAllocator,
    errors::{MemoryError, Result},
};
use sumi_abi::{
    address::{DirectMap, PhysicalAddr},
    arch::layout::{PAGE_SIZE, PAGE_TABLE_SIZE},
};

const MAX_ALLOC: usize = 1 << 24;
const FREE_LIST_END: usize = 0;

#[repr(C)]
#[derive(Clone, Copy)]
struct FreeBlock {
    size: usize,
    next: usize,
}

const MIN_FREE_BLOCK_SIZE: usize = size_of::<FreeBlock>();
const MIN_ALIGNMENT: usize = align_of::<FreeBlock>();

/// Size of the header prepended to every allocation.
/// Equals MIN_FREE_BLOCK_SIZE so that the header never creates an
/// unusable prefix fragment when carving from an aligned free block.
const HEADER_SIZE: usize = MIN_FREE_BLOCK_SIZE;

struct AllocatorInner {
    free_list_head: usize,
}

impl AllocatorInner {
    const fn new() -> Self {
        Self {
            free_list_head: FREE_LIST_END,
        }
    }
}

#[derive(Clone, Copy)]
struct Placement {
    alloc_start: usize,
    alloc_size: usize,
    prefix_size: usize,
    suffix_start: usize,
    suffix_size: usize,
}

pub struct KernelAllocator<'i, DM: DirectMap> {
    inner: spin::Mutex<AllocatorInner>,
    palloc: &'i PageAllocator,
    dm: &'i DM,
}

unsafe impl<'i, DM: DirectMap + Sync> Sync for KernelAllocator<'i, DM> {}

impl<'i, DM: DirectMap> KernelAllocator<'i, DM> {
    pub const fn new(dm: &'i DM, palloc: &'i PageAllocator) -> Self {
        Self {
            inner: spin::Mutex::new(AllocatorInner::new()),
            palloc,
            dm,
        }
    }

    pub fn alloc(&self, size: usize) -> Result<PhysicalAddr> {
        self.alloc_internal(size, 0)
    }

    /// Allocate with an explicit minimum alignment (must be a power of two).
    pub fn alloc_aligned(&self, size: usize, min_align: usize) -> Result<PhysicalAddr> {
        self.alloc_internal(size, min_align)
    }

    pub fn free(&self, ptr: PhysicalAddr) {
        self.free_internal(ptr)
    }

    pub fn calloc(&self, size: usize) -> Result<PhysicalAddr> {
        let addr = self.alloc(size)?;
        unsafe {
            write_bytes(addr.to_virtual(self.dm).as_ptr::<u8>(), 0, size);
        }
        Ok(addr)
    }

    pub fn direct_map(&self) -> &'i DM {
        self.dm
    }

    fn alloc_internal(&self, requested_size: usize, min_align: usize) -> Result<PhysicalAddr> {
        let requested_size = requested_size.max(1);
        if requested_size > MAX_ALLOC {
            return Err(MemoryError::AllocationTooLarge {
                requested: requested_size,
                max: MAX_ALLOC,
            });
        }

        let total_size =
            align_up(requested_size + HEADER_SIZE, MIN_ALIGNMENT).max(MIN_FREE_BLOCK_SIZE);
        let user_align = allocation_alignment(requested_size).max(min_align);
        let mut inner = self.inner.lock();

        loop {
            if let Some(addr) = self.try_alloc_from_free_list(&mut inner, total_size, user_align)? {
                return Ok(addr);
            }

            self.grow_free_list(&mut inner, total_size)?;
        }
    }

    fn free_internal(&self, user_ptr: PhysicalAddr) {
        let block_start = user_ptr.as_usize() - HEADER_SIZE;
        let alloc_size = self.read_header(block_start);
        let mut inner = self.inner.lock();
        self.insert_free_block(&mut inner, block_start, alloc_size);
    }

    fn try_alloc_from_free_list(
        &self,
        inner: &mut AllocatorInner,
        alloc_size: usize,
        user_align: usize,
    ) -> Result<Option<PhysicalAddr>> {
        let mut prev = FREE_LIST_END;
        let mut current = inner.free_list_head;

        while current != FREE_LIST_END {
            let block = self.read_free_block(current);
            if let Some(placement) = place_allocation(current, block.size, alloc_size, user_align) {
                self.consume_free_block(inner, prev, current, block.next, placement);
                self.write_header(placement.alloc_start, placement.alloc_size);
                let user_ptr = PhysicalAddr::new(placement.alloc_start + HEADER_SIZE);
                return Ok(Some(user_ptr));
            }

            prev = current;
            current = block.next;
        }

        Ok(None)
    }

    fn consume_free_block(
        &self,
        inner: &mut AllocatorInner,
        prev: usize,
        current: usize,
        next: usize,
        placement: Placement,
    ) {
        match (placement.prefix_size > 0, placement.suffix_size > 0) {
            (true, true) => {
                self.write_free_block(current, placement.prefix_size, placement.suffix_start);
                self.write_free_block(placement.suffix_start, placement.suffix_size, next);
                if prev == FREE_LIST_END {
                    inner.free_list_head = current;
                } else {
                    self.free_block_mut(prev).next = current;
                }
            }
            (true, false) => {
                self.write_free_block(current, placement.prefix_size, next);
                if prev == FREE_LIST_END {
                    inner.free_list_head = current;
                } else {
                    self.free_block_mut(prev).next = current;
                }
            }
            (false, true) => {
                self.write_free_block(placement.suffix_start, placement.suffix_size, next);
                if prev == FREE_LIST_END {
                    inner.free_list_head = placement.suffix_start;
                } else {
                    self.free_block_mut(prev).next = placement.suffix_start;
                }
            }
            (false, false) => {
                if prev == FREE_LIST_END {
                    inner.free_list_head = next;
                } else {
                    self.free_block_mut(prev).next = next;
                }
            }
        }
    }

    fn grow_free_list(&self, inner: &mut AllocatorInner, alloc_size: usize) -> Result<()> {
        let pages = alloc_size.div_ceil(PAGE_SIZE).max(1);
        let base = self.palloc.alloc(pages)?;
        self.insert_free_block(inner, base.as_usize(), pages * PAGE_SIZE);
        Ok(())
    }

    fn insert_free_block(&self, inner: &mut AllocatorInner, start: usize, size: usize) {
        debug_assert!(size >= MIN_FREE_BLOCK_SIZE);

        let mut prev = FREE_LIST_END;
        let mut current = inner.free_list_head;

        while current != FREE_LIST_END && current < start {
            prev = current;
            current = self.read_free_block(current).next;
        }

        let merged_start = if prev != FREE_LIST_END {
            let prev_block = self.read_free_block(prev);
            if prev + prev_block.size == start {
                let prev_block = self.free_block_mut(prev);
                prev_block.size += size;
                prev
            } else {
                self.write_free_block(start, size, current);
                self.free_block_mut(prev).next = start;
                start
            }
        } else {
            self.write_free_block(start, size, current);
            inner.free_list_head = start;
            start
        };

        if current != FREE_LIST_END {
            let merged_block = self.read_free_block(merged_start);
            if merged_start + merged_block.size == current {
                let next_block = self.read_free_block(current);
                let merged_block = self.free_block_mut(merged_start);
                merged_block.size += next_block.size;
                merged_block.next = next_block.next;
            }
        }
    }

    fn write_header(&self, alloc_start: usize, alloc_size: usize) {
        unsafe {
            *PhysicalAddr::new(alloc_start)
                .to_virtual(self.dm)
                .as_ptr::<usize>() = alloc_size;
        }
    }

    fn read_header(&self, alloc_start: usize) -> usize {
        unsafe {
            *PhysicalAddr::new(alloc_start)
                .to_virtual(self.dm)
                .as_ptr::<usize>()
        }
    }

    fn read_free_block(&self, addr: usize) -> FreeBlock {
        *self.free_block(addr)
    }

    fn write_free_block(&self, addr: usize, size: usize, next: usize) {
        unsafe {
            *PhysicalAddr::new(addr)
                .to_virtual(self.dm)
                .as_ptr::<FreeBlock>() = FreeBlock { size, next };
        }
    }

    fn free_block(&self, addr: usize) -> &FreeBlock {
        unsafe {
            PhysicalAddr::new(addr)
                .to_virtual(self.dm)
                .as_ref_mut::<FreeBlock>()
        }
    }

    #[allow(clippy::mut_from_ref)] // Mutation goes through direct-map raw pointers, not &self
    fn free_block_mut(&self, addr: usize) -> &mut FreeBlock {
        unsafe {
            PhysicalAddr::new(addr)
                .to_virtual(self.dm)
                .as_ref_mut::<FreeBlock>()
        }
    }
}

fn allocation_alignment(size: usize) -> usize {
    size.next_power_of_two()
        .clamp(MIN_ALIGNMENT, PAGE_TABLE_SIZE)
}

fn align_up(addr: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (addr + align - 1) & !(align - 1)
}

/// Find a placement within `[block_start, block_start + block_size)` for an
/// allocation of `alloc_size` bytes (including the HEADER_SIZE prefix) such
/// that the *user pointer* (`alloc_start + HEADER_SIZE`) is aligned to
/// `user_align`.
fn place_allocation(
    block_start: usize,
    block_size: usize,
    alloc_size: usize,
    user_align: usize,
) -> Option<Placement> {
    let block_end = block_start.checked_add(block_size)?;

    // The user pointer (alloc_start + HEADER_SIZE) must be aligned to user_align.
    let user_ptr = align_up(block_start + HEADER_SIZE, user_align);
    let mut alloc_start = user_ptr - HEADER_SIZE;
    let mut prefix_size = alloc_start.checked_sub(block_start)?;

    if prefix_size != 0 && prefix_size < MIN_FREE_BLOCK_SIZE {
        let user_ptr = user_ptr.checked_add(user_align)?;
        alloc_start = user_ptr - HEADER_SIZE;
        prefix_size = alloc_start.checked_sub(block_start)?;
    }

    let alloc_end = alloc_start.checked_add(alloc_size)?;
    if alloc_end > block_end {
        return None;
    }

    let mut final_alloc_size = alloc_size;
    let mut suffix_start = alloc_end;
    let mut suffix_size = block_end.checked_sub(alloc_end)?;

    if suffix_size != 0 && suffix_size < MIN_FREE_BLOCK_SIZE {
        final_alloc_size = final_alloc_size.checked_add(suffix_size)?;
        suffix_start = block_end;
        suffix_size = 0;
    }

    Some(Placement {
        alloc_start,
        alloc_size: final_alloc_size,
        prefix_size,
        suffix_start,
        suffix_size,
    })
}

#[cfg(test)]
#[path = "kmalloc_test.rs"]
mod kmalloc_test;
