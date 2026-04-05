use core::alloc::{GlobalAlloc, Layout};
use core::panic::PanicInfo;

use sumi_kernel::{
    KernelState,
    arch::{debugcon_write_byte, halt_forever, syscall},
    exec,
    fs::virtio_fs::VirtioFsClient,
};

struct GlobalKernelAlloc;

unsafe impl GlobalAlloc for GlobalKernelAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match sumi_kernel::KERNEL_ALLOCATOR.alloc(layout.size()) {
            Ok(paddr) => paddr
                .to_virtual(sumi_kernel::KERNEL_ALLOCATOR.direct_map())
                .as_ptr::<u8>(),
            Err(_) => core::ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        use sumi_abi::address::VirtualAddr;
        let vaddr = VirtualAddr::new(ptr as usize);
        if let Some(paddr) = vaddr.to_physical(sumi_kernel::KERNEL_ALLOCATOR.direct_map()) {
            let _ = sumi_kernel::KERNEL_ALLOCATOR.free(paddr);
        }
    }
}

#[global_allocator]
static GLOBAL_ALLOC: GlobalKernelAlloc = GlobalKernelAlloc;

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    let _kernel = KernelState::new(
        &sumi_kernel::PAGE_ALLOCATOR,
        &sumi_kernel::KERNEL_ALLOCATOR,
        &sumi_kernel::KERNEL_PAGE_TABLE,
    );

    syscall::init();

    // Initialize virtio-fs if the device is present
    if let Some(fs) = VirtioFsClient::init(&sumi_kernel::KERNEL_ALLOCATOR) {
        sumi_kernel::VIRTIO_FS.call_once(|| fs);
    }

    // Check for user program to execute
    if let Some(path) = exec::read_boot_info() {
        exec::exec_user_program(path);
    }

    // No program specified — run selftests
    sumi_kernel::selftest::run_all();

    debugcon_write_byte(0x41);
    halt_forever()
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    // Write "PANIC\n" to debugcon so we can see panics in VM output
    for &b in b"PANIC\n" {
        debugcon_write_byte(b);
    }
    halt_forever()
}
