use core::ptr::write_bytes;

use crate::memory::{
    alloc::palloc::PageAllocator,
    errors::{MemoryError, Result},
};
use sumi_abi::{
    address::{DirectMap, PhysicalAddr},
    arch::layout::PAGE_SIZE,
};

const MIN_SHIFT: u32 = 10;
const MAX_SHIFT: u32 = 24;
const MIN_ALLOC_SIZE: usize = 1 << MIN_SHIFT;
const MAX_ALLOC_SIZE: usize = 1 << MAX_SHIFT;
const SMALL_CLASS_COUNT: usize = 12;
const MAX_SLABS_PER_CLASS: usize = 512;
const MAX_LARGE_ALLOCS: usize = 256;
const FREE_LIST_END: u16 = u16::MAX;
const PAGE_MASK: usize = !(PAGE_SIZE - 1);
const SMALL_SLAB_MAP_SIZE: usize = 4096;

const SMALL_CLASS_SIZES: [u32; SMALL_CLASS_COUNT] = [
    1 << 10,
    1 << 11,
    1 << 12,
    1 << 13,
    1 << 14,
    1 << 15,
    1 << 16,
    1 << 17,
    1 << 18,
    1 << 19,
    1 << 20,
    1 << 21,
];

#[derive(Clone, Copy)]
struct Slab {
    in_use: bool,
    base: PhysicalAddr,
    capacity: u16,
    free_count: u16,
    free_head: u16,
}

impl Slab {
    const fn empty() -> Self {
        Self {
            in_use: false,
            base: PhysicalAddr::new(0),
            capacity: 0,
            free_count: 0,
            free_head: FREE_LIST_END,
        }
    }
}

#[derive(Clone, Copy)]
struct SizeClass {
    block_size: u32,
    slabs: [Slab; MAX_SLABS_PER_CLASS],
    last_alloc_slab: usize,
}

impl SizeClass {
    const fn new(block_size: u32) -> Self {
        Self {
            block_size,
            slabs: [Slab::empty(); MAX_SLABS_PER_CLASS],
            last_alloc_slab: 0,
        }
    }
}

#[derive(Clone, Copy)]
struct LargeAlloc {
    in_use: bool,
    base: PhysicalAddr,
    pages: usize,
}

impl LargeAlloc {
    const fn empty() -> Self {
        Self {
            in_use: false,
            base: PhysicalAddr::new(0),
            pages: 0,
        }
    }
}

#[derive(Clone, Copy)]
struct SmallSlabMapEntry {
    key_page_plus_one: u32,
    value: u16,
    _reserved: u16,
}

impl SmallSlabMapEntry {
    const fn empty() -> Self {
        Self {
            key_page_plus_one: 0,
            value: 0,
            _reserved: 0,
        }
    }
}

struct KernelAllocatorImpl<'i, DM: DirectMap> {
    small: [SizeClass; SMALL_CLASS_COUNT],
    small_slab_map: [SmallSlabMapEntry; SMALL_SLAB_MAP_SIZE],
    large: [LargeAlloc; MAX_LARGE_ALLOCS],
    palloc: &'i PageAllocator,
    dm: &'i DM,
}

impl<'i, DM: DirectMap> KernelAllocatorImpl<'i, DM> {
    const fn new(dm: &'i DM, page_alloc: &'i PageAllocator) -> Self {
        Self {
            small: build_small_classes(),
            small_slab_map: [SmallSlabMapEntry::empty(); SMALL_SLAB_MAP_SIZE],
            large: [LargeAlloc::empty(); MAX_LARGE_ALLOCS],
            palloc: page_alloc,
            dm,
        }
    }

    fn alloc(&mut self, size: usize) -> Result<PhysicalAddr> {
        let class_size = size_to_class(size)?;

        if class_size <= PAGE_SIZE {
            self.alloc_small(class_size as u32)
        } else {
            self.alloc_large(class_size)
        }
    }

    fn calloc(&mut self, size: usize) -> Result<PhysicalAddr> {
        let addr = self.alloc(size)?;

        unsafe {
            write_bytes(addr.to_virtual(self.dm).as_ptr::<u8>(), 0, size);
        }

        Ok(addr)
    }

    fn free(&mut self, ptr: PhysicalAddr) -> Result<()> {
        if self.free_small(ptr)? {
            return Ok(());
        }

        self.free_large(ptr)
    }

    fn alloc_small(&mut self, block_size: u32) -> Result<PhysicalAddr> {
        let class_idx = (block_size.trailing_zeros() - MIN_SHIFT) as usize;
        let start_idx = self.small[class_idx].last_alloc_slab;

        for offset in 0..MAX_SLABS_PER_CLASS {
            let slab_idx = (start_idx + offset) % MAX_SLABS_PER_CLASS;
            let slab = &mut self.small[class_idx].slabs[slab_idx];
            if slab.in_use && slab.free_count > 0 {
                self.small[class_idx].last_alloc_slab = slab_idx;
                let block = self.small[class_idx].block_size as usize;
                return alloc_from_small_slab(slab, block, self.dm);
            }
        }

        for slab_idx in 0..MAX_SLABS_PER_CLASS {
            if !self.small[class_idx].slabs[slab_idx].in_use {
                let base = {
                    let slab = &mut self.small[class_idx].slabs[slab_idx];
                    let block = self.small[class_idx].block_size;
                    init_small_slab(self.palloc, slab, block, self.dm)?;
                    slab.base.as_usize()
                };
                self.small_slab_map_insert(base, class_idx, slab_idx)?;
                self.small[class_idx].last_alloc_slab = slab_idx;
                let slab = &mut self.small[class_idx].slabs[slab_idx];
                let block = self.small[class_idx].block_size as usize;
                return alloc_from_small_slab(slab, block, self.dm);
            }
        }

        Err(MemoryError::TooManySlabs {
            class_size: self.small[class_idx].block_size,
        })
    }

    fn free_small(&mut self, addr: PhysicalAddr) -> Result<bool> {
        let p = addr.as_usize();
        let page_base = p & PAGE_MASK;
        let Some((class_idx, slab_idx)) = self.small_slab_map_get(page_base) else {
            return Ok(false);
        };

        let slab = &mut self.small[class_idx].slabs[slab_idx];
        let offset = p - slab.base.as_usize();
        let block_size = self.small[class_idx].block_size as usize;

        if offset % block_size != 0 {
            return Err(MemoryError::SlabAlignmentMismatch {
                addr: p,
                block_size,
            });
        }

        let idx = (offset / block_size) as u16;
        unsafe {
            *small_slab_link_ptr(slab, idx, self.dm) = slab.free_head;
        }
        slab.free_head = idx;
        slab.free_count += 1;

        if slab.free_count == slab.capacity {
            let base = slab.base;
            *slab = Slab::empty();
            self.small_slab_map_remove(page_base);
            self.palloc.free(base)?;
        }

        Ok(true)
    }

    fn alloc_large(&mut self, class_size: usize) -> Result<PhysicalAddr> {
        let pages = class_size.div_ceil(PAGE_SIZE);
        let base = self.palloc.alloc(pages)?;

        for slot in &mut self.large {
            if !slot.in_use {
                *slot = LargeAlloc {
                    in_use: true,
                    base,
                    pages,
                };
                return Ok(base);
            }
        }

        for page in 0..pages {
            self.palloc.free(base.add(page * PAGE_SIZE))?;
        }
        Err(MemoryError::TooManyLargeAllocations)
    }

    fn free_large(&mut self, addr: PhysicalAddr) -> Result<()> {
        for slot in &mut self.large {
            if slot.in_use && slot.base == addr {
                for page in 0..slot.pages {
                    self.palloc.free(slot.base.add(page * PAGE_SIZE))?;
                }
                *slot = LargeAlloc::empty();
                return Ok(());
            }
        }

        Err(MemoryError::UnknownAllocation {
            addr: addr.as_usize(),
        })
    }

    fn small_slab_map_insert(
        &mut self,
        page_base: usize,
        class_idx: usize,
        slab_idx: usize,
    ) -> Result<()> {
        let value = (class_idx * MAX_SLABS_PER_CLASS + slab_idx + 1) as u16;
        for probe in 0..SMALL_SLAB_MAP_SIZE {
            let idx = (hash_page_base(page_base) + probe) & (SMALL_SLAB_MAP_SIZE - 1);
            let entry = self.small_slab_map[idx];
            if entry.value == 0 || entry.key_page_plus_one == to_page_plus_one(page_base) {
                self.small_slab_map[idx] = SmallSlabMapEntry {
                    key_page_plus_one: to_page_plus_one(page_base),
                    value,
                    _reserved: 0,
                };
                return Ok(());
            }
        }

        Err(MemoryError::TooManySlabs {
            class_size: self.small[class_idx].block_size,
        })
    }

    fn small_slab_map_get(&self, page_base: usize) -> Option<(usize, usize)> {
        for probe in 0..SMALL_SLAB_MAP_SIZE {
            let idx = (hash_page_base(page_base) + probe) & (SMALL_SLAB_MAP_SIZE - 1);
            let entry = self.small_slab_map[idx];
            if entry.value == 0 {
                return None;
            }
            if entry.key_page_plus_one == to_page_plus_one(page_base) {
                let unpacked = entry.value as usize - 1;
                return Some((
                    unpacked / MAX_SLABS_PER_CLASS,
                    unpacked % MAX_SLABS_PER_CLASS,
                ));
            }
        }

        None
    }

    fn small_slab_map_remove(&mut self, page_base: usize) {
        let mut removed_idx = None;
        for probe in 0..SMALL_SLAB_MAP_SIZE {
            let idx = (hash_page_base(page_base) + probe) & (SMALL_SLAB_MAP_SIZE - 1);
            let entry = self.small_slab_map[idx];
            if entry.value == 0 {
                return;
            }
            if entry.key_page_plus_one == to_page_plus_one(page_base) {
                removed_idx = Some(idx);
                break;
            }
        }

        let Some(remove_idx) = removed_idx else {
            return;
        };

        self.small_slab_map[remove_idx] = SmallSlabMapEntry::empty();
        let mut scan = (remove_idx + 1) & (SMALL_SLAB_MAP_SIZE - 1);
        for _ in 0..SMALL_SLAB_MAP_SIZE {
            let entry = self.small_slab_map[scan];
            if entry.value == 0 {
                return;
            }
            self.small_slab_map[scan] = SmallSlabMapEntry::empty();

            for probe in 0..SMALL_SLAB_MAP_SIZE {
                let idx = (hash_page_base(from_page_plus_one(entry.key_page_plus_one)) + probe)
                    & (SMALL_SLAB_MAP_SIZE - 1);
                if self.small_slab_map[idx].value == 0 {
                    self.small_slab_map[idx] = entry;
                    break;
                }
            }

            scan = (scan + 1) & (SMALL_SLAB_MAP_SIZE - 1);
        }
    }
}

fn init_small_slab(
    palloc: &PageAllocator,
    slab: &mut Slab,
    block_size: u32,
    dm: &impl DirectMap,
) -> Result<()> {
    let base = palloc.alloc(1)?;
    let capacity = (PAGE_SIZE / block_size as usize) as u16;
    if capacity == 0 {
        return Err(MemoryError::InvalidSlabCapacity);
    }

    *slab = Slab {
        in_use: true,
        base,
        capacity,
        free_count: capacity,
        free_head: 0,
    };

    for i in 0..capacity {
        let next = if i + 1 < capacity {
            i + 1
        } else {
            FREE_LIST_END
        };
        unsafe {
            *small_slab_link_ptr(slab, i, dm) = next;
        }
    }

    Ok(())
}

const fn build_small_classes() -> [SizeClass; SMALL_CLASS_COUNT] {
    [
        SizeClass::new(SMALL_CLASS_SIZES[0]),
        SizeClass::new(SMALL_CLASS_SIZES[1]),
        SizeClass::new(SMALL_CLASS_SIZES[2]),
        SizeClass::new(SMALL_CLASS_SIZES[3]),
        SizeClass::new(SMALL_CLASS_SIZES[4]),
        SizeClass::new(SMALL_CLASS_SIZES[5]),
        SizeClass::new(SMALL_CLASS_SIZES[6]),
        SizeClass::new(SMALL_CLASS_SIZES[7]),
        SizeClass::new(SMALL_CLASS_SIZES[8]),
        SizeClass::new(SMALL_CLASS_SIZES[9]),
        SizeClass::new(SMALL_CLASS_SIZES[10]),
        SizeClass::new(SMALL_CLASS_SIZES[11]),
    ]
}

fn size_to_class(size: usize) -> Result<usize> {
    let requested = if size == 0 { MIN_ALLOC_SIZE } else { size };
    if requested > MAX_ALLOC_SIZE {
        return Err(MemoryError::AllocationTooLarge {
            requested,
            max: MAX_ALLOC_SIZE,
        });
    }

    Ok(requested.next_power_of_two().max(MIN_ALLOC_SIZE))
}

fn alloc_from_small_slab(
    slab: &mut Slab,
    block_size: usize,
    dm: &impl DirectMap,
) -> Result<PhysicalAddr> {
    let idx = slab.free_head;
    if idx == FREE_LIST_END {
        return Err(MemoryError::SlabEmpty);
    }

    let next = unsafe { *small_slab_link_ptr(slab, idx, dm) };
    slab.free_head = next;
    slab.free_count -= 1;

    let offset = idx as usize * block_size;
    Ok(slab.base.add(offset))
}

unsafe fn small_slab_link_ptr(slab: &Slab, idx: u16, map: &impl DirectMap) -> *mut u16 {
    let addr = slab.base.as_usize() + idx as usize * (PAGE_SIZE / slab.capacity as usize);
    PhysicalAddr::new(addr).to_virtual(map).as_ptr::<u16>()
}

#[inline(always)]
const fn hash_page_base(page_base: usize) -> usize {
    ((page_base >> 21).wrapping_mul(0x9E37_79B9_7F4A_7C15usize)) >> 2
}

#[inline(always)]
const fn to_page_plus_one(page_base: usize) -> u32 {
    (page_base / PAGE_SIZE + 1) as u32
}

#[inline(always)]
const fn from_page_plus_one(page_plus_one: u32) -> usize {
    (page_plus_one as usize - 1) * PAGE_SIZE
}

pub struct KernelAllocator<'i, DM: DirectMap>(spin::Mutex<KernelAllocatorImpl<'i, DM>>);

impl<'i, DM: DirectMap> KernelAllocator<'i, DM> {
    pub const fn new(dm: &'i DM, palloc: &'i PageAllocator) -> Self {
        Self(spin::Mutex::new(KernelAllocatorImpl::new(dm, palloc)))
    }

    pub fn alloc(&self, size: usize) -> Result<PhysicalAddr> {
        self.0.lock().alloc(size)
    }

    pub fn free(&self, ptr: PhysicalAddr, _size: usize) -> Result<()> {
        self.0.lock().free(ptr)
    }

    pub fn calloc(&self, size: usize) -> Result<PhysicalAddr> {
        self.0.lock().calloc(size)
    }

    pub fn direct_map(&self) -> &'i DM {
        self.0.lock().dm
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arch::KernelDirectMap;

    #[test]
    fn class_rounding_works() {
        assert_eq!(size_to_class(0).unwrap(), 1024);
        assert_eq!(size_to_class(1024).unwrap(), 1024);
        assert_eq!(size_to_class(1025).unwrap(), 2048);
        assert_eq!(size_to_class((1 << 22) + 1).unwrap(), 1 << 23);
    }

    #[test]
    fn class_boundaries_are_powers_of_two() {
        for shift in MIN_SHIFT..=MAX_SHIFT {
            let class = 1 << shift;
            assert_eq!(size_to_class(class - 1).unwrap(), class);
            assert_eq!(size_to_class(class).unwrap(), class);
            if shift < MAX_SHIFT {
                assert_eq!(size_to_class(class + 1).unwrap(), class << 1);
            }
        }
    }

    #[test]
    fn class_rounding_errors_above_limit() {
        assert!(matches!(
            size_to_class(MAX_ALLOC_SIZE + 1),
            Err(MemoryError::AllocationTooLarge { .. })
        ));
    }

    #[test]
    fn kmalloc_large_is_contiguous_and_reused() {
        let dm = KernelDirectMap;
        let page_alloc = Box::new(PageAllocator::new());
        let alloc = Box::new(KernelAllocator::new(&dm, &page_alloc));

        let a = alloc.alloc((1 << 22) + 1).unwrap();
        let b = alloc.alloc(1 << 22).unwrap();

        assert_eq!(a.as_usize() % PAGE_SIZE, 0);
        assert_eq!(b.as_usize() % PAGE_SIZE, 0);

        alloc.free(a, (1 << 22) + 1).unwrap();
        let c = alloc.alloc(1 << 23).unwrap();
        assert_eq!(c.as_u64(), a.as_u64());
    }

    #[test]
    fn kmalloc_large_allocations_do_not_overlap() {
        let dm = KernelDirectMap;
        let page_alloc = Box::new(PageAllocator::new());
        let alloc = Box::new(KernelAllocator::new(&dm, &page_alloc));

        let a = alloc.alloc(1 << 22).unwrap();
        let b = alloc.alloc(1 << 22).unwrap();

        let a_phys = a.as_u64();
        let b_phys = b.as_u64();

        assert_ne!(a_phys, b_phys);
        let diff = a_phys.abs_diff(b_phys);
        assert!(diff >= (1 << 22));
    }

    #[test]
    fn kmalloc_large_free_and_realloc_same_class_reuses_address() {
        let dm = KernelDirectMap;
        let page_alloc = Box::new(PageAllocator::new());
        let alloc = Box::new(KernelAllocator::new(&dm, &page_alloc));

        let a = alloc.alloc(1 << 24).unwrap();
        let b = alloc.alloc(1 << 24).unwrap();
        assert_ne!(a.as_u64(), b.as_u64());

        alloc.free(b, 1 << 24).unwrap();
        let c = alloc.alloc(1 << 24).unwrap();
        assert_eq!(c.as_u64(), b.as_u64());
    }
}
