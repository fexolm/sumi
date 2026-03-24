use core::cell::UnsafeCell;
use core::ptr::write_bytes;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::memory::{
    alloc::palloc::PageAllocator,
    errors::{MemoryError, Result},
};
use sumi_abi::{
    address::{DirectMap, PhysicalAddr},
    arch::layout::PAGE_SIZE,
};

const _: () = assert!(
    sumi_abi::arch::layout::KERNEL_STACK.as_usize() != 0,
    "seg_base_cache uses 0 as uninitialized sentinel; KERNEL_STACK must be non-zero"
);

// ── Size classes ──────────────────────────────────────────────────────────────
const SIZE_CLASSES: [u32; 22] = [
    64, 128, 192, 256, 320, 384, 512, 640, 768, 1024, 1280, 1536, 2048, 2560, 3072, 4096, 5120,
    6144, 8192, 10240, 12288, 16384,
];
const SIZE_CLASS_COUNT: usize = SIZE_CLASSES.len();
const MAX_SMALL_ALLOC: usize = SIZE_CLASSES[SIZE_CLASS_COUNT - 1] as usize;
const MAX_ALLOC: usize = 1 << 24;

// ── Geometry ──────────────────────────────────────────────────────────────────
// Each palloc page (2 MiB) = one allocator segment, subdivided into 32 pages.
const SLAB_PAGE_SIZE: usize = 65_536; // 64 KiB per page
const SLAB_PAGES_PER_SEGMENT: usize = PAGE_SIZE / SLAB_PAGE_SIZE; // 32
const MAX_SEGMENTS: usize = 64;
const MAX_TOTAL_PAGES: usize = MAX_SEGMENTS * SLAB_PAGES_PER_SEGMENT; // 2048
const MAX_LARGE_ALLOCS: usize = 256;

// Block indices are 1-based so that the atomic remote-free heads can be
// zero-initialised (0 = FREE_END = empty list, no block has index 0).
const FREE_END: u32 = 0;
const PAGE_NONE: u32 = u32::MAX;

// ── Small helpers ─────────────────────────────────────────────────────────────
#[inline]
const fn encode_page(si: usize, pi: usize) -> u32 {
    (si * SLAB_PAGES_PER_SEGMENT + pi) as u32
}
#[inline]
const fn decode_page(v: u32) -> (usize, usize) {
    let v = v as usize;
    (v / SLAB_PAGES_PER_SEGMENT, v % SLAB_PAGES_PER_SEGMENT)
}
#[inline]
const fn page_id(si: usize, pi: usize) -> usize {
    si * SLAB_PAGES_PER_SEGMENT + pi
}
/// O(1) size-class lookup using `leading_zeros`.
///
/// Size classes are split into two regions:
///   1. ≤ 384: multiples of 64.  class = (size - 1) / 64
///   2. > 384: 3-sub-class bands per power-of-two (see SIZE_CLASSES above).
///      Each band [2^k, 2^(k+1)) has three classes spaced 2^(k-2) apart.
///      Sizes that exceed the 3rd class of band k belong to the first class of
///      band k+1.
#[inline]
fn size_class_for(size: usize) -> usize {
    debug_assert!(size >= 1);
    if size <= 384 {
        return (size - 1) / 64;
    }
    // floor(log2(size))
    let k = (usize::BITS - 1 - size.leading_zeros()) as usize;
    // Sizes 385–511 (k=8) round up into the 512 band (k=9).
    let k = if k < 9 { 9 } else { k };
    let band = 1usize << k;
    let step = 1usize << (k - 2);
    // off = size - band, or 0 if size < band (sizes 385–511 after k-adjustment).
    let off = if size >= band { size - band } else { 0 };
    let sub = ((off + step - 1) / step).min(3); // ceil(off / step), capped at 3
    let base = 6 + 3 * (k - 9); // base class index for this band
    (base + sub).min(SIZE_CLASS_COUNT - 1)
}

// ── Per-page remote free list (lock-free, Acquire/Release) ────────────────────
//
// Padded to one cache line (64 bytes) to prevent false sharing between pages.
// Block indices are 1-based; FREE_END = 0 allows zero-init.
//
// Any thread can push (CAS, Release ordering).
// Only the owning thread drains (atomic swap, Acquire ordering).
#[repr(C, align(64))]
struct RemoteFreeHead {
    head: AtomicU32,
    _pad: [u8; 60],
}

impl RemoteFreeHead {
    /// Push `block_idx` (1-based) onto this list.
    /// Writes `block_idx`'s intrusive next-pointer via the DirectMap before CAS.
    fn push(&self, block_idx: u32, block_phys: PhysicalAddr, dm: &impl DirectMap) {
        let mut cur = self.head.load(Ordering::Relaxed);
        loop {
            // Write cur into the block as its linked-list next pointer.
            unsafe {
                *block_phys.to_virtual(dm).as_ptr::<u32>() = cur;
            }
            match self.head.compare_exchange_weak(
                cur,
                block_idx,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(newer) => cur = newer,
            }
        }
    }

    /// Atomically drain the entire list; returns old head (FREE_END if empty).
    fn drain(&self) -> u32 {
        self.head.swap(FREE_END, Ordering::Acquire)
    }
}

// ── Page / segment metadata (behind the arena Mutex) ─────────────────────────
#[derive(Clone, Copy)]
struct SlabPage {
    in_use: bool,
}
impl SlabPage {
    const fn empty() -> Self {
        Self { in_use: false }
    }
}

#[derive(Clone, Copy)]
struct Segment {
    in_use: bool,
    base: PhysicalAddr,
    pages: [SlabPage; SLAB_PAGES_PER_SEGMENT],
}
impl Segment {
    const fn empty() -> Self {
        Self {
            in_use: false,
            base: PhysicalAddr::new(0),
            pages: [SlabPage::empty(); SLAB_PAGES_PER_SEGMENT],
        }
    }
}

#[derive(Clone, Copy)]
struct LargeAlloc {
    in_use: bool,
    base: PhysicalAddr,
    palloc_pages: usize,
}
impl LargeAlloc {
    const fn empty() -> Self {
        Self {
            in_use: false,
            base: PhysicalAddr::new(0),
            palloc_pages: 0,
        }
    }
}

// ── Thread-local heap ─────────────────────────────────────────────────────────
//
// All per-thread state — no locks needed to access this.
//
// `local_free_head[page_id]`  – head of the local (thread-private) free list.
//                                 FREE_END = 0; entries are 1-based block indices.
// `local_free_count[page_id]` – count of blocks in the local free list.
// `page_capacity[page_id]`    – capacity of the page (set on acquire, never changes).
//
// Kernel usage: stored as `UnsafeCell<LocalHeap>` inside `KernelAllocator`
//   (safe on single-CPU where only one thread runs at a time).
// Multi-threaded tests: each thread creates its own `LocalHeap` and passes it
//   explicitly to `alloc_with_local` / `free_with_local`.
pub struct LocalHeap {
    current_page: [u32; SIZE_CLASS_COUNT],
    local_free_head: [u32; MAX_TOTAL_PAGES],
    local_free_count: [u16; MAX_TOTAL_PAGES],
    page_capacity: [u16; MAX_TOTAL_PAGES],
}

impl LocalHeap {
    pub const fn new() -> Self {
        Self {
            current_page: [PAGE_NONE; SIZE_CLASS_COUNT],
            local_free_head: [FREE_END; MAX_TOTAL_PAGES],
            local_free_count: [0; MAX_TOTAL_PAGES],
            page_capacity: [0; MAX_TOTAL_PAGES],
        }
    }
}

// ── Shared arena (slow path) ──────────────────────────────────────────────────
struct KernelArena<'i, DM: DirectMap> {
    segments: [Segment; MAX_SEGMENTS],
    large: [LargeAlloc; MAX_LARGE_ALLOCS],
    palloc: &'i PageAllocator,
    dm: &'i DM,
}

impl<'i, DM: DirectMap> KernelArena<'i, DM> {
    const fn new(dm: &'i DM, palloc: &'i PageAllocator) -> Self {
        Self {
            segments: [Segment::empty(); MAX_SEGMENTS],
            large: [LargeAlloc::empty(); MAX_LARGE_ALLOCS],
            palloc,
            dm,
        }
    }

    fn alloc_large(&mut self, size: usize) -> Result<PhysicalAddr> {
        let n = size.div_ceil(PAGE_SIZE);
        let base = self.palloc.alloc(n)?;
        for slot in &mut self.large {
            if !slot.in_use {
                *slot = LargeAlloc {
                    in_use: true,
                    base,
                    palloc_pages: n,
                };
                return Ok(base);
            }
        }
        for i in 0..n {
            let _ = self.palloc.free(base.add(i * PAGE_SIZE));
        }
        Err(MemoryError::TooManyLargeAllocations)
    }

    fn free_large(&mut self, ptr: PhysicalAddr) -> Result<()> {
        for slot in &mut self.large {
            if slot.in_use && slot.base == ptr {
                let (base, n) = (slot.base, slot.palloc_pages);
                *slot = LargeAlloc::empty();
                for i in 0..n {
                    self.palloc.free(base.add(i * PAGE_SIZE))?;
                }
                return Ok(());
            }
        }
        Err(MemoryError::UnknownAllocation {
            addr: ptr.as_usize(),
        })
    }

    /// Acquire a page for size class `sc`. Returns (si, pi, capacity).
    fn acquire_page(
        &mut self,
        sc: usize,
        // atomic caches written here so hot paths read without the lock
        seg_base_cache: &[AtomicU64; MAX_SEGMENTS],
        page_block_sz_cache: &[AtomicU32; MAX_TOTAL_PAGES],
    ) -> Result<(usize, usize, u16)> {
        let block_size = SIZE_CLASSES[sc];
        for si in 0..MAX_SEGMENTS {
            if !self.segments[si].in_use {
                continue;
            }
            for pi in 0..SLAB_PAGES_PER_SEGMENT {
                if !self.segments[si].pages[pi].in_use {
                    let cap = self.init_page(
                        si,
                        pi,
                        block_size,
                        page_block_sz_cache,
                    )?;
                    return Ok((si, pi, cap));
                }
            }
        }
        let si = self.alloc_segment(seg_base_cache)?;
        let cap = self.init_page(
            si,
            0,
            block_size,
            page_block_sz_cache,
        )?;
        Ok((si, 0, cap))
    }

    fn alloc_segment(&mut self, seg_base_cache: &[AtomicU64; MAX_SEGMENTS]) -> Result<usize> {
        for si in 0..MAX_SEGMENTS {
            if !self.segments[si].in_use {
                let base = self.palloc.alloc(1)?;
                self.segments[si] = Segment {
                    in_use: true,
                    base,
                    pages: [SlabPage::empty(); SLAB_PAGES_PER_SEGMENT],
                };
                // Publish base so lock-free hot paths can compute page addresses.
                seg_base_cache[si].store(base.as_u64(), Ordering::Release);
                return Ok(si);
            }
        }
        Err(MemoryError::OutOfMemory)
    }

    fn init_page(
        &mut self,
        si: usize,
        pi: usize,
        block_size: u32,
        page_block_sz_cache: &[AtomicU32; MAX_TOTAL_PAGES],
    ) -> Result<u16> {
        let bs = block_size as usize;
        let capacity = (SLAB_PAGE_SIZE / bs) as u16;
        if capacity == 0 {
            return Err(MemoryError::InvalidSlabCapacity);
        }

        let base = self.segments[si].base;
        let pg_base = base.add(pi * SLAB_PAGE_SIZE);

        // Build embedded free list using 1-based block indices.
        // Block i is at byte offset (i-1)*block_size; its next = i+1 (or FREE_END for last).
        for i in 1u32..=capacity as u32 {
            let next = if i < capacity as u32 { i + 1 } else { FREE_END };
            let blk_addr = pg_base.add((i - 1) as usize * bs);
            unsafe {
                *blk_addr.to_virtual(self.dm).as_ptr::<u32>() = next;
            }
        }

        // seg_base_cache[si] was already stored with Release in alloc_segment.
        // Publish block_size atomically so free path reads it without the lock.
        page_block_sz_cache[page_id(si, pi)].store(block_size, Ordering::Release);
        self.segments[si].pages[pi] = SlabPage { in_use: true };
        Ok(capacity)
    }

    /// Release a fully-free page (and its segment if all pages are free).
    fn release_page(
        &mut self,
        si: usize,
        pi: usize,
        seg_base_cache: &[AtomicU64; MAX_SEGMENTS],
        page_block_sz_cache: &[AtomicU32; MAX_TOTAL_PAGES],
    ) {
        page_block_sz_cache[page_id(si, pi)].store(0, Ordering::Release);
        self.segments[si].pages[pi] = SlabPage::empty();

        if self.segments[si].pages.iter().all(|p| !p.in_use) {
            let base = self.segments[si].base;
            // Clear the cache entry before freeing so find_segment won't
            // match the stale base address after palloc reuses the page.
            seg_base_cache[si].store(0, Ordering::Release);
            self.segments[si] = Segment::empty();
            let _ = self.palloc.free(base);
        }
    }
}

// ── Public allocator ──────────────────────────────────────────────────────────
//
// Lock-free hot alloc path:
//   1. Pop from LocalHeap.local_free_head[current_page]  — zero locks.
//   2. Drain remote_free[current_page] into local         — zero locks, Acquire swap.
//   3. Slow path: acquire arena lock for new page.
//
// Lock-free hot free path (zero locks, one CAS):
//   1. find_segment(ptr) — lock-free linear scan of seg_base_cache (64 entries, L1).
//   2. page_block_sz_cache read — Acquire atomic load.
//   3. CAS push onto remote_free[page_id].head — Acquire/Release.
//
// False-sharing prevention:
//   Each RemoteFreeHead is padded to 64 bytes (one cache line per page slot).
pub struct KernelAllocator<'i, DM: DirectMap> {
    /// Slow-path only: segment + large-alloc management.
    arena: spin::Mutex<KernelArena<'i, DM>>,
    /// Lock-free metadata published by init_page / alloc_segment.
    seg_base_cache: [AtomicU64; MAX_SEGMENTS],
    page_block_sz_cache: [AtomicU32; MAX_TOTAL_PAGES],
    /// Lock-free per-page remote free lists. One cache line per slot = no false sharing.
    remote_free: [RemoteFreeHead; MAX_TOTAL_PAGES],
    /// Single-CPU local heap (kernel).  Multi-threaded callers pass LocalHeap explicitly.
    local: UnsafeCell<LocalHeap>,
    /// DirectMap needed on the free hot path to write intrusive next-pointers.
    dm: &'i DM,
}

// SAFETY: single-CPU kernel; `local` is only ever accessed by the one active CPU.
// All shared state (`remote_free`, atomic caches) is thread-safe by design.
unsafe impl<'i, DM: DirectMap + Sync> Sync for KernelAllocator<'i, DM> {}

impl<'i, DM: DirectMap> KernelAllocator<'i, DM> {
    pub const fn new(dm: &'i DM, palloc: &'i PageAllocator) -> Self {
        Self {
            arena: spin::Mutex::new(KernelArena::new(dm, palloc)),
            seg_base_cache: [const { AtomicU64::new(0) }; MAX_SEGMENTS],
            page_block_sz_cache: [const { AtomicU32::new(0) }; MAX_TOTAL_PAGES],
            remote_free: [const {
                RemoteFreeHead {
                    head: AtomicU32::new(FREE_END),
                    _pad: [0u8; 60],
                }
            }; MAX_TOTAL_PAGES],
            local: UnsafeCell::new(LocalHeap::new()),
            dm,
        }
    }

    // ── Explicit-LocalHeap API (multi-threaded / test usage) ──────────────────

    pub fn alloc_with_local(&self, local: &mut LocalHeap, size: usize) -> Result<PhysicalAddr> {
        let size = size.max(1);
        if size > MAX_ALLOC {
            return Err(MemoryError::AllocationTooLarge {
                requested: size,
                max: MAX_ALLOC,
            });
        }
        if size <= MAX_SMALL_ALLOC {
            self.alloc_small(local, size_class_for(size))
        } else {
            self.arena.lock().alloc_large(size)
        }
    }

    pub fn free_with_local(&self, local: &mut LocalHeap, ptr: PhysicalAddr) -> Result<()> {
        if self.free_small(local, ptr)? {
            return Ok(());
        }
        self.arena.lock().free_large(ptr)
    }

    pub fn calloc_with_local(&self, local: &mut LocalHeap, size: usize) -> Result<PhysicalAddr> {
        let addr = self.alloc_with_local(local, size)?;
        unsafe {
            write_bytes(addr.to_virtual(self.dm).as_ptr::<u8>(), 0, size);
        }
        Ok(addr)
    }

    // ── Single-CPU kernel convenience wrappers ────────────────────────────────

    pub fn alloc(&self, size: usize) -> Result<PhysicalAddr> {
        self.alloc_with_local(unsafe { &mut *self.local.get() }, size)
    }

    pub fn free(&self, ptr: PhysicalAddr) -> Result<()> {
        self.free_with_local(unsafe { &mut *self.local.get() }, ptr)
    }

    pub fn calloc(&self, size: usize) -> Result<PhysicalAddr> {
        self.calloc_with_local(unsafe { &mut *self.local.get() }, size)
    }

    pub fn direct_map(&self) -> &'i DM {
        self.dm
    }

    // ── Segment lookup (lock-free) ────────────────────────────────────────────

    /// Locate the segment and slab-page indices for any pointer returned by this
    /// allocator.  Performs a linear scan of `seg_base_cache` (64 × 8 bytes =
    /// 512 bytes, always fits in L1).  Returns `None` when the pointer does not
    /// belong to any live segment (e.g. after the segment was released).
    fn find_segment(&self, ptr: PhysicalAddr) -> Option<(usize, usize)> {
        // Each segment IS one palloc page (PAGE_SIZE = 2 MiB), so its base is
        // PAGE_SIZE-aligned.  Masking off the low bits gives the unique key.
        let seg_base = (ptr.as_usize() / PAGE_SIZE) * PAGE_SIZE;
        let pi = (ptr.as_usize() % PAGE_SIZE) / SLAB_PAGE_SIZE;
        for si in 0..MAX_SEGMENTS {
            if self.seg_base_cache[si].load(Ordering::Acquire) == seg_base as u64 {
                return Some((si, pi));
            }
        }
        None
    }

    // ── Hot alloc path (lock-free) ────────────────────────────────────────────

    fn alloc_small(&self, local: &mut LocalHeap, sc: usize) -> Result<PhysicalAddr> {
        // 1. Try current page's local free list.
        let cur = local.current_page[sc];
        if cur != PAGE_NONE {
            let (si, pi) = decode_page(cur);
            let pg = page_id(si, pi);
            if local.local_free_head[pg] == FREE_END {
                self.drain_remote(local, si, pi); // merge remote → local (no lock)
            }
            if local.local_free_head[pg] != FREE_END {
                return self.pop_local(local, si, pi);
            }
            local.current_page[sc] = PAGE_NONE;
        }

        // 2. Slow path: get a page from the arena.
        let (si, pi, capacity) = {
            let mut arena = self.arena.lock();
            arena.acquire_page(
                sc,
                &self.seg_base_cache,
                &self.page_block_sz_cache,
            )?
        };
        let pg = page_id(si, pi);
        local.current_page[sc] = encode_page(si, pi);
        local.local_free_head[pg] = 1; // first block (1-based)
        local.local_free_count[pg] = capacity;
        local.page_capacity[pg] = capacity;
        self.pop_local(local, si, pi)
    }

    /// Pop one block from page (si,pi)'s LOCAL free list. Lock-free.
    #[inline]
    fn pop_local(&self, local: &mut LocalHeap, si: usize, pi: usize) -> Result<PhysicalAddr> {
        let pg = page_id(si, pi);
        let block_idx = local.local_free_head[pg];
        if block_idx == FREE_END {
            return Err(MemoryError::SlabEmpty);
        }

        let block_size = self.page_block_sz_cache[pg].load(Ordering::Acquire) as usize;
        let seg_base = PhysicalAddr::new(self.seg_base_cache[si].load(Ordering::Acquire) as usize);
        let block_addr = seg_base.add(pi * SLAB_PAGE_SIZE + (block_idx - 1) as usize * block_size);

        let next = unsafe { *block_addr.to_virtual(self.dm).as_ptr::<u32>() };
        local.local_free_head[pg] = next;
        local.local_free_count[pg] -= 1;
        Ok(block_addr)
    }

    /// Drain the remote free list of page (si,pi) into the local free list.
    /// Lock-free: uses Acquire swap on the remote head.
    ///
    /// Must only be called when `local.local_free_head[pg] == FREE_END`; the
    /// remote list tail already points to FREE_END (the push protocol stores the
    /// old head, which was FREE_END when the list was empty), so no tail write is
    /// needed — the local list simply becomes the remote list as-is.
    fn drain_remote(&self, local: &mut LocalHeap, si: usize, pi: usize) {
        let pg = page_id(si, pi);
        debug_assert_eq!(
            local.local_free_head[pg],
            FREE_END,
            "drain_remote must only be called when local list is empty"
        );
        let remote_head = self.remote_free[pg].drain(); // Acquire swap
        if remote_head == FREE_END {
            return;
        }

        let block_size = self.page_block_sz_cache[pg].load(Ordering::Acquire) as usize;
        let seg_base = PhysicalAddr::new(self.seg_base_cache[si].load(Ordering::Acquire) as usize);

        // Walk the remote list to count elements.  The tail already ends with
        // FREE_END (written by the push protocol), so local_free_head is just
        // set to remote_head — no extra write to the tail block needed.
        let mut count = 0u16;
        let mut cur = remote_head;
        loop {
            count += 1;
            let blk_addr = seg_base.add(pi * SLAB_PAGE_SIZE + (cur - 1) as usize * block_size);
            cur = unsafe { *blk_addr.to_virtual(self.dm).as_ptr::<u32>() };
            if cur == FREE_END {
                // The remote list tail already points to FREE_END — no write needed.
                local.local_free_head[pg] = remote_head;
                local.local_free_count[pg] += count;
                return;
            }
        }
    }

    // ── Hot free path ─────────────────────────────────────────────────────────
    //
    // Two sub-paths:
    //   Local (owning thread)  — direct push onto LocalHeap list, zero atomics.
    //   Remote (other threads) — lock-free CAS onto remote_free, Acquire/Release.
    //
    // "Ownership" is detected by checking whether the LocalHeap passed in already
    // has this page active (current_page or a non-zero local_free_count).  This
    // avoids any per-thread ID or TLS lookup.

    fn free_small(&self, local: &mut LocalHeap, ptr: PhysicalAddr) -> Result<bool> {
        // Lock-free segment lookup: scan seg_base_cache (64 entries, ~512 B, L1).
        let Some((si, pi)) = self.find_segment(ptr) else {
            return Ok(false);
        };

        let pg = page_id(si, pi);
        let block_size = self.page_block_sz_cache[pg].load(Ordering::Acquire) as usize;
        if block_size == 0 {
            return Ok(false);
        } // page released between lookup and load

        let seg_base = PhysicalAddr::new(self.seg_base_cache[si].load(Ordering::Acquire) as usize);
        let page_base = seg_base.add(pi * SLAB_PAGE_SIZE);
        let offset = ptr.as_usize() - page_base.as_usize();

        if offset % block_size != 0 {
            return Err(MemoryError::SlabAlignmentMismatch {
                addr: ptr.as_usize(),
                block_size,
            });
        }
        let block_idx = (offset / block_size) as u32 + 1; // 1-based

        let sc = size_class_for(block_size);
        let is_owner = local.current_page[sc] == encode_page(si, pi) || local.page_capacity[pg] > 0; // acquired this page at some point

        if is_owner {
            // Local free: push directly onto this thread's list — no locks, no CAS.
            unsafe {
                *ptr.to_virtual(self.dm).as_ptr::<u32>() = local.local_free_head[pg];
            }
            local.local_free_head[pg] = block_idx;
            local.local_free_count[pg] += 1;

            // Release page if it just became fully free.
            if local.local_free_count[pg] == local.page_capacity[pg] {
                let mut arena = self.arena.lock();
                arena.release_page(si, pi, &self.seg_base_cache, &self.page_block_sz_cache);
                local.local_free_head[pg] = FREE_END;
                local.local_free_count[pg] = 0;
                local.page_capacity[pg] = 0;
                if local.current_page[sc] == encode_page(si, pi) {
                    local.current_page[sc] = PAGE_NONE;
                }
            }
        } else {
            // Remote free: lock-free CAS push (Release ordering).
            self.remote_free[pg].push(block_idx, ptr, self.dm);
        }
        Ok(true)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use sumi_abi::{address::VirtualAddr, arch::layout::KERNEL_STACK};

    // ── TestDirectMap ─────────────────────────────────────────────────────────
    struct TestDirectMap {
        phys_base: usize,
        buf: Vec<u8>,
    }
    impl TestDirectMap {
        fn new(pages: usize) -> Self {
            Self {
                phys_base: KERNEL_STACK.as_usize(),
                buf: vec![0u8; pages * PAGE_SIZE],
            }
        }
    }
    impl sumi_abi::address::DirectMap for TestDirectMap {
        fn p2v(&self, paddr: PhysicalAddr) -> VirtualAddr {
            VirtualAddr::new(self.buf.as_ptr() as usize + (paddr.as_usize() - self.phys_base))
        }
        fn v2p(&self, vaddr: VirtualAddr) -> Option<PhysicalAddr> {
            let base = self.buf.as_ptr() as usize;
            let len = self.buf.len();
            if vaddr.as_usize() < base || vaddr.as_usize() >= base + len {
                return None;
            }
            Some(PhysicalAddr::new(vaddr.as_usize() - base + self.phys_base))
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────
    // Box dm/pa then leak so the allocator can hold 'static refs.
    fn make_alloc(
        pages: usize,
    ) -> (
        Box<TestDirectMap>,
        Box<PageAllocator>,
        Box<KernelAllocator<'static, TestDirectMap>>,
    ) {
        let dm = Box::new(TestDirectMap::new(pages));
        let pa = Box::new(PageAllocator::new());
        let dm_ref: &'static TestDirectMap = unsafe { &*(dm.as_ref() as *const _) };
        let pa_ref: &'static PageAllocator = unsafe { &*(pa.as_ref() as *const _) };
        (dm, pa, Box::new(KernelAllocator::new(dm_ref, pa_ref)))
    }

    // ── Correctness tests ─────────────────────────────────────────────────────

    #[test]
    fn small_alloc_and_free() {
        let (_dm, _pa, alloc) = make_alloc(8);
        let mut local = LocalHeap::new();
        let a = alloc.alloc_with_local(&mut local, 64).unwrap();
        let b = alloc.alloc_with_local(&mut local, 64).unwrap();
        assert_ne!(a.as_u64(), b.as_u64());
        alloc.free_with_local(&mut local, a).unwrap();
        let c = alloc.alloc_with_local(&mut local, 64).unwrap();
        assert_eq!(c.as_u64(), a.as_u64()); // freed block is reused
    }

    #[test]
    fn small_allocs_dont_overlap() {
        let (_dm, _pa, alloc) = make_alloc(8);
        let mut local = LocalHeap::new();
        let a = alloc.alloc_with_local(&mut local, 4096).unwrap();
        let b = alloc.alloc_with_local(&mut local, 4096).unwrap();
        assert!(a.as_usize().abs_diff(b.as_usize()) >= 4096);
    }

    #[test]
    fn calloc_zeroes_memory() {
        let (dm, _pa, alloc) = make_alloc(8);
        let mut local = LocalHeap::new();
        let tmp = alloc.alloc_with_local(&mut local, 128).unwrap();
        unsafe {
            *tmp.to_virtual(&*dm).as_ptr::<u64>() = 0xDEAD_BEEF_CAFE_BABE;
        }
        alloc.free_with_local(&mut local, tmp).unwrap();
        let b = alloc.calloc_with_local(&mut local, 128).unwrap();
        let slice = unsafe { core::slice::from_raw_parts(b.to_virtual(&*dm).as_ptr::<u8>(), 128) };
        assert!(slice.iter().all(|&x| x == 0));
    }

    #[test]
    fn large_allocs_dont_overlap() {
        let (_dm, _pa, alloc) = make_alloc(8);
        let mut local = LocalHeap::new();
        let a = alloc.alloc_with_local(&mut local, 1 << 22).unwrap();
        let b = alloc.alloc_with_local(&mut local, 1 << 22).unwrap();
        assert!(a.as_usize().abs_diff(b.as_usize()) >= (1 << 22));
    }

    #[test]
    fn large_free_and_realloc_reuses_address() {
        let (_dm, _pa, alloc) = make_alloc(8);
        let mut local = LocalHeap::new();
        let a = alloc.alloc_with_local(&mut local, 1 << 24).unwrap();
        let b = alloc.alloc_with_local(&mut local, 1 << 24).unwrap();
        assert_ne!(a.as_u64(), b.as_u64());
        alloc.free_with_local(&mut local, b).unwrap();
        let c = alloc.alloc_with_local(&mut local, 1 << 24).unwrap();
        assert_eq!(c.as_u64(), b.as_u64());
    }

    #[test]
    fn alloc_too_large_fails() {
        let (_dm, _pa, alloc) = make_alloc(8);
        let mut local = LocalHeap::new();
        assert!(matches!(
            alloc.alloc_with_local(&mut local, MAX_ALLOC + 1),
            Err(MemoryError::AllocationTooLarge { .. })
        ));
    }

    #[test]
    fn cross_thread_free_via_remote_list() {
        let (_dm, _pa, alloc) = make_alloc(16);

        // Thread A exhausts the entire first 64-byte page so its local list is empty.
        // SLAB_PAGE_SIZE / 64 = 1024 blocks per 64 KiB page.
        let capacity = SLAB_PAGE_SIZE / 64;
        let mut local_a = LocalHeap::new();
        let ptrs: Vec<PhysicalAddr> = (0..capacity)
            .map(|_| alloc.alloc_with_local(&mut local_a, 64).unwrap())
            .collect();
        let first = ptrs[0];

        // Thread B frees `first` via its own LocalHeap → goes to remote_free.
        // (local_b has never acquired this page, so is_owner = false)
        let mut local_b = LocalHeap::new();
        alloc.free_with_local(&mut local_b, first).unwrap();

        // Thread A's local list is now empty (all 1024 blocks allocated).
        // The next alloc drains remote_free, which yields `first` back.
        let ptr2 = alloc.alloc_with_local(&mut local_a, 64).unwrap();
        assert_eq!(
            ptr2.as_u64(),
            first.as_u64(),
            "remote-freed block should be returned when local list is empty"
        );

        // Cleanup: free everything so the allocator is consistent.
        alloc.free_with_local(&mut local_a, ptr2).unwrap();
        for &p in &ptrs[1..] {
            alloc.free_with_local(&mut local_a, p).unwrap();
        }
    }

    // ── 100-thread benchmark ──────────────────────────────────────────────────
    //
    // Two-phase design to allow correct global-uniqueness verification:
    //
    //   Phase 1 (concurrent alloc): All 100 threads allocate OPS blocks each,
    //     keeping ALL allocations live (no frees yet).  Each thread uses its own
    //     TLS-style LocalHeap — the lock-free hot path.
    //
    //   Phase 2 (verify + free): After all threads join, collect every address.
    //     Check that no two concurrent live allocations share an address.
    //     Then free everything, mixing local and remote-free paths.
    //
    // Sizing: 100 threads × 20 allocs × 64 B = 2000 allocs.
    //   A 64 KiB page holds 1024 blocks of 64 B → ~2 pages needed (4 MiB).
    //   make_alloc(8) = 16 MiB — ample headroom.
    #[test]
    fn bench_100_threads() {
        use std::collections::HashSet;
        use std::sync::Arc;

        const THREADS: usize = 100;
        const OPS: usize = 20;
        const SIZE: usize = 64;

        let (_dm, _pa, alloc) = make_alloc(8);
        let alloc: Arc<KernelAllocator<'static, TestDirectMap>> = Arc::new(*alloc);

        // Phase 1: concurrent allocations, no frees yet.
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let a = Arc::clone(&alloc);
                std::thread::spawn(move || -> Vec<usize> {
                    let mut local = LocalHeap::new();
                    (0..OPS)
                        .map(|_| a.alloc_with_local(&mut local, SIZE).unwrap().as_usize())
                        .collect()
                })
            })
            .collect();

        let all_ptrs: Vec<Vec<usize>> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // Global uniqueness: no two live allocations share an address.
        let mut seen = HashSet::new();
        for ptrs in &all_ptrs {
            for &addr in ptrs {
                assert!(
                    seen.insert(addr),
                    "duplicate address {addr:#x} from concurrent allocs"
                );
            }
        }

        // Phase 2: free everything — half per-thread-local, half simulated remote.
        let handles: Vec<_> = all_ptrs
            .into_iter()
            .map(|ptrs| {
                let a = Arc::clone(&alloc);
                std::thread::spawn(move || {
                    let mut local = LocalHeap::new();
                    let mut remote = LocalHeap::new();
                    let mid = ptrs.len() / 2;
                    for &addr in &ptrs[..mid] {
                        a.free_with_local(&mut local, PhysicalAddr::new(addr))
                            .unwrap();
                    }
                    for &addr in &ptrs[mid..] {
                        a.free_with_local(&mut remote, PhysicalAddr::new(addr))
                            .unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }

    // ── Strict correctness tests ──────────────────────────────────────────────

    /// size_class_for must agree with the naive linear scan for EVERY size.
    #[test]
    fn size_class_exhaustive() {
        fn linear(size: usize) -> usize {
            for (i, &sc) in SIZE_CLASSES.iter().enumerate() {
                if size <= sc as usize { return i; }
            }
            SIZE_CLASS_COUNT - 1
        }
        for size in 1..=MAX_SMALL_ALLOC {
            let expected = linear(size);
            let got      = size_class_for(size);
            assert_eq!(
                got, expected,
                "size={size}: expected class {expected} ({}) but got class {got} ({})",
                SIZE_CLASSES[expected], SIZE_CLASSES[got]
            );
        }
    }

    /// Exact size-class boundary checks: SIZE_CLASSES[i] → class i,
    /// SIZE_CLASSES[i]+1 → class i+1.
    #[test]
    fn size_class_boundaries() {
        for i in 0..SIZE_CLASS_COUNT - 1 {
            let at   = SIZE_CLASSES[i] as usize;
            let over = at + 1;
            assert_eq!(size_class_for(at),   i,     "size={at} → expected class {i}");
            assert_eq!(size_class_for(over), i + 1, "size={over} → expected class {}", i + 1);
        }
        assert_eq!(size_class_for(MAX_SMALL_ALLOC), SIZE_CLASS_COUNT - 1);
    }

    /// size_class_for(SIZE_CLASSES[i]) must equal i for every class.
    #[test]
    fn size_class_exact_values() {
        for (i, &sc) in SIZE_CLASSES.iter().enumerate() {
            assert_eq!(size_class_for(sc as usize), i,
                "SIZE_CLASSES[{i}]={sc}: expected class {i}");
        }
    }

    /// alloc(0) must succeed (treated as size 1, class 0).
    #[test]
    fn zero_size_alloc() {
        let (_dm, _pa, alloc) = make_alloc(4);
        let mut local = LocalHeap::new();
        let p = alloc.alloc_with_local(&mut local, 0).unwrap();
        alloc.free_with_local(&mut local, p).unwrap();
    }

    /// Every allocation within a page must be aligned to block_size.
    #[test]
    fn alloc_block_aligned() {
        let (_dm, _pa, alloc) = make_alloc(8);
        let mut local = LocalHeap::new();
        for &sc in &SIZE_CLASSES {
            let size = sc as usize;
            let a = alloc.alloc_with_local(&mut local, size).unwrap();
            let b = alloc.alloc_with_local(&mut local, size).unwrap();
            let base_a = (a.as_usize() / SLAB_PAGE_SIZE) * SLAB_PAGE_SIZE;
            let base_b = (b.as_usize() / SLAB_PAGE_SIZE) * SLAB_PAGE_SIZE;
            assert_eq!((a.as_usize() - base_a) % size, 0,
                "size={size}: addr {:#x} not aligned", a.as_usize());
            assert_eq!((b.as_usize() - base_b) % size, 0,
                "size={size}: addr {:#x} not aligned", b.as_usize());
            alloc.free_with_local(&mut local, a).unwrap();
            alloc.free_with_local(&mut local, b).unwrap();
        }
    }

    /// Allocate all blocks in one 64-byte page — all addresses must be unique.
    #[test]
    fn exhaust_page_no_duplicates() {
        let (_dm, _pa, alloc) = make_alloc(8);
        let mut local = LocalHeap::new();
        let cap = SLAB_PAGE_SIZE / 64;
        let ptrs: Vec<usize> = (0..cap)
            .map(|_| alloc.alloc_with_local(&mut local, 64).unwrap().as_usize())
            .collect();
        let mut sorted = ptrs.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), cap, "duplicate addresses within one page");
        for p in ptrs { alloc.free_with_local(&mut local, PhysicalAddr::new(p)).unwrap(); }
    }

    /// Allocate cap+1 blocks — the extra block must come from a new page
    /// and not overlap with any from the first page.
    #[test]
    fn alloc_past_page_boundary() {
        let (_dm, _pa, alloc) = make_alloc(8);
        let mut local = LocalHeap::new();
        let cap = SLAB_PAGE_SIZE / 64 + 1;
        let mut ptrs: Vec<usize> = (0..cap)
            .map(|_| alloc.alloc_with_local(&mut local, 64).unwrap().as_usize())
            .collect();
        ptrs.sort();
        ptrs.dedup();
        assert_eq!(ptrs.len(), cap, "duplicate address across page boundary");
        for p in ptrs { alloc.free_with_local(&mut local, PhysicalAddr::new(p)).unwrap(); }
    }

    /// Allocate all blocks in a slab page (64-byte class), free all.
    /// The palloc segment must be returned and a second full-page alloc must succeed.
    #[test]
    fn segment_released_and_reused() {
        let (_dm, _pa, alloc) = make_alloc(4);
        let mut local = LocalHeap::new();
        let cap = SLAB_PAGE_SIZE / 64;
        // Round 1
        let ptrs: Vec<PhysicalAddr> = (0..cap)
            .map(|_| alloc.alloc_with_local(&mut local, 64).unwrap())
            .collect();
        for p in &ptrs { alloc.free_with_local(&mut local, *p).unwrap(); }
        // Round 2: segment was returned to palloc; re-allocating must succeed
        let ptrs2: Vec<PhysicalAddr> = (0..cap)
            .map(|_| alloc.alloc_with_local(&mut local, 64).unwrap())
            .collect();
        let mut addrs: Vec<usize> = ptrs2.iter().map(|p| p.as_usize()).collect();
        addrs.sort(); addrs.dedup();
        assert_eq!(addrs.len(), cap);
        for p in ptrs2 { alloc.free_with_local(&mut local, p).unwrap(); }
    }

    /// Multiple threads push to remote_free; owner drains and gets all back.
    #[test]
    fn multi_thread_remote_drain() {
        use std::sync::Arc;
        const THREADS: usize = 8;

        let (_dm, _pa, alloc) = make_alloc(16);
        let alloc = Arc::new(alloc);

        // Owner fills an entire page.
        let cap = SLAB_PAGE_SIZE / 64;
        let mut local_owner = LocalHeap::new();
        let ptrs: Vec<PhysicalAddr> = (0..cap)
            .map(|_| alloc.alloc_with_local(&mut local_owner, 64).unwrap())
            .collect();

        // Split equally among worker threads; each frees its chunk remotely.
        let chunk_size = cap / THREADS;
        let handles: Vec<_> = (0..THREADS).map(|t| {
            let a     = Arc::clone(&alloc);
            let chunk: Vec<PhysicalAddr> = ptrs[t * chunk_size..(t + 1) * chunk_size].to_vec();
            std::thread::spawn(move || {
                let mut local = LocalHeap::new();
                for p in chunk { a.free_with_local(&mut local, p).unwrap(); }
            })
        }).collect();
        for h in handles { h.join().unwrap(); }

        // Owner drains remote and reallocates — must get exactly `cap` unique addresses.
        let realloc: Vec<usize> = (0..cap)
            .map(|_| alloc.alloc_with_local(&mut local_owner, 64).unwrap().as_usize())
            .collect();
        let mut sorted = realloc.clone();
        sorted.sort(); sorted.dedup();
        assert_eq!(sorted.len(), cap, "not all remote-freed blocks returned by drain");

        for &p in &realloc {
            alloc.free_with_local(&mut local_owner, PhysicalAddr::new(p)).unwrap();
        }
    }

    /// After a page is fully freed (released back to palloc), its slot can be
    /// reacquired for a different size class without corruption.
    #[test]
    fn page_release_then_reacquire_different_size() {
        let (_dm, _pa, alloc) = make_alloc(8);
        let mut local = LocalHeap::new();

        // Fill and free a full 64-byte page (triggers page + segment release).
        let cap64 = SLAB_PAGE_SIZE / 64;
        let ptrs: Vec<PhysicalAddr> = (0..cap64)
            .map(|_| alloc.alloc_with_local(&mut local, 64).unwrap())
            .collect();
        for p in ptrs { alloc.free_with_local(&mut local, p).unwrap(); }

        // Now allocate 128-byte blocks — may reuse the same (si, pi) slot.
        let cap128 = SLAB_PAGE_SIZE / 128;
        let ptrs2: Vec<PhysicalAddr> = (0..cap128)
            .map(|_| alloc.alloc_with_local(&mut local, 128).unwrap())
            .collect();
        let mut addrs: Vec<usize> = ptrs2.iter().map(|p| p.as_usize()).collect();
        addrs.sort(); addrs.dedup();
        assert_eq!(addrs.len(), cap128, "duplicate after page reacquire for different size");
        for p in ptrs2 { alloc.free_with_local(&mut local, p).unwrap(); }
    }

    /// calloc on a previously-dirtied slot must return zeroed memory.
    #[test]
    fn calloc_with_dirty_slot() {
        let (dm, _pa, alloc) = make_alloc(8);
        let mut local = LocalHeap::new();
        // Dirty several slots.
        let ptrs: Vec<PhysicalAddr> = (0..16)
            .map(|_| alloc.alloc_with_local(&mut local, 64).unwrap())
            .collect();
        for &p in &ptrs {
            unsafe { *p.to_virtual(&*dm).as_ptr::<u64>() = 0xDEAD_BEEF_DEAD_BEEF; }
        }
        for p in ptrs { alloc.free_with_local(&mut local, p).unwrap(); }
        // calloc must zero regardless of previous contents.
        for _ in 0..16 {
            let p = alloc.calloc_with_local(&mut local, 64).unwrap();
            let slice = unsafe { core::slice::from_raw_parts(p.to_virtual(&*dm).as_ptr::<u8>(), 64) };
            assert!(slice.iter().all(|&b| b == 0), "calloc returned non-zeroed memory");
            alloc.free_with_local(&mut local, p).unwrap();
        }
    }

    /// Allocations of different size classes must not produce overlapping ranges.
    #[test]
    fn different_size_classes_no_overlap() {
        let (_dm, _pa, alloc) = make_alloc(8);
        let mut local = LocalHeap::new();
        let a = alloc.alloc_with_local(&mut local, 64).unwrap();
        let b = alloc.alloc_with_local(&mut local, 128).unwrap();
        let c = alloc.alloc_with_local(&mut local, 256).unwrap();
        // Minimum separation: block b must be at least 64 bytes away from a, etc.
        // The allocator places different size classes in different 64 KiB pages.
        let page_a = (a.as_usize() / SLAB_PAGE_SIZE) * SLAB_PAGE_SIZE;
        let page_b = (b.as_usize() / SLAB_PAGE_SIZE) * SLAB_PAGE_SIZE;
        let page_c = (c.as_usize() / SLAB_PAGE_SIZE) * SLAB_PAGE_SIZE;
        assert_ne!(page_a, page_b, "size 64 and 128 share a slab page");
        assert_ne!(page_a, page_c);
        assert_ne!(page_b, page_c);
        alloc.free_with_local(&mut local, a).unwrap();
        alloc.free_with_local(&mut local, b).unwrap();
        alloc.free_with_local(&mut local, c).unwrap();
    }

    /// Writing to an allocated block must not corrupt adjacent blocks.
    #[test]
    fn write_does_not_corrupt_neighbors() {
        let (dm, _pa, alloc) = make_alloc(8);
        let mut local = LocalHeap::new();
        const N: usize = 32;
        let ptrs: Vec<PhysicalAddr> = (0..N)
            .map(|_| alloc.alloc_with_local(&mut local, 64).unwrap())
            .collect();
        // Write sentinel to each block.
        for (i, &p) in ptrs.iter().enumerate() {
            unsafe { *p.to_virtual(&*dm).as_ptr::<u64>() = i as u64 * 0x0101_0101_0101_0101; }
        }
        // Verify sentinels are intact (no aliasing).
        for (i, &p) in ptrs.iter().enumerate() {
            let val = unsafe { *p.to_virtual(&*dm).as_ptr::<u64>() };
            assert_eq!(val, i as u64 * 0x0101_0101_0101_0101,
                "block {i} at {:#x} was corrupted", p.as_usize());
        }
        for p in ptrs { alloc.free_with_local(&mut local, p).unwrap(); }
    }

    /// After a page is fully freed and its segment released, freeing any pointer
    /// from that page a second time must return an error rather than silently
    /// corrupting allocator state.
    ///
    /// Mechanism: `find_segment` returns `None` once `seg_base_cache[si]` is
    /// cleared to 0 by `release_page`, so `free_small` returns `Ok(false)`,
    /// then `free_large` returns `Err(UnknownAllocation)`.
    #[test]
    fn double_free_is_detected() {
        let (_dm, _pa, alloc) = make_alloc(4);
        let mut local = LocalHeap::new();
        // A block size of SLAB_PAGE_SIZE/2 = 32768 gives capacity = 2 per slab page.
        // Freeing both blocks exhausts the page and triggers segment release.
        let cap = SLAB_PAGE_SIZE / 32768; // == 2
        let ptrs: Vec<PhysicalAddr> = (0..cap)
            .map(|_| alloc.alloc_with_local(&mut local, 32768).unwrap())
            .collect();
        for &p in &ptrs {
            alloc.free_with_local(&mut local, p).unwrap();
        }
        // Page (and segment) is now released; a second free of the same pointer
        // must be rejected.
        let result = alloc.free_with_local(&mut local, ptrs[0]);
        assert!(
            result.is_err(),
            "double-free of a released page should return an error"
        );
    }

    /// After fully freeing a page the segment is returned to palloc.  The next
    /// allocation of the same size class must successfully reuse that segment
    /// (or another one) and produce the expected number of unique addresses.
    ///
    /// With a tight palloc budget (2 pages) a failure to release the first
    /// segment would cause OOM on the second round.
    #[test]
    fn alloc_uses_released_segment_base() {
        let (_dm, _pa, alloc) = make_alloc(2);
        let mut local = LocalHeap::new();
        let cap = SLAB_PAGE_SIZE / 64;

        // Round 1: allocate a full slab page, then free everything.
        let round1: Vec<PhysicalAddr> = (0..cap)
            .map(|_| alloc.alloc_with_local(&mut local, 64).unwrap())
            .collect();
        for p in &round1 {
            alloc.free_with_local(&mut local, *p).unwrap();
        }

        // Round 2: with only 2 palloc pages available, this succeeds only if
        // the segment from round 1 was properly released back to palloc.
        let round2: Vec<PhysicalAddr> = (0..cap)
            .map(|_| alloc.alloc_with_local(&mut local, 64).unwrap())
            .collect();
        let mut addrs: Vec<usize> = round2.iter().map(|p| p.as_usize()).collect();
        addrs.sort();
        addrs.dedup();
        assert_eq!(addrs.len(), cap, "duplicate addresses in round 2");

        for p in round2 {
            alloc.free_with_local(&mut local, p).unwrap();
        }
    }
}
