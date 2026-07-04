//! AP main entry in Rust.
//!
//! Called from `ap_start_asm` with:
//!   - `rdi` = cpu_id (1..N) — System V ABI first integer arg.
//!   - `rsp` = top of `AP_BOOT_STACKS[cpu_id]` (16-byte aligned).
//!
//! Responsibilities:
//!   1. Program `PER_CPU[cpu_id]` and `IA32_GS_BASE` for this CPU.
//!   2. Program per-CPU syscall MSRs (LSTAR / STAR / SFMASK).
//!   3. Wait until the BSP publishes `KERNEL_READY`.
//!   4. Enter the Phase 1 idle loop (`sti; hlt`).
//!
//! Nothing before step 1 may touch `gs:` (GS_BASE is 0 on entry —
//! KVM zeros all MSRs on a fresh vCPU). `init_for_cpu` is therefore
//! the FIRST Rust statement.

use core::sync::atomic::Ordering;

use crate::kprintln;
use crate::sched::{self, KERNEL_READY};

/// AP entry point. Never returns.
///
/// `#[unsafe(no_mangle)]` + `extern "C"` ensures a stable ABI symbol
/// that `ap_start_asm` can `call`, even with LTO enabled.
#[unsafe(no_mangle)]
pub extern "C" fn ap_main_rust(cpu_id: u32) -> ! {
    // 1. Per-CPU kernel state + GS base. No `gs:` access is allowed
    // before this returns.
    sched::init_for_cpu(cpu_id);

    // 2. Per-CPU syscall MSRs. `syscall::init` only writes LSTAR /
    // STAR / SFMASK, which are per-CPU, so each AP must call it.
    // Idempotent with respect to the BSP.
    crate::arch::x86_64::syscall::init();

    // Phase 9: load per-CPU TSS (IST1 stack for interrupts), share the
    // BSP's IDT (same handlers, LAPIC vectors, same IDT base pointer),
    // and start this AP's own LAPIC periodic timer.
    crate::arch::x86_64::tss::init_and_load(cpu_id);
    crate::arch::x86_64::idt::load();
    crate::arch::x86_64::lapic::init();

    // Announce liveness BEFORE the KERNEL_READY spin so the smoke test
    // can verify all N CPUs reached `ap_main_rust`. This is safe DESPITE
    // running before KERNEL_READY only because `kprintln!` writes to the
    // debugcon I/O port (a hypervisor-side pin) and a static spin lock —
    // neither of which is touched by BSP init. Any future routing of
    // `kprintln!` through virtio-console MUST move this print below the
    // KERNEL_READY wait.
    kprintln!("[ap] cpu {} online", cpu_id);

    // 4. Spin until the BSP has finished initialising every global
    // state that any AP could touch (virtio FS, console, FD table,
    // allocators, user program image). Acquire pairs with the
    // Release store in `kernel_main::_start`.
    while !KERNEL_READY.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }

    // 5. Phase 3 idle loop. Registers the AP's idle thread (reusing the
    // AP boot stack) and enters idle_loop(), which parks the vCPU via
    // `hlt` until the scheduler has work. Never returns.
    crate::sched::init_phase3_ap(cpu_id);
}
