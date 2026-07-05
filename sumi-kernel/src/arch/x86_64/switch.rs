//! Low-level context switch.
//!
//! See `docs/design/multithreading-v2.md` for the scheduler invariants.

use core::arch::naked_asm;

use crate::sched::thread::ThreadContext;

/// Save the callee-saved registers of `prev` into `*prev_ctx` and restore
/// them from `*next_ctx`, transferring execution to the thread whose return
/// address sits on `next_ctx.rsp`.
///
/// Also eager `fxsave`/`fxrstor`s the legacy x87/MMX/SSE register file
/// alongside the GP context, and clears `*prev_on_cpu` once `prev`'s
/// context is fully captured and we've committed to running as `next` — the
/// signal that it is now safe for another CPU to run or reap `prev`.
///
/// Loads IA32_FS_BASE from `next_fs_base` (3rd arg) immediately before
/// `ret`, so the new thread observes the correct fs_base before the
/// trampoline or any user code runs.
///
/// # Interrupt discipline
///
/// Called with interrupts already disabled (every kernel entry point clears
/// IF: `SFMASK` on `syscall`, interrupt gates on ISRs). `next_ctx.rflags`
/// may still have IF=1 baked in for a thread that has never been switched
/// away from yet (kthreads/clone children are constructed with
/// `rflags = 0x202`). We must not let `popfq` set IF=1 here: unlike `sti`,
/// `popfq` has no interrupt-shadow — a pending timer tick can be recognized
/// at the very next instruction boundary (confirmed empirically under KVM:
/// an earlier version of this function tried `popfq` then `cli`, and a
/// timer tick landing in that exact gap re-entered `schedule_preempt` with
/// RSP pointing at `next`'s still-being-restored kernel stack, corrupting
/// it). So the IF bit is cleared in the *pushed copy* before `popfq` runs,
/// meaning IF simply never becomes 1 here — no race window at all. IF
/// stays 0 across this function's fs_base-loading tail and into
/// `kthread_trampoline` / `thread_entry_trampoline`, both of which still
/// read live per-thread stack data at their start. Interrupts are
/// re-enabled only at well-defined, stack-safe points downstream: the
/// manual restore in `isr_timer`/`isr_ipi`, the `syscall_entry` tail, or
/// `thread_entry_trampoline` (each via an explicit `sti` once RSP is the
/// user stack, or an explicit `sti` in a kthread body like `idle_loop`).
///
/// # Safety
///
/// - `prev_ctx` and `next_ctx` must be valid, non-null `*mut ThreadContext`,
///   each pointing at a distinct `Thread.ctx` field.
/// - `prev_on_cpu` must be a valid, non-null `*const AtomicBool` pointing at
///   the same `Thread`'s `on_cpu` field as `prev_ctx`.
/// - `next_ctx.rsp` must point into a valid kernel stack. For a newly spawned
///   kthread the topmost qword must be the address of `kthread_trampoline`.
/// - `next_fs_base` must be canonical (bits 47..63 zero) or zero — non-canonical
///   values cause `#GP` on `wrmsr`. Caller (`schedule()`) ensures this because
///   the value flows from `arch_prctl(ARCH_SET_FS)` (handler validates) or
///   `clone(..., tls)` where tls is a user pointer.
/// - The caller must not hold any non-irqsave `spin::Mutex` across this call.
#[unsafe(naked)]
pub unsafe extern "C" fn __switch_to_asm(
    prev_ctx: *mut ThreadContext,
    next_ctx: *mut ThreadContext,
    next_fs_base: u64,
    prev_on_cpu: *const core::sync::atomic::AtomicBool,
) {
    naked_asm!(
        // Save prev callee-saved into *prev_ctx (rdi).
        "mov [rdi + 0x00], rsp",
        "mov [rdi + 0x08], rbp",
        "mov [rdi + 0x10], rbx",
        "mov [rdi + 0x18], r12",
        "mov [rdi + 0x20], r13",
        "mov [rdi + 0x28], r14",
        "mov [rdi + 0x30], r15",
        "pushfq",
        "pop qword ptr [rdi + 0x38]",
        "fxsave [rdi + 0x40]", // Save prev's legacy x87/MMX/SSE state.
        // Restore next callee-saved from *next_ctx (rsi).
        "mov rsp, [rsi + 0x00]",
        "mov rbp, [rsi + 0x08]",
        "mov rbx, [rsi + 0x10]",
        "mov r12, [rsi + 0x18]",
        "mov r13, [rsi + 0x20]",
        "mov r14, [rsi + 0x28]",
        "mov r15, [rsi + 0x30]",
        // Note: this temporarily writes at rsp-8 (i.e. ctx.rsp - 8). For a
        // brand-new clone child whose ctx.rsp = stack_top - 80, that write
        // lands at stack_top - 88, which must lie inside the allocated kernel
        // stack page. clone.rs::clone_create_user_thread relies on this.
        "push qword ptr [rsi + 0x38]",
        "and dword ptr [rsp], 0xFFFFFDFF", // Clear IF (bit 9) in the pushed copy; see doc comment.
        "popfq",
        "fxrstor [rsi + 0x40]", // Restore next's legacy x87/MMX/SSE state.
        // prev's context (GP + FPU) is now fully saved and we've
        // switched onto next's stack, so it is safe for another CPU to run
        // or reap prev. rcx = prev_on_cpu (4th SysV arg); nothing above
        // touches rcx. A plain store is enough for release semantics: x86
        // stores are never reordered with later stores (TSO).
        "mov byte ptr [rcx], 0",
        // Load IA32_FS_BASE = rdx (next_fs_base, 3rd SysV arg).
        // wrmsr takes MSR in ECX, value in EDX:EAX. We destructively read rdx.
        // rax/rcx/rdx are caller-saved per SysV AMD64; clobbering is legal at
        // a function-call boundary (rcx's prev_on_cpu value is no longer
        // needed after the store above).
        "mov rax, rdx",        // rax = fs_base (low 32 used by wrmsr)
        "shr rdx, 32",         // edx = high 32 of original fs_base
        "mov ecx, 0xc0000100", // IA32_FS_BASE
        "wrmsr",
        // Transfer execution: the return address on next's stack is either
        // the instruction after the `call __switch_to_asm` in schedule()
        // (for a previously-switched-away thread) or `kthread_trampoline`
        // (for a freshly-spawned kthread).
        "ret",
    )
}
