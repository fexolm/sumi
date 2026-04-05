#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod arch;
pub mod drivers;
pub mod exec;
pub mod fs;
pub mod memory;
pub mod selftest;
pub mod syscall;

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
pub static KERNEL_PAGE_TABLE: RootPageTable<KernelDirectMap> =
    unsafe { RootPageTable::from_paddr(DIRECT_MAP_PML4, &KERNEL_ALLOCATOR) };

pub static FD_TABLE: spin::Mutex<fs::FdTable> = spin::Mutex::new(fs::FdTable::new());
pub static VIRTIO_FS: spin::Once<fs::virtio_fs::VirtioFsClient> = spin::Once::new();
pub static VIRTIO_CONSOLE: spin::Once<drivers::virtio::console::VirtioConsole> = spin::Once::new();

// User program memory state
pub static BRK_BASE: spin::Mutex<VirtualAddr> = spin::Mutex::new(VirtualAddr::new(0));
pub static BRK_CURRENT: spin::Mutex<VirtualAddr> = spin::Mutex::new(VirtualAddr::new(0));
pub static MMAP_NEXT: spin::Mutex<VirtualAddr> = spin::Mutex::new(USER_MMAP_BASE);

pub static DAX_ALLOCATOR: spin::Mutex<fs::dax::DaxAllocator> =
    spin::Mutex::new(fs::dax::DaxAllocator::new());
pub static VMA_TABLE: spin::Mutex<memory::vma::VmaTable> =
    spin::Mutex::new(memory::vma::VmaTable::new());
