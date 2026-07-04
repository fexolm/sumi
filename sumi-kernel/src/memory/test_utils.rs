use super::alloc::{kmalloc::KernelAllocator, palloc::PageAllocator};
use sumi_abi::{
    address::{DirectMap, PhysicalAddr, VirtualAddr},
    arch::layout::{KERNEL_STACK, PAGE_SIZE},
};

/// PAGE_TABLE_ALIGN must be a power of two and at least as large as the
/// `align(4096)` on `PageTable`, so that virtual addresses produced by
/// `p2v` satisfy the alignment assertion in `VirtualAddr::as_ref_mut`.
/// Set to the full `PAGE_SIZE` (2 MiB, not just 4 KiB) so `p2v` also
/// satisfies callers that require page-aligned addresses at `PAGE_SIZE`
/// granularity (e.g. `sched::clone::clone_create_user_thread`'s kernel-stack
/// top) — `KERNEL_STACK` and every `PageAllocator` allocation are
/// `PAGE_SIZE`-aligned, so preserving that alignment through `p2v` requires
/// `aligned_ptr` itself to be `PAGE_SIZE`-aligned, not just 4 KiB-aligned.
const PAGE_TABLE_ALIGN: usize = PAGE_SIZE;

/// Backs physical addresses with a `PAGE_SIZE`-aligned heap buffer.
/// Physical address KERNEL_STACK maps to buf[0], so subsequent allocations
/// at KERNEL_STACK + k*PAGE_SIZE are also PAGE_TABLE_ALIGN-aligned.
pub struct TestDirectMap {
    phys_base: usize,
    _buf: std::vec::Vec<u8>,
    aligned_ptr: *mut u8,
    len: usize,
}

impl TestDirectMap {
    pub fn new(pages: usize) -> Self {
        let len = pages * PAGE_SIZE;
        let raw_len = len + PAGE_TABLE_ALIGN;
        let buf = vec![0u8; raw_len];
        let raw = buf.as_ptr() as usize;
        let aligned = (raw + PAGE_TABLE_ALIGN - 1) & !(PAGE_TABLE_ALIGN - 1);
        let aligned_ptr = aligned as *mut u8;
        Self {
            phys_base: KERNEL_STACK.as_usize(),
            _buf: buf,
            aligned_ptr,
            len,
        }
    }
}

// SAFETY: TestDirectMap owns the buffer; aligned_ptr is valid for `len` bytes
// inside `_buf` which is never moved while the struct is alive.
unsafe impl Send for TestDirectMap {}
unsafe impl Sync for TestDirectMap {}

impl DirectMap for TestDirectMap {
    fn p2v(&self, paddr: PhysicalAddr) -> VirtualAddr {
        VirtualAddr::new(self.aligned_ptr as usize + (paddr.as_usize() - self.phys_base))
    }

    fn v2p(&self, vaddr: VirtualAddr) -> Option<PhysicalAddr> {
        let base = self.aligned_ptr as usize;
        if vaddr.as_usize() < base || vaddr.as_usize() >= base + self.len {
            return None;
        }
        Some(PhysicalAddr::new(vaddr.as_usize() - base + self.phys_base))
    }
}

/// Creates a TestDirectMap, PageAllocator, and KernelAllocator.
/// Returns boxed values; the static references into them are valid as long
/// as the boxes are alive (caller must keep the boxes alive).
pub fn make_alloc(
    pages: usize,
) -> (
    Box<TestDirectMap>,
    Box<PageAllocator>,
    Box<KernelAllocator<'static, TestDirectMap>>,
) {
    let dm = Box::new(TestDirectMap::new(pages));
    let pa = Box::new(PageAllocator::new());
    // SAFETY: The boxes live for the duration of each test; the raw-pointer
    // cast to 'static is safe because the caller keeps the boxes alive.
    let dm_ref: &'static TestDirectMap = unsafe { &*(dm.as_ref() as *const _) };
    let pa_ref: &'static PageAllocator = unsafe { &*(pa.as_ref() as *const _) };
    (dm, pa, Box::new(KernelAllocator::new(dm_ref, pa_ref)))
}
