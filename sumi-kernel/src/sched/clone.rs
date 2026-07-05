//! Child-thread construction for `sys_clone` / `sys_clone3`.
//!
//! See `docs/design/multithreading-v2.md`.
//!
//! `sys_clone` / `sys_clone3` in `syscall/handlers/clone.rs` are the callers;
//! this module owns the low-level kernel-stack-frame construction and the
//! trampoline that transitions a freshly-created thread into user mode for the
//! first time.

use crate::sched::thread::{Thread, Tid};
use alloc::sync::Arc;
use sumi_abi::address::{DirectMap, VirtualAddr};

/// Kernel-stack state needed to enter user mode for the first time.
///
/// Written by `clone_create_user_thread` at the top of a fresh per-thread
/// kernel stack. Consumed once by `thread_entry_trampoline` on first
/// schedule-in.
///
/// Layout at the top of the kernel stack (low→high addresses, 10 qwords):
///
///   slot 0 (ctx.rsp): trampoline_addr  — __switch_to_asm pops via ret
///   slot 1          : arg0 (rdi)
///   slot 2          : arg1 (rsi)
///   slot 3          : arg2 (rdx)       ← clone3 wrapper stores thread fn here
///   slot 4          : arg3 (r10)
///   slot 5          : arg4 (r8)        ← clone3 wrapper stores thread arg here
///   slot 6          : arg5 (r9)
///   slot 7          : user_rip
///   slot 8          : user_rflags
///   slot 9          : user_rsp         — highest slot
///
/// Slots 1-6 preserve the parent's caller-saved argument registers so that
/// glibc's clone3 child stub can read `rdx` (thread fn) and `r8` (thread arg)
/// after the trampoline restores them. Traditional `clone` callers that push
/// fn/arg onto the child stack also work correctly because the trampoline
/// restores the original values, which are whatever the parent had at syscall
/// time.
#[repr(C)]
pub struct InitialFrame {
    /// arg0 (rdi) at the time of the parent's clone/clone3 syscall.
    pub arg0: u64,
    /// arg1 (rsi) at the time of the parent's clone/clone3 syscall.
    pub arg1: u64,
    /// arg2 (rdx) at the time of the parent's clone/clone3 syscall.
    /// glibc clone3 wrapper stores the thread function in rdx.
    pub arg2: u64,
    /// arg3 (r10) at the time of the parent's clone/clone3 syscall.
    pub arg3: u64,
    /// arg4 (r8) at the time of the parent's clone/clone3 syscall.
    /// glibc clone3 wrapper stores the thread arg in r8 (moved from rcx).
    pub arg4: u64,
    /// arg5 (r9) at the time of the parent's clone/clone3 syscall.
    pub arg5: u64,
    /// Return address for the child (= caller_rip = parent's rcx at syscall).
    pub user_rip: u64,
    /// User RFLAGS to restore in the child (= parent's r11 at syscall).
    pub user_rflags: u64,
    /// Child user-space stack pointer (child_stack arg to clone/clone3).
    pub user_rsp: u64,
}

const _: () = assert!(core::mem::size_of::<InitialFrame>() == 72);

/// Errors returned by `clone_create_user_thread`.
#[derive(Debug, Clone, Copy)]
pub enum CloneError {
    OutOfMemory,
}

/// Allocate a per-thread kernel stack and build a `Thread` whose first
/// `__switch_to_asm` restore will land in `thread_entry_trampoline` and
/// transfer to user mode at `frame.user_rip` with `rax = 0`.
///
/// The caller-saved registers in `frame` (arg0–arg5) are restored in the
/// child before it jumps to user code, preserving the register state that
/// glibc's clone3 child stub expects (thread fn in rdx, thread arg in r8).
///
/// The caller is responsible for allocating a TID, assigning it, pushing
/// the returned Arc to the registry and runqueue, and setting state to
/// Runnable.
///
/// Generic over `DM: DirectMap` (rather than hard-coded to the global
/// `crate::KERNEL_ALLOCATOR`) so this function is directly host-testable
/// with `crate::memory::test_utils::TestDirectMap` — same seam as
/// `RootPageTable`/`KernelAllocator`. Production calls it with
/// `crate::KERNEL_ALLOCATOR`.
// Each parameter is an independent, already-validated piece of the new
// Thread's state (the caller — `do_clone` — is where they'd naturally
// group, but it validates and forwards them one at a time); no subset of
// them shares a lifecycle that would justify a bundling struct.
#[allow(clippy::too_many_arguments)]
pub fn clone_create_user_thread<DM: DirectMap>(
    tid: Tid,
    tgid: Tid,
    frame: InitialFrame,
    fs_base: u64,
    user_stack_base: VirtualAddr,
    user_stack_size: usize,
    clear_child_tid: u64,
    kalloc: &crate::memory::alloc::kmalloc::KernelAllocator<'_, DM>,
) -> Result<Arc<Thread>, CloneError> {
    use core::cell::UnsafeCell;
    use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64};

    use sumi_abi::arch::layout::KERNEL_STACK_SIZE;

    use crate::sched::thread::{FxsaveArea, RunLink, ThreadContext, ThreadState, WaitLink};

    // 1. Allocate a fresh compact kernel stack. User stacks are provided by
    // pthread/mmap; kernel stacks only need enough room for syscall/trap frames
    // and scheduler handoff state.
    let stack_phys = kalloc
        .calloc(KERNEL_STACK_SIZE)
        .map_err(|_| CloneError::OutOfMemory)?;
    let stack_top_virt = stack_phys
        .add(KERNEL_STACK_SIZE)
        .to_virtual(kalloc.direct_map());

    // 2. Write the initial frame at the top of the kernel stack.
    //
    // Layout (low→high, 10 × 8 = 80 bytes below stack top):
    //   slot 0 (ctx.rsp) : trampoline_addr   — __switch_to_asm pops via ret
    //   slot 1           : arg0 (rdi)
    //   slot 2           : arg1 (rsi)
    //   slot 3           : arg2 (rdx)
    //   slot 4           : arg3 (r10)
    //   slot 5           : arg4 (r8)
    //   slot 6           : arg5 (r9)
    //   slot 7           : user_rip
    //   slot 8           : user_rflags
    //   slot 9           : user_rsp          — highest slot
    //
    // After `ret` in __switch_to_asm, rsp points at slot 1 (arg0).
    //
    // Note: __switch_to_asm does `push qword ptr [rsi+0x38]; popfq` which
    // temporarily writes at ctx.rsp - 8 = top - 88 before popfq restores rsp.
    // That address is well inside the allocated kernel stack.
    let top = stack_top_virt.as_usize();
    debug_assert_eq!(top % 16, 0, "kernel stack top must be 16-byte aligned");

    // SAFETY: Fresh kernel stack allocation, exclusively owned by this thread
    // builder. All slots are 8-aligned and lie within the stack.
    unsafe {
        let slot0 = (top - 80) as *mut u64; // ctx.rsp: trampoline addr
        let slot1 = (top - 72) as *mut u64; // arg0 (rdi)
        let slot2 = (top - 64) as *mut u64; // arg1 (rsi)
        let slot3 = (top - 56) as *mut u64; // arg2 (rdx) — clone3 thread fn
        let slot4 = (top - 48) as *mut u64; // arg3 (r10)
        let slot5 = (top - 40) as *mut u64; // arg4 (r8)  — clone3 thread arg
        let slot6 = (top - 32) as *mut u64; // arg5 (r9)
        let slot7 = (top - 24) as *mut u64; // user_rip
        let slot8 = (top - 16) as *mut u64; // user_rflags
        let slot9 = (top - 8) as *mut u64; // user_rsp

        core::ptr::write(slot0, thread_entry_trampoline as *const () as u64);
        core::ptr::write(slot1, frame.arg0);
        core::ptr::write(slot2, frame.arg1);
        core::ptr::write(slot3, frame.arg2);
        core::ptr::write(slot4, frame.arg3);
        core::ptr::write(slot5, frame.arg4);
        core::ptr::write(slot6, frame.arg5);
        core::ptr::write(slot7, frame.user_rip);
        core::ptr::write(slot8, frame.user_rflags);
        core::ptr::write(slot9, frame.user_rsp);
    }

    // 3. Construct the Arc<Thread>.
    let rsp = (top - 80) as u64; // ctx.rsp points at slot0
    let t = Arc::new(Thread {
        tid,
        tgid,
        state: AtomicU32::new(ThreadState::New as u32),
        exit_code: AtomicI32::new(0),
        ctx: UnsafeCell::new(ThreadContext {
            rsp,
            rbp: 0,
            rbx: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rflags: 0x202, // reserved bit 1 + IF
            fxsave_area: FxsaveArea::new(),
        }),
        kernel_stack_top: stack_top_virt,
        kernel_stack_phys: stack_phys,
        kernel_stack_size: KERNEL_STACK_SIZE,
        kernel_stack_freeable: true,
        user_stack_base,
        user_stack_size,
        fs_base: AtomicU64::new(fs_base),
        clear_child_tid: AtomicU64::new(clear_child_tid),
        robust_list_head: AtomicU64::new(0),
        cpu: AtomicU32::new(u32::MAX),
        on_cpu: AtomicBool::new(false),
        run_link: RunLink::new(),
        wait_link: WaitLink::new(),
        // entry_fn / entry_arg are kthread-only; user threads enter via
        // the trampoline and never read these fields.
        entry_fn: AtomicU64::new(0),
        entry_arg: AtomicU64::new(0),
    });
    Ok(t)
}

/// First code executed on a brand-new user thread.
///
/// The scheduler lands here via `__switch_to_asm` → `ret`. At entry, rsp
/// points at slot 1 (arg0). The trampoline:
///
/// 1. Pops arg0-arg5 into rdi, rsi, rdx, r10, r8, r9 (restoring the
///    parent's caller-saved registers so glibc's clone3 stub can read
///    rdx = thread fn and r8 = thread arg).
/// 2. Saves user_rip in r11 (scratch).
/// 3. Pushes user_rflags and restores RFLAGS via popfq.
/// 4. Switches rsp to user_rsp.
/// 5. Zeroes rax (child sees clone/clone3 return value = 0).
/// 6. Jumps to user_rip.
///
/// # Safety
///
/// Invoked only by `__switch_to_asm` when scheduling-in a child built by
/// `clone_create_user_thread`. The stack layout at entry must be exactly
/// the one written by that function.
#[cfg(not(test))]
#[unsafe(naked)]
unsafe extern "C" fn thread_entry_trampoline() -> ! {
    core::arch::naked_asm!(
        // After __switch_to_asm's `ret`, rsp → slot1 (arg0/rdi).
        //
        // Restore the parent's caller-saved argument registers. This is
        // required for glibc's clone3 child stub which reads rdx (thread fn)
        // and r8 (thread arg) without re-loading them from the child stack.
        "pop rdi", // slot1: arg0
        "pop rsi", // slot2: arg1
        "pop rdx", // slot3: arg2 — clone3 thread fn
        "pop r10", // slot4: arg3
        "pop r8",  // slot5: arg4 — clone3 thread arg
        "pop r9",  // slot6: arg5
        // rsp → slot7 (user_rip).
        //
        // Use r11 as scratch for user_rip. r11 was clobbered by the syscall
        // instruction in the parent and holds no meaningful value in the child.
        "mov r11, [rsp]", // r11 = user_rip
        // Zero rax BEFORE popfq. xor modifies ZF/SF/PF/CF/OF, which would
        // override the user_rflags value we are about to restore via popfq.
        "xor eax, eax",
        // Push user_rflags (slot8) so popfq can restore them.
        // push does not modify RFLAGS, so the xor result is still in flags
        // but that doesn't matter — popfq is the last flag-touching operation.
        //
        // IF must NOT become 1 via this `popfq`: unlike `sti`, `popfq`
        // setting IF has no interrupt-shadow — a timer tick can be
        // recognized at the very next instruction boundary, i.e. before
        // `mov rsp, [rsp+16]` runs, which would deliver the timer ISR
        // with RSP still pointing at this (about to be freed) kernel-stack
        // slot data (confirmed empirically under KVM in the analogous
        // syscall_entry tail). Clear IF in the pushed copy before `popfq`,
        // complete the RSP switch, then `sti` — its shadow is real and
        // covers exactly the `jmp` that follows.
        "push qword ptr [rsp + 8]", // push user_rflags (slot8 = [rsp+8] before push)
        "and dword ptr [rsp], 0xFFFFFDFF", // clear IF (bit 9) in the pushed copy
        "popfq",                    // restore user RFLAGS; IF stays 0
        // Switch to user stack. user_rsp is at [rsp + 16]:
        //   rsp+0  = slot7 (user_rip)   — rsp is back here after push+popfq balanced
        //   rsp+8  = slot8 (user_rflags, already consumed)
        //   rsp+16 = slot9 (user_rsp)
        "mov rsp, [rsp + 16]", // rsp = user_rsp — safe, IF is still 0
        "sti",                 // shadow covers the next instruction (`jmp r11`)
        // Jump to user code at the instruction after the parent's syscall.
        "jmp r11",
    )
}

/// Host stand-in for the naked-asm trampoline above: `clone_create_user_thread`
/// needs *some* function address to write into the frame's `ctx.rsp` slot so
/// its layout is checkable under test. This is
/// never actually invoked under test — there is no real `__switch_to_asm`
/// to jump into it — so it exists purely as a valid, distinct symbol.
#[cfg(test)]
extern "C" fn thread_entry_trampoline() -> ! {
    unreachable!("thread_entry_trampoline stub invoked under host test")
}
