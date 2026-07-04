use core::alloc::{GlobalAlloc, Layout};
use core::panic::PanicInfo;

use sumi_kernel::{
    arch::{halt_forever, syscall},
    exec,
    fs::virtio_fs::VirtioFsClient,
    kprintln,
    sched,
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
            sumi_kernel::KERNEL_ALLOCATOR.free(paddr);
        }
    }
}

#[global_allocator]
static GLOBAL_ALLOC: GlobalKernelAlloc = GlobalKernelAlloc;

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    // Populate PER_CPU[0] and load IA32_GS_BASE so `syscall_entry` can
    // reach its per-CPU syscall stack via `gs:[8]`/`gs:[16]`. MUST precede
    // `syscall::init()` — the asm is live the moment `MSR_LSTAR` is
    // programmed inside `syscall::init()`.
    sched::init_for_cpu(0);
    syscall::init();

    // Phase 9: load TSS (for IST1 interrupt stack), IDT (exception and
    // timer/IPI vectors), and start the LAPIC periodic timer (~1 ms).
    // These must run BEFORE `sched::init_phase3_bsp()` so user-mode code
    // is always entered with a valid IDT and LAPIC timer running.
    sumi_kernel::arch::x86_64::tss::init_and_load(0);
    sumi_kernel::arch::x86_64::idt::init_and_load();
    sumi_kernel::arch::x86_64::lapic::init();

    sumi_kernel::FD_TABLE.lock().init_defaults();

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

    // Read boot info FIRST: this initializes time and RNG_SEED — both
    // of which are globals that APs may eventually observe. KERNEL_READY
    // must be set only after all such state is published.
    let user_program_path = exec::read_boot_info();

    // Phase 3: register the BSP main thread and idle thread before APs
    // start. Runs after PAGE_ALLOCATOR is usable (which it is from boot).
    // Must run BEFORE KERNEL_READY so APs see valid PER_CPU[0].idle_thread.
    sched::init_phase3_bsp();

    // Last step of BSP init: release any AP currently spinning in
    // `ap_main_rust`. MUST come after every global the APs could touch
    // is published (virtio FS, virtio console, FD defaults, allocators,
    // time, RNG seed, scheduler state). See docs/design/multithreading-v2.md
    // §15.2 risk 10.
    use core::sync::atomic::Ordering;
    sched::KERNEL_READY.store(true, Ordering::Release);

    if let Some(path) = user_program_path {
        exec::exec_user_program(path);
    }

    kprintln!("[kernel] no user program specified, halting");
    halt_forever()
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    // SAFETY: panic handler runs with the kernel in a known-broken state.
    // If the lock was held by some thread that panicked mid-print, force
    // release it so this panic message and any subsequent debugcon writes
    // from any CPU can proceed. spin::Mutex::force_unlock() is the
    // approved API for exactly this case.
    unsafe { sumi_kernel::arch::debugcon::force_unlock_debugcon() };

    // Write panic info to debugcon. Try fmt first for full message,
    // fall back to raw bytes if formatting fails.
    use core::fmt::Write;
    struct DebugconWriter;
    impl Write for DebugconWriter {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            for &b in s.as_bytes() {
                sumi_kernel::arch::debugcon_write_byte(b);
            }
            Ok(())
        }
    }
    let _ = writeln!(DebugconWriter, "PANIC: {}", info);
    halt_forever()
}
