use core::panic::PanicInfo;

use sumi_abi::arch::layout::DIRECT_MAP_PML4;
use sumi_kernel::{
    KernelState,
    arch::{KernelDirectMap, RootPageTable, debugcon_write_byte, halt_forever},
    memory::alloc::{kmalloc::KernelAllocator, palloc::PageAllocator},
};

static PAGE_ALLOCATOR: PageAllocator = PageAllocator::new();
static KERNEL_DIRECT_MAP: KernelDirectMap = KernelDirectMap;
static KERNEL_ALLOCATOR: KernelAllocator<KernelDirectMap> =
    KernelAllocator::new(&KERNEL_DIRECT_MAP, &PAGE_ALLOCATOR);
static KERNEL_PAGE_TABLE: RootPageTable<KernelDirectMap> =
    unsafe { RootPageTable::from_paddr(DIRECT_MAP_PML4, &KERNEL_ALLOCATOR) };

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    let _kernel = KernelState::new(&PAGE_ALLOCATOR, &KERNEL_ALLOCATOR, &KERNEL_PAGE_TABLE);

    debugcon_write_byte(0x41);
    halt_forever()
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    halt_forever()
}
