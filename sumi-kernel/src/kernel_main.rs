use core::alloc::{GlobalAlloc, Layout};
use core::panic::PanicInfo;

use sumi_kernel::{
    arch::{halt_forever, syscall},
    exec,
    fs::virtio_fs::VirtioFsClient,
    kprintln,
};

struct GlobalKernelAlloc;

unsafe impl GlobalAlloc for GlobalKernelAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match sumi_kernel::KERNEL_ALLOCATOR.alloc_aligned(layout.size(), layout.align()) {
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
    syscall::init();

    // Initialize virtio-fs if the device is present
    if let Some(fs) = VirtioFsClient::init(&sumi_kernel::KERNEL_ALLOCATOR) {
        sumi_kernel::VIRTIO_FS.call_once(|| fs);
    }

    if let Some(console) = sumi_kernel::drivers::virtio::console::VirtioConsole::init(
        &sumi_kernel::KERNEL_ALLOCATOR,
        &sumi_kernel::PAGE_ALLOCATOR,
    ) {
        sumi_kernel::VIRTIO_CONSOLE.call_once(|| console);
    }

    // Check for user program to execute
    if let Some(path) = exec::read_boot_info() {
        exec::exec_user_program(path);
    }

    // No program specified — run selftests
    sumi_kernel::selftest::run_all();

    kprintln!("A");
    halt_forever()
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    // Write "PANIC\n" to debugcon so we can see panics in VM output.
    // Use raw byte writes — fmt machinery may not be safe in a panic handler.
    for &b in b"PANIC\n" {
        sumi_kernel::arch::debugcon_write_byte(b);
    }
    halt_forever()
}
