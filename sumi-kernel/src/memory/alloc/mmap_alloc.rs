use alloc::vec::Vec;

use sumi_abi::{
    address::{PhysicalAddr, VirtualAddr},
    arch::layout::{BASE_PAGE_SIZE, PAGE_SIZE, USER_MMAP_BASE},
};

use crate::memory::errors::{MemoryError, Result};

const FRAMES_PER_ARENA: usize = PAGE_SIZE / BASE_PAGE_SIZE;
const BITMAP_WORDS: usize = FRAMES_PER_ARENA / u64::BITS as usize;
const FULL_WORD: u64 = !0;

#[derive(Clone, Copy)]
struct MmapArena {
    vaddr_base: VirtualAddr,
    paddr_base: PhysicalAddr,
    free: [u64; BITMAP_WORDS],
    free_frames: usize,
}

impl MmapArena {
    fn new(vaddr_base: VirtualAddr, paddr_base: PhysicalAddr) -> Self {
        Self {
            vaddr_base,
            paddr_base,
            free: [FULL_WORD; BITMAP_WORDS],
            free_frames: FRAMES_PER_ARENA,
        }
    }

    fn contains(&self, vaddr: VirtualAddr) -> bool {
        let addr = vaddr.as_usize();
        let base = self.vaddr_base.as_usize();
        addr >= base && addr < base + PAGE_SIZE
    }

    fn alloc(&mut self, frames: usize) -> Option<VirtualAddr> {
        if frames == 0 || frames > self.free_frames || frames > FRAMES_PER_ARENA {
            return None;
        }

        for start in (0..=FRAMES_PER_ARENA - frames).rev() {
            if self.range_is_free(start, frames) {
                self.mark_range(start, frames, false);
                self.free_frames -= frames;
                return Some(self.vaddr_base.add(start * BASE_PAGE_SIZE));
            }
        }

        None
    }

    fn free(&mut self, vaddr: VirtualAddr, frames: usize) {
        let start = (vaddr.as_usize() - self.vaddr_base.as_usize()) / BASE_PAGE_SIZE;
        self.mark_range(start, frames, true);
        self.free_frames += frames;
    }

    fn paddr_for(&self, vaddr: VirtualAddr) -> PhysicalAddr {
        self.paddr_base
            .add(vaddr.as_usize() - self.vaddr_base.as_usize())
    }

    fn range_is_free(&self, start: usize, frames: usize) -> bool {
        for frame in start..start + frames {
            let word = frame / u64::BITS as usize;
            let bit = frame % u64::BITS as usize;
            if self.free[word] & (1 << bit) == 0 {
                return false;
            }
        }
        true
    }

    fn mark_range(&mut self, start: usize, frames: usize, free: bool) {
        for frame in start..start + frames {
            let word = frame / u64::BITS as usize;
            let bit = frame % u64::BITS as usize;
            if free {
                self.free[word] |= 1 << bit;
            } else {
                self.free[word] &= !(1 << bit);
            }
        }
    }
}

struct UserMmapAllocatorInner {
    arenas: Vec<MmapArena>,
}

impl UserMmapAllocatorInner {
    const fn new() -> Self {
        Self { arenas: Vec::new() }
    }
}

pub struct UserMmapAllocator(spin::Mutex<UserMmapAllocatorInner>);

#[allow(clippy::new_without_default)]
impl UserMmapAllocator {
    pub const fn new() -> Self {
        Self(spin::Mutex::new(UserMmapAllocatorInner::new()))
    }

    pub fn can_allocate_small(&self, len: usize) -> bool {
        len > 0 && len < PAGE_SIZE
    }

    pub fn alloc(&self, len: usize) -> Result<(VirtualAddr, usize)> {
        let aligned_len = align_up_base_page(len)?;
        let frames = aligned_len / BASE_PAGE_SIZE;
        let mut inner = self.0.lock();

        loop {
            if let Some((arena_index, vaddr)) = inner
                .arenas
                .iter_mut()
                .enumerate()
                .find_map(|(index, arena)| arena.alloc(frames).map(|vaddr| (index, vaddr)))
            {
                zero_small_range(inner.arenas[arena_index].paddr_for(vaddr), aligned_len);
                return Ok((vaddr, aligned_len));
            }

            let arena = allocate_arena(inner.arenas.iter().map(|arena| arena.vaddr_base))?;
            inner.arenas.push(arena);
        }
    }

    pub fn free(&self, vaddr: VirtualAddr, len: usize) {
        let Ok(aligned_len) = align_up_base_page(len) else {
            return;
        };
        let frames = aligned_len / BASE_PAGE_SIZE;
        let mut inner = self.0.lock();
        let Some(index) = inner.arenas.iter().position(|arena| arena.contains(vaddr)) else {
            return;
        };

        inner.arenas[index].free(vaddr, frames);
        if inner.arenas[index].free_frames == FRAMES_PER_ARENA {
            let arena = inner.arenas.swap_remove(index);
            if crate::KERNEL_PAGE_TABLE
                .lock()
                .unmap_2mb(arena.vaddr_base)
                .is_ok()
            {
                let _ = crate::PAGE_ALLOCATOR.free(arena.paddr_base);
            }
        }
    }

    pub fn contains(&self, vaddr: VirtualAddr) -> bool {
        self.0
            .lock()
            .arenas
            .iter()
            .any(|arena| arena.contains(vaddr))
    }

    pub fn lowest_arena_base(&self) -> Option<VirtualAddr> {
        self.0
            .lock()
            .arenas
            .iter()
            .map(|arena| arena.vaddr_base.as_usize())
            .min()
            .map(VirtualAddr::new)
    }
}

fn allocate_arena(existing_bases: impl Iterator<Item = VirtualAddr>) -> Result<MmapArena> {
    let high = existing_bases
        .map(|base| base.as_usize())
        .min()
        .map(VirtualAddr::new)
        .unwrap_or(USER_MMAP_BASE);
    let vaddr_base = crate::VMA_TABLE
        .lock()
        .find_free_downward_aligned(high, PAGE_SIZE, PAGE_SIZE)
        .ok_or(MemoryError::OutOfMemory)?;
    let paddr_base = crate::PAGE_ALLOCATOR.alloc(1)?;

    crate::exec::zero_page(paddr_base);
    if crate::KERNEL_PAGE_TABLE
        .lock()
        .map_2mb(vaddr_base, paddr_base)
        .is_err()
    {
        let _ = crate::PAGE_ALLOCATOR.free(paddr_base);
        return Err(MemoryError::OutOfMemory);
    }

    Ok(MmapArena::new(vaddr_base, paddr_base))
}

fn zero_small_range(paddr: PhysicalAddr, len: usize) {
    let vaddr = paddr.to_virtual(&crate::KERNEL_DIRECT_MAP);
    unsafe {
        core::ptr::write_bytes(vaddr.as_ptr::<u8>(), 0, len);
    }
}

fn align_up_base_page(len: usize) -> Result<usize> {
    len.checked_add(BASE_PAGE_SIZE - 1)
        .map(|value| value & !(BASE_PAGE_SIZE - 1))
        .filter(|&value| value <= PAGE_SIZE)
        .ok_or(MemoryError::OutOfMemory)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VBASE: VirtualAddr = VirtualAddr::new(0x7000_0000);
    const PBASE: PhysicalAddr = PhysicalAddr::new(0x2000_0000);

    #[test]
    fn arena_allocates_from_high_addresses_first() {
        let mut arena = MmapArena::new(VBASE, PBASE);

        let first = arena.alloc(16).expect("first allocation");
        let second = arena.alloc(1).expect("second allocation");

        assert_eq!(first, VBASE.add(PAGE_SIZE - 16 * BASE_PAGE_SIZE));
        assert_eq!(second, VBASE.add(PAGE_SIZE - 17 * BASE_PAGE_SIZE));
        assert_eq!(arena.free_frames, FRAMES_PER_ARENA - 17);
    }

    #[test]
    fn arena_free_makes_exact_range_reusable() {
        let mut arena = MmapArena::new(VBASE, PBASE);

        let first = arena.alloc(8).expect("first allocation");
        let second = arena.alloc(8).expect("second allocation");
        arena.free(first, 8);

        let reused = arena.alloc(8).expect("reused allocation");
        assert_eq!(reused, first);
        assert_ne!(reused, second);
    }

    #[test]
    fn arena_reports_containment_within_2mb_window() {
        let arena = MmapArena::new(VBASE, PBASE);

        assert!(arena.contains(VBASE));
        assert!(arena.contains(VBASE.add(PAGE_SIZE - 1)));
        assert!(!arena.contains(VBASE.add(PAGE_SIZE)));
    }

    #[test]
    fn paddr_for_preserves_offset_inside_arena() {
        let arena = MmapArena::new(VBASE, PBASE);
        let offset = 37 * BASE_PAGE_SIZE;

        assert_eq!(arena.paddr_for(VBASE.add(offset)), PBASE.add(offset));
    }

    #[test]
    fn align_up_base_page_accepts_sub_2mb_lengths_only() {
        assert_eq!(align_up_base_page(1).unwrap(), BASE_PAGE_SIZE);
        assert_eq!(align_up_base_page(BASE_PAGE_SIZE).unwrap(), BASE_PAGE_SIZE);
        assert_eq!(
            align_up_base_page(PAGE_SIZE - BASE_PAGE_SIZE + 1).unwrap(),
            PAGE_SIZE
        );
        assert!(align_up_base_page(PAGE_SIZE + 1).is_err());
    }
}
