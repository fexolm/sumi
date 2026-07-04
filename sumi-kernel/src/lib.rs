#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod arch;
pub mod drivers;
pub mod exec;
pub mod fs;
pub mod memory;
pub mod sched;
pub mod syscall;
pub mod time;

use arch::{KernelDirectMap, RootPageTable};
use memory::alloc::{kmalloc::KernelAllocator, palloc::PageAllocator};
use sumi_abi::address::VirtualAddr;
use sumi_abi::arch::layout::{DIRECT_MAP_PML4, USER_MMAP_BASE};

// Global kernel state
pub static PAGE_ALLOCATOR: PageAllocator = PageAllocator::new();
pub static KERNEL_DIRECT_MAP: KernelDirectMap = KernelDirectMap;
pub static KERNEL_ALLOCATOR: KernelAllocator<KernelDirectMap> =
    KernelAllocator::new(&KERNEL_DIRECT_MAP, &PAGE_ALLOCATOR);
// SAFETY: DIRECT_MAP_PML4 is the physical address of the PML4 table initialized
// by the hypervisor before the kernel starts.
pub static KERNEL_PAGE_TABLE: spin::Mutex<RootPageTable<KernelDirectMap>> =
    spin::Mutex::new(unsafe { RootPageTable::from_paddr(DIRECT_MAP_PML4, &KERNEL_ALLOCATOR) });

pub static FD_TABLE: spin::Mutex<fs::FdTable> = spin::Mutex::new(fs::FdTable::new());
pub static VIRTIO_FS: spin::Once<fs::virtio_fs::VirtioFsClient> = spin::Once::new();
pub static VIRTIO_CONSOLE: spin::Once<drivers::virtio::console::VirtioConsole> = spin::Once::new();
pub static RNG_SEED: spin::Once<[u8; 32]> = spin::Once::new();

/// Global TLB generation counter. Bumped whenever a page-table change requires
/// all CPUs to flush their TLBs (mprotect PROT_NONE, munmap). Each CPU checks
/// this at syscall return and performs a CR3 reload if its local generation lags.
pub static TLB_GENERATION: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// Unified lock for all user-space memory bookkeeping.
/// Previously split across three separate statics (BRK_BASE, BRK_CURRENT, MMAP_NEXT);
/// merged so that operations that touch multiple fields (e.g. brk + mmap interplay)
/// can be done atomically under one lock.
pub struct MemoryState {
    pub brk_base: VirtualAddr,
    pub brk_current: VirtualAddr,
    pub mmap_next: VirtualAddr,
}

pub static MEMORY_STATE: spin::Mutex<MemoryState> = spin::Mutex::new(MemoryState {
    brk_base: VirtualAddr::new(0),
    brk_current: VirtualAddr::new(0),
    mmap_next: USER_MMAP_BASE,
});

/// Returns the global VirtioFsClient. Panics if not yet initialized.
pub fn fs() -> &'static fs::virtio_fs::VirtioFsClient {
    VIRTIO_FS.get().expect("virtio-fs not initialized")
}

/// Returns the global VirtioConsole. Panics if not yet initialized.
pub fn console() -> &'static drivers::virtio::console::VirtioConsole {
    VIRTIO_CONSOLE
        .get()
        .expect("virtio-console not initialized")
}

pub static DAX_ALLOCATOR: spin::Mutex<fs::dax::DaxAllocator> =
    spin::Mutex::new(fs::dax::DaxAllocator::new());
pub static VMA_TABLE: spin::Mutex<memory::vma::VmaTable> =
    spin::Mutex::new(memory::vma::VmaTable::new());
