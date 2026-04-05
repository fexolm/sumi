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
const MAX_ACTIVE_ALLOCS: usize = 4096;

#[repr(C)]
#[derive(Clone, Copy)]
struct FreeBlock {
    size: usize,
    next: usize,
}

#[derive(Clone, Copy)]
struct ActiveAllocation {
    in_use: bool,
    addr: PhysicalAddr,
    size: usize,
}

impl ActiveAllocation {
    const fn empty() -> Self {
        Self {
            in_use: false,
            addr: PhysicalAddr::new(0),
            size: 0,
        }
    }
}

const MIN_FREE_BLOCK_SIZE: usize = size_of::<FreeBlock>();
const MIN_ALIGNMENT: usize = align_of::<FreeBlock>();

struct AllocatorInner {
    free_list_head: usize,
    active: [ActiveAllocation; MAX_ACTIVE_ALLOCS],
}

impl AllocatorInner {
    const fn new() -> Self {
        Self {
            free_list_head: FREE_LIST_END,
            active: [const { ActiveAllocation::empty() }; MAX_ACTIVE_ALLOCS],
        }
    }

    fn reserve_slot(&self) -> Result<usize> {
        self.active
            .iter()
            .position(|slot| !slot.in_use)
            .ok_or(MemoryError::OutOfMemory)
    }

    fn remember(&mut self, slot: usize, addr: PhysicalAddr, size: usize) {
        self.active[slot] = ActiveAllocation {
            in_use: true,
            addr,
            size,
        };
    }

    fn forget(&mut self, addr: PhysicalAddr) -> Result<usize> {
        for slot in &mut self.active {
            if slot.in_use && slot.addr == addr {
                let size = slot.size;
                *slot = ActiveAllocation::empty();
                return Ok(size);
            }
        }

        Err(MemoryError::UnknownAllocation {
            addr: addr.as_usize(),
        })
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

    pub fn free(&self, ptr: PhysicalAddr) -> Result<()> {
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

        let alloc_size = requested_size.max(MIN_FREE_BLOCK_SIZE);
        let align = allocation_alignment(alloc_size).max(min_align);
        let mut inner = self.inner.lock();

        loop {
            if let Some(addr) = self.try_alloc_from_free_list(&mut inner, alloc_size, align)? {
                return Ok(addr);
            }

            self.grow_free_list(&mut inner, alloc_size)?;
        }
    }

    fn free_internal(&self, ptr: PhysicalAddr) -> Result<()> {
        let mut inner = self.inner.lock();
        let size = inner.forget(ptr)?;
        self.insert_free_block(&mut inner, ptr.as_usize(), size);
        Ok(())
    }

    fn try_alloc_from_free_list(
        &self,
        inner: &mut AllocatorInner,
        alloc_size: usize,
        align: usize,
    ) -> Result<Option<PhysicalAddr>> {
        let mut prev = FREE_LIST_END;
        let mut current = inner.free_list_head;

        while current != FREE_LIST_END {
            let block = self.read_free_block(current);
            if let Some(placement) = place_allocation(current, block.size, alloc_size, align) {
                let slot = inner.reserve_slot()?;
                self.consume_free_block(inner, prev, current, block.next, placement);
                let addr = PhysicalAddr::new(placement.alloc_start);
                inner.remember(slot, addr, placement.alloc_size);
                return Ok(Some(addr));
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

fn place_allocation(
    block_start: usize,
    block_size: usize,
    alloc_size: usize,
    align: usize,
) -> Option<Placement> {
    let block_end = block_start.checked_add(block_size)?;
    let mut alloc_start = align_up(block_start, align);
    let mut prefix_size = alloc_start.checked_sub(block_start)?;

    if prefix_size != 0 && prefix_size < MIN_FREE_BLOCK_SIZE {
        alloc_start = alloc_start.checked_add(align)?;
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
mod tests {
    use super::*;
    use std::{collections::HashSet, sync::Arc};
    use crate::memory::test_utils::{TestDirectMap, make_alloc};

    #[test]
    fn small_alloc_and_free_reuses_address() {
        let (_dm, _pa, alloc) = make_alloc(4);
        let a = alloc.alloc(64).unwrap();
        let b = alloc.alloc(64).unwrap();
        assert_ne!(a, b);

        alloc.free(a).unwrap();
        let c = alloc.alloc(64).unwrap();
        assert_eq!(c, a);
    }

    #[test]
    fn small_allocs_do_not_overlap() {
        let (_dm, _pa, alloc) = make_alloc(4);
        let a = alloc.alloc(4096).unwrap();
        let b = alloc.alloc(4096).unwrap();
        assert!(a.as_usize().abs_diff(b.as_usize()) >= 4096);
    }

    #[test]
    fn calloc_zeroes_memory() {
        let (dm, _pa, alloc) = make_alloc(4);

        let ptr = alloc.alloc(128).unwrap();
        unsafe {
            *ptr.to_virtual(&*dm).as_ptr::<u64>() = 0xDEAD_BEEF_CAFE_BABE;
        }
        alloc.free(ptr).unwrap();

        let zeroed = alloc.calloc(128).unwrap();
        let slice =
            unsafe { core::slice::from_raw_parts(zeroed.to_virtual(&*dm).as_ptr::<u8>(), 128) };
        assert!(slice.iter().all(|&byte| byte == 0));
    }

    #[test]
    fn page_table_alloc_is_aligned() {
        let (_dm, _pa, alloc) = make_alloc(4);
        let ptr = alloc.alloc(PAGE_TABLE_SIZE).unwrap();
        assert_eq!(ptr.as_usize() % PAGE_TABLE_SIZE, 0);
    }

    #[test]
    fn adjacent_frees_are_coalesced() {
        let (_dm, _pa, alloc) = make_alloc(4);
        let a = alloc.alloc(PAGE_TABLE_SIZE).unwrap();
        let b = alloc.alloc(PAGE_TABLE_SIZE).unwrap();
        assert_eq!(b.as_usize(), a.as_usize() + PAGE_TABLE_SIZE);

        alloc.free(b).unwrap();
        alloc.free(a).unwrap();

        let big = alloc.alloc(PAGE_TABLE_SIZE * 2).unwrap();
        assert_eq!(big, a);
    }

    #[test]
    fn large_allocs_dont_overlap() {
        let (_dm, _pa, alloc) = make_alloc(8);
        let a = alloc.alloc(1 << 22).unwrap();
        let b = alloc.alloc(1 << 22).unwrap();
        assert!(a.as_usize().abs_diff(b.as_usize()) >= (1 << 22));
    }

    #[test]
    fn large_free_and_realloc_reuses_address() {
        let (_dm, _pa, alloc) = make_alloc(8);
        let a = alloc.alloc(1 << 22).unwrap();
        let b = alloc.alloc(1 << 22).unwrap();
        assert_ne!(a, b);

        alloc.free(b).unwrap();
        let c = alloc.alloc(1 << 22).unwrap();
        assert_eq!(c, b);
    }

    #[test]
    fn zero_size_alloc_succeeds() {
        let (_dm, _pa, alloc) = make_alloc(2);
        let ptr = alloc.alloc(0).unwrap();
        alloc.free(ptr).unwrap();
    }

    #[test]
    fn alloc_too_large_fails() {
        let (_dm, _pa, alloc) = make_alloc(8);
        assert!(matches!(
            alloc.alloc(MAX_ALLOC + 1),
            Err(MemoryError::AllocationTooLarge { .. })
        ));
    }

    #[test]
    fn double_free_is_detected() {
        let (_dm, _pa, alloc) = make_alloc(2);
        let ptr = alloc.alloc(64).unwrap();
        alloc.free(ptr).unwrap();

        let result = alloc.free(ptr);
        assert!(matches!(result, Err(MemoryError::UnknownAllocation { .. })));
    }

    #[test]
    fn cross_thread_free_is_reused() {
        let (_dm, _pa, alloc) = make_alloc(4);
        let alloc: Arc<KernelAllocator<'static, TestDirectMap>> = Arc::from(alloc);

        let ptr = alloc.alloc(64).unwrap();

        let worker_alloc = Arc::clone(&alloc);
        let worker = std::thread::spawn(move || {
            worker_alloc.free(ptr).unwrap();
        });
        worker.join().unwrap();

        let next = alloc.alloc(64).unwrap();
        assert_eq!(next, ptr);
    }

    #[test]
    fn concurrent_live_allocations_are_unique() {
        const THREADS: usize = 32;
        const OPS: usize = 16;
        const SIZE: usize = 64;

        let (_dm, _pa, alloc) = make_alloc(4);
        let alloc: Arc<KernelAllocator<'static, TestDirectMap>> = Arc::from(alloc);

        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let alloc = Arc::clone(&alloc);
                std::thread::spawn(move || {
                    (0..OPS)
                        .map(|_| alloc.alloc(SIZE).unwrap().as_usize())
                        .collect::<Vec<_>>()
                })
            })
            .collect();

        let allocations: Vec<Vec<usize>> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        let mut seen = HashSet::new();
        for ptrs in &allocations {
            for &addr in ptrs {
                assert!(seen.insert(addr), "duplicate live allocation at {addr:#x}");
            }
        }

        for ptrs in allocations {
            for addr in ptrs {
                alloc.free(PhysicalAddr::new(addr)).unwrap();
            }
        }
    }
}
