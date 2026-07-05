//! Kernel-thread construction: the BSP main/idle threads, the AP idle
//! thread, and the trampolines that transition a freshly-built kernel
//! thread into its entry function. Split out of `thread.rs` (which owns
//! the `Thread`/`ThreadContext` layout types) to keep both files under the
//! 500-line cap.

use alloc::sync::Arc;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64};

use sumi_abi::address::{DirectMap, PhysicalAddr, VirtualAddr};

use super::thread::{FxsaveArea, RunLink, Thread, ThreadContext, ThreadState, Tid, WaitLink};

/// Build a `Thread` for a kernel thread, with a `kthread_trampoline` return
/// address written at the top of its stack so the first `__switch_to_asm`
/// restore jumps there. Shared by the BSP idle thread constructor (the only
/// remaining caller now that `kthread_spawn` is gone).
///
/// `stack_top_virt` must already be a writable virtual address — real
/// guest memory in production, or a `TestDirectMap`-backed host buffer in
/// tests (see `build_idle_thread`'s `dm` parameter). This function itself
/// has no arch dependency, so it needs no `cfg(test)` split.
fn build_kthread_arc(
    entry_fn: extern "C" fn(u64) -> !,
    arg: u64,
    tid: Tid,
    stack_phys: PhysicalAddr,
    stack_top_virt: VirtualAddr,
    stack_size: usize,
) -> Thread {
    debug_assert_eq!(
        stack_top_virt.as_usize() % 16,
        0,
        "stack top must be 16-aligned"
    );

    // Place the trampoline return-address slot at a 16-aligned address.
    // After `ret` pops 8 bytes, the trampoline executes with
    // (rsp + 8) % 16 == 0 as SysV AMD64 requires (System V AMD64 ABI §3.2.2).
    let top = stack_top_virt.as_usize() as u64 & !0xF;
    let slot = top - 16;
    debug_assert_eq!(slot % 16, 0);
    // SAFETY: fresh page, unique owner; slot is inside the page.
    unsafe {
        core::ptr::write(slot as *mut u64, kthread_trampoline as *const () as u64);
    }

    Thread {
        tid,
        tgid: tid,
        state: AtomicU32::new(ThreadState::Runnable as u32),
        exit_code: AtomicI32::new(0),
        ctx: UnsafeCell::new(ThreadContext {
            rsp: slot,
            rbp: 0,
            rbx: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            // EFLAGS: bit 1 (reserved, always 1) | bit 9 (IF=1). __switch_to_asm
            // forces IF back to 0 for a thread's first switch-in regardless
            // of this value; this only matters for the reserved bit.
            rflags: 0x202,
            fxsave_area: FxsaveArea::new(),
        }),
        kernel_stack_top: stack_top_virt,
        kernel_stack_phys: stack_phys,
        kernel_stack_size: stack_size,
        kernel_stack_freeable: false,
        user_stack_base: VirtualAddr::new(0),
        user_stack_size: 0,
        fs_base: AtomicU64::new(0),
        clear_child_tid: AtomicU64::new(0),
        robust_list_head: AtomicU64::new(0),
        cpu: AtomicU32::new(u32::MAX),
        on_cpu: AtomicBool::new(false),
        run_link: RunLink::new(),
        wait_link: WaitLink::new(),
        entry_fn: AtomicU64::new(entry_fn as *const () as u64),
        entry_arg: AtomicU64::new(arg),
    }
}

/// Build the BSP "main thread" that wraps the existing boot stack.
///
/// The BSP is already executing on this stack when we construct the Thread,
/// so `ctx` is left zeroed — it will be populated the first time `schedule()`
/// switches away from this thread (saving live RSP/RBP/etc. into `ctx`).
///
/// Generic over `DM: DirectMap` (rather than hard-coded to the global
/// `crate::KERNEL_ALLOCATOR`) purely so this is directly host-testable with
/// `TestDirectMap` — this function never dereferences `kernel_stack_top`
/// itself, only stores the translated value, so the choice of `dm` only
/// matters to whoever later reads that field.
pub(super) fn build_current_main_thread<DM: DirectMap>(dm: &DM) -> Arc<Thread> {
    use sumi_abi::arch::layout::{KERNEL_STACK, KERNEL_STACK_SIZE};

    Arc::new(Thread {
        tid: Tid(1),
        tgid: Tid(1),
        state: AtomicU32::new(ThreadState::Running as u32),
        exit_code: AtomicI32::new(0),
        // ctx will be populated by the first __switch_to_asm save.
        ctx: UnsafeCell::new(ThreadContext {
            rsp: 0,
            rbp: 0,
            rbx: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rflags: 0,
            fxsave_area: FxsaveArea::new(),
        }),
        // KERNEL_STACK is the *top* of the BSP boot stack (post align-up).
        // The stack spans [KERNEL_STACK - KERNEL_STACK_SIZE, KERNEL_STACK).
        //
        // INVARIANT: the BSP boot stack (KERNEL_STACK, 32 KB) must be IDLE
        // at the moment of the first user syscall. The syscall_entry asm
        // resets RSP to this top on every syscall entry; if any kernel code
        // were still live in frames below the top at that moment, the next
        // syscall would clobber it.
        //
        // Today this is upheld because `_start` runs:
        //   sched::init_phase3_bsp() -> KERNEL_READY.store(true)
        //     -> exec::exec_user_program() -> jump_to_user_asm
        // which abandons the BSP boot stack before dropping to user mode.
        // Any future code that runs kernel work on the BSP boot stack while
        // expecting to take a syscall later will silently corrupt itself.
        kernel_stack_top: KERNEL_STACK.to_virtual(dm),
        kernel_stack_phys: PhysicalAddr::new(KERNEL_STACK.as_usize() - KERNEL_STACK_SIZE),
        kernel_stack_size: KERNEL_STACK_SIZE,
        kernel_stack_freeable: false,
        user_stack_base: VirtualAddr::new(0),
        user_stack_size: 0,
        fs_base: AtomicU64::new(0),
        clear_child_tid: AtomicU64::new(0),
        robust_list_head: AtomicU64::new(0),
        cpu: AtomicU32::new(0),        // BSP is always cpu 0
        on_cpu: AtomicBool::new(true), // already executing
        run_link: RunLink::new(),
        wait_link: WaitLink::new(),
        entry_fn: AtomicU64::new(0),
        entry_arg: AtomicU64::new(0),
    })
}

/// Build the BSP idle thread. Allocates a fresh 2 MB page as the stack and
/// sets up a trampoline frame for `idle_loop_entry`.
///
/// Generic over `DM: DirectMap` for the same reason as
/// `build_current_main_thread` — here the direct map also matters for
/// safety, not just data, since the trampoline return address is actually
/// written through the translated address (`build_kthread_arc`).
pub(super) fn build_idle_thread<DM: DirectMap>(dm: &DM) -> Arc<Thread> {
    use sumi_abi::arch::layout::PAGE_SIZE;

    let stack_phys = match crate::PAGE_ALLOCATOR.alloc(1) {
        Ok(p) => p,
        Err(_) => {
            crate::kprintln!("[sched] init_phase3_bsp: out of memory allocating idle stack");
            shutdown_out_of_memory();
        }
    };
    let stack_top_virt = stack_phys.add(PAGE_SIZE).to_virtual(dm);

    let tid = super::registry::alloc_tid();
    Arc::new(build_kthread_arc(
        idle_loop_entry,
        0,
        tid,
        stack_phys,
        stack_top_virt,
        PAGE_SIZE,
    ))
}

/// Out-of-memory leaf for `build_idle_thread`: real hypercall shutdown in
/// production (never returns); host stand-in panics instead, since there is
/// no VM to terminate under `cargo test`.
#[cfg(not(test))]
fn shutdown_out_of_memory() -> ! {
    crate::arch::x86_64::hypercall::shutdown(1)
}

#[cfg(test)]
fn shutdown_out_of_memory() -> ! {
    panic!("build_idle_thread: out of memory (host test build)")
}

/// Build an idle thread for an AP that reuses its `AP_BOOT_STACKS[cpu_id]`
/// slot. The AP is already running on that stack, so `ctx.rsp = 0` — the
/// first `schedule()` call on the AP will save the live RSP into `ctx`.
#[cfg(not(test))]
pub(super) fn build_idle_thread_for_ap_reusing_boot_stack(cpu_id: u32) -> Arc<Thread> {
    use crate::sched::percpu::{AP_BOOT_STACK_SIZE, AP_BOOT_STACKS};

    // cpu_id < MAX_VCPUS (checked by ap_main_rust). Pointer arithmetic
    // avoids indexing the static array directly, which would trigger a
    // bounds-check in debug builds for a raw addr-of.
    let base =
        core::ptr::addr_of!(AP_BOOT_STACKS) as u64 + (cpu_id as u64) * AP_BOOT_STACK_SIZE as u64;
    let stack_top_virt: u64 = base + AP_BOOT_STACK_SIZE as u64;

    let tid = super::registry::alloc_tid();
    Arc::new(Thread {
        tid,
        tgid: tid,
        state: AtomicU32::new(ThreadState::Runnable as u32),
        exit_code: AtomicI32::new(0),
        // rsp = 0: the AP is live on this stack; schedule() will save
        // the real RSP on first switch-away.
        ctx: UnsafeCell::new(ThreadContext {
            rsp: 0,
            rbp: 0,
            rbx: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rflags: 0x202,
            fxsave_area: FxsaveArea::new(),
        }),
        kernel_stack_top: VirtualAddr::new(stack_top_virt as usize),
        // AP boot stacks are not physical-memory objects tracked by palloc;
        // use zero as the physical address sentinel.
        kernel_stack_phys: PhysicalAddr::new(0),
        kernel_stack_size: AP_BOOT_STACK_SIZE,
        kernel_stack_freeable: false,
        user_stack_base: VirtualAddr::new(0),
        user_stack_size: 0,
        fs_base: AtomicU64::new(0),
        clear_child_tid: AtomicU64::new(0),
        robust_list_head: AtomicU64::new(0),
        cpu: AtomicU32::new(cpu_id),
        on_cpu: AtomicBool::new(true), // already executing
        run_link: RunLink::new(),
        wait_link: WaitLink::new(),
        entry_fn: AtomicU64::new(0),
        entry_arg: AtomicU64::new(0),
    })
}

/// Entry point for the idle thread. `extern "C" fn(u64) -> !` matches the
/// signature expected by `kthread_trampoline`. The `_arg` is unused.
#[cfg(not(test))]
extern "C" fn idle_loop_entry(_arg: u64) -> ! {
    super::idle_loop()
}

/// Host stand-in: `build_idle_thread` needs some function address to write
/// into the trampoline frame's return slot. Never
/// actually invoked under test — there is no real `__switch_to_asm` to jump
/// into it, and `idle_loop` itself stays arch-gated (real `cli`/`sti;hlt`).
#[cfg(test)]
extern "C" fn idle_loop_entry(_arg: u64) -> ! {
    unreachable!("idle_loop_entry stub invoked under host test")
}

/// First code executed on a fresh kernel thread's stack. `__switch_to_asm`
/// returns into this function the very first time a kthread is scheduled.
///
/// Reads `entry_fn` / `entry_arg` from `current_thread()` (set by
/// `schedule()` just before the switch) and tail-calls them.
#[cfg(not(test))]
extern "C" fn kthread_trampoline() -> ! {
    use core::sync::atomic::Ordering;

    // SAFETY: schedule() stores the new thread pointer in current_thread
    // before calling __switch_to_asm, so by the time we execute here the
    // pointer is valid and stable.
    let t = unsafe {
        let pc = super::percpu::this_cpu();
        let ptr = pc.current_thread.load(Ordering::Relaxed);
        &*ptr
    };
    // SAFETY: entry_fn was written as a valid `extern "C" fn(u64) -> !`
    // pointer by build_kthread_arc and is never mutated.
    let entry: extern "C" fn(u64) -> ! =
        unsafe { core::mem::transmute(t.entry_fn.load(Ordering::Relaxed)) };
    let arg = t.entry_arg.load(Ordering::Relaxed);
    entry(arg)
}

/// Host stand-in: `build_kthread_arc` needs some function address to write
/// into the trampoline frame's return slot. Never
/// actually invoked under test — reached only via a real `__switch_to_asm`
/// return, which does not exist on the host.
#[cfg(test)]
extern "C" fn kthread_trampoline() -> ! {
    unreachable!("kthread_trampoline stub invoked under host test")
}
