//! Interrupt and exception handlers for Phase 9.
//!
//! ## Exception handlers
//! Naked trampolines save GP regs, call a Rust handler that prints
//! diagnostics and shuts down, then never return (the Rust handler
//! calls `hypercall::shutdown`).
//!
//! ## Timer ISR (vector 0x40)
//! Fires at ~1 ms from the LAPIC (periodic, configured in `lapic::init`).
//! Entry is always from user mode (IF=1) via IST1. The trampoline:
//!   1. Saves 15 GP regs on the IST1 stack.
//!   2. Increments `preempt_count` (via gs:).
//!   3. Calls `timer_interrupt()` (sends EOI, reloads CR3 if this CPU's TLB
//!      is stale, and sets need_resched).
//!   4. Decrements `preempt_count`.
//!   5. If `preempt_count == 0` and `need_resched`, takes the preemption
//!      path: copies the 20-qword frame (15 GP + 5 CPU-hardware) from the
//!      IST1 stack to the current thread's kernel stack, switches RSP, and
//!      calls `schedule_preempt()`. On return, falls through to `.restore`.
//!   6. Restores 15 GP regs, then an explicit RSP-switch + `ret` back to
//!      user mode — NOT `iretq`. F9 in `docs/design/multithreading-fixes.md`
//!      claimed a plain `iretq` would work here ("64-bit IRET always pops
//!      SS:RSP"), but that is only half true: 64-bit mode always *pushes*
//!      SS:RSP on interrupt entry (for IST), but IRET's pop of SS:RSP is
//!      still conditional on the popped CS.RPL differing from the current
//!      CPL. sumi runs every ring at CPL0 (`sregs.cs.dpl = 0` for both
//!      "kernel" and "user" code — see `sumi-vm/.../kvm/mod.rs`), so that
//!      condition is never true and a plain `iretq` silently leaves RSP
//!      wrong. Confirmed empirically: switching this to `iretq` reproduced
//!      exactly this failure (RIP correct, RSP left pointing into the ISR's
//!      own frame) under KVM. The manual restore below is therefore kept.
//!
//! ## IPI ISR (vector 0x41)
//! Used only to wake a CPU from `hlt` so it re-checks its runqueue. The
//! trampoline simply sends EOI and `iretq`; no preemption logic is needed.

use core::arch::naked_asm;
use core::sync::atomic::Ordering;

use crate::sched::percpu;
use crate::sched::thread::KERNEL_STACK_TOP_OFFSET;

// ---- Rust-side handlers ------------------------------------------------

/// Called by the timer ISR trampoline with interrupts disabled (interrupt
/// gate clears IF on entry). Sends EOI, reloads CR3 if this CPU's TLB is
/// stale (F10), and marks the current CPU as needing a reschedule.
#[unsafe(no_mangle)]
extern "C" fn timer_interrupt() {
    super::lapic::eoi();
    let cpu = percpu::this_cpu();
    // F10: without this, a thread preempted here resumes with a stale TLB
    // (from a peer CPU's mprotect/munmap) until its next syscall — the
    // syscall postamble already does the same check, but a preempted
    // thread may run for a long time before its next syscall.
    cpu.reload_tlb_if_stale();
    // Relaxed store is sufficient: the ISR trampoline reads need_resched
    // immediately after decrementing preempt_count on the same CPU with
    // interrupts still disabled, so there is no ordering hazard here.
    cpu.need_resched.store(true, Ordering::Relaxed);
}

/// Called by the IPI trampoline to acknowledge the interrupt and wake
/// the CPU so it can re-check its runqueue in `idle_loop`.
#[unsafe(no_mangle)]
extern "C" fn ipi_interrupt() {
    super::lapic::eoi();
}

/// Preempt the current thread: put it back on the runqueue and switch.
///
/// Called from the timer ISR trampoline on the thread's own kernel stack
/// (the trampoline has already copied the interrupt frame there). On
/// return, the trampoline restores the frame and `iretq`s to user mode.
#[unsafe(no_mangle)]
extern "C" fn schedule_preempt() {
    use crate::sched::{thread::ThreadState, percpu as pc};
    use core::sync::atomic::Ordering;

    let cpu = pc::this_cpu();
    // Clear need_resched before re-enqueuing so that a concurrent wake
    // that fires between now and the schedule() call sets it again and is
    // not lost.
    cpu.need_resched.store(false, Ordering::Release);

    let current_ptr = cpu.current_thread.load(Ordering::Relaxed);
    let idle_ptr    = cpu.idle_thread.load(Ordering::Relaxed);

    // Only push to the runqueue if we are not the idle thread.
    if !core::ptr::eq(current_ptr, idle_ptr) {
        // SAFETY: current_ptr is valid — set by schedule() and init_phase3_bsp/ap,
        // non-null after init_phase3, lives for the kernel's lifetime.
        let current = unsafe { &*current_ptr };
        // Transition Running → Runnable so schedule() won't demote us again.
        let _ = current.state.compare_exchange(
            ThreadState::Running as u32,
            ThreadState::Runnable as u32,
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
        cpu.runqueue.push(current);
    }

    crate::sched::schedule();
}

// ---- Exception handlers -------------------------------------------------

/// Common handler for CPU exceptions. Prints the vector and a hex dump
/// of the saved GP regs (passed as a C array on the stack), then shuts
/// down. Never returns.
#[unsafe(no_mangle)]
extern "C" fn exception_handler(vector: u64, error_code: u64, rip: u64, rsp: u64) -> ! {
    crate::kprintln!(
        "[exception] vector={:#x} error_code={:#x} rip={:#x} rsp={:#x}",
        vector, error_code, rip, rsp,
    );
    crate::arch::x86_64::hypercall::shutdown(1);
}

// ---- Exception trampolines (no error code) -------------------------------

/// #UD (vector 0x06) — Undefined Instruction.
///
/// # Safety
/// Called only by the CPU via the IDT. The IDT entry must be correctly
/// configured with this function's address. Not to be called directly.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn isr_ud() {
    // SAFETY: CPU pushed RIP, CS, RFLAGS on the stack (no error code).
    naked_asm!(
        "push 0",           // fake error code for uniformity
        "push rax",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        // rip is at [rsp + 10*8 + 8] (after error + 10 pushes; CPU pushed RIP first)
        // For a simple shutdown, we just call exception_handler with the vector.
        // Pass args: rdi=vector, rsi=error_code, rdx=rip, rcx=rsp (original)
        "mov rdi, 0x06",
        "mov rsi, 0",
        "mov rdx, [rsp + 80]",   // saved RIP (10 pushes * 8 = 80 bytes)
        "mov rcx, rsp",
        "call exception_handler",
        // exception_handler calls shutdown and never returns; this is unreachable.
        "ud2",
    );
}

/// #DF (vector 0x08) — Double Fault (has error code, always 0).
///
/// # Safety
/// Called only by the CPU via the IDT. Not to be called directly.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn isr_df() {
    // SAFETY: CPU pushed error code (always 0) at RSP, then the 5-qword
    // ring-transition frame above it. We push 9 regs (72 bytes), so the
    // error code is at [RSP+72] and RIP is at [RSP+80].
    naked_asm!(
        // error code is already on the stack (pushed by CPU at offset 0)
        "push rax",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        // 9 pushes = 72 bytes; error code at [rsp+72], RIP at [rsp+80]
        "mov rdi, 0x08",
        "mov rsi, [rsp + 72]",   // error code
        "mov rdx, [rsp + 80]",   // saved RIP
        "mov rcx, rsp",
        "call exception_handler",
        "ud2",
    );
}

/// #GP (vector 0x0D) — General Protection Fault (has error code).
///
/// # Safety
/// Called only by the CPU via the IDT. Not to be called directly.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn isr_gp() {
    // SAFETY: CPU pushed error code at RSP. After 9 reg pushes (72 bytes),
    // error code is at [rsp+72] and RIP is at [rsp+80].
    naked_asm!(
        "push rax",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "mov rdi, 0x0D",
        "mov rsi, [rsp + 72]",   // error code
        "mov rdx, [rsp + 80]",   // saved RIP
        "mov rcx, rsp",
        "call exception_handler",
        "ud2",
    );
}

/// #PF (vector 0x0E) — Page Fault (has error code).
///
/// # Safety
/// Called only by the CPU via the IDT. Not to be called directly.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn isr_pf() {
    // SAFETY: CPU pushed error code at RSP. After 9 reg pushes (72 bytes),
    // error code is at [rsp+72] and RIP is at [rsp+80].
    naked_asm!(
        "push rax",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "mov rdi, 0x0E",
        "mov rsi, [rsp + 72]",   // error code
        "mov rdx, [rsp + 80]",   // saved RIP
        "mov rcx, rsp",
        "call exception_handler",
        "ud2",
    );
}

// ---- Timer ISR trampoline ------------------------------------------------

/// Timer ISR (vector 0x40). Entry from user mode via IST1.
///
/// Frame on IST1 at entry (CPU-pushed, high addresses first):
///   SS
///   RSP  (user)
///   RFLAGS
///   CS
///   RIP  (user) ← RSP on entry
///
/// We save 15 GP regs below that, giving a 20-qword (160-byte) frame.
///
/// ## Return path note (ring-0 IST)
/// In this unikernel all code runs at CPL=0 — including nominal "user"
/// code (`sregs.cs.dpl = 0` for every ring; see `sumi-vm/.../kvm/mod.rs`).
/// When the timer fires, the CPU pushes the full 5-qword frame (SS, RSP,
/// RFLAGS, CS, RIP) because IST always triggers a stack switch on entry,
/// privilege change or not. `iretq`, however, only *pops* SS:RSP when the
/// popped CS.RPL differs from the current CPL — since that's never true
/// here, a plain `iretq` would silently leave RSP wrong (confirmed
/// empirically: RIP lands correctly, RSP does not). So the restore path
/// below does the stack switch itself instead of using `iretq`: clear IF
/// in the RFLAGS copy (keeping interrupts disabled during the entire
/// critical window), restore remaining flags via `popfq`, save user `rax`
/// to `gs:[{saved_rsp_off}]` (the stale `saved_user_rsp` slot, which is
/// only written by syscall_entry and never read here), switch RSP to the
/// saved user RSP, push the return RIP onto the user stack, restore `rax`,
/// `sti`, then `ret`.
///
/// # Safety
/// Called only by the CPU via the IDT when the LAPIC timer fires. The
/// IDT entry must point at this function with IST=1. Not to be called
/// directly. GS base must be valid (set by `init_for_cpu`) and
/// `current_thread` must be non-null (`init_phase3_bsp/ap` ran).
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn isr_timer() {
    naked_asm!(
        // Save all caller-saved and callee-saved GP regs.
        // RSP is managed explicitly (IST1 / kernel stack).
        "push rax",
        "push rcx",
        "push rdx",
        "push rbx",
        "push rbp",
        "push rsi",
        "push rdi",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        // 15 pushes → RSP now points at saved R15 (bottom of GP regs).
        // Above RSP (higher addresses): the 5 CPU-pushed qwords.
        // Total = 20 qwords = 160 bytes.

        // Increment preempt_count. Done with interrupts disabled (interrupt
        // gate cleared IF) so no atomic needed; plain add to gs:[offset].
        "add dword ptr gs:[{preempt_off}], 1",

        // Call Rust timer handler (EOI + set need_resched).
        "call timer_interrupt",

        // Decrement preempt_count.
        "sub dword ptr gs:[{preempt_off}], 1",

        // Check whether we should preempt:
        //   preempt_count must be 0 AND need_resched must be true.
        "cmp dword ptr gs:[{preempt_off}], 0",
        "jne 2f",
        "cmp byte ptr gs:[{need_resched_off}], 0",
        "je 2f",

        // === Preemption path ===
        //
        // The 20-qword interrupt frame lives on the IST1 stack. We must
        // copy it to the current thread's kernel stack before calling
        // schedule() so the IST1 stack is free for future interrupts.
        //
        // Guard: if current_thread is null (not yet set by init_phase3),
        // skip preemption. This handles the early-init window on BSP/AP
        // between lapic::init() and init_phase3_bsp/ap().
        "mov rax, gs:[{current_thread_off}]", // rax = *current_thread ptr
        "test rax, rax",
        "jz 2f",                              // null → skip preemption
        //
        // Load kernel_stack_top from the current thread:
        //   [thread + {kstack_off}] = Thread.kernel_stack_top
        "mov rax, [rax + {kstack_off}]",      // rax = kernel_stack_top (virtual addr)

        // Reserve 160 bytes below kernel_stack_top for the frame copy.
        "sub rax, 160",

        // Copy 20 qwords from IST1 stack (rsp) to kernel stack (rax).
        // rsi = source (IST1 rsp), rdi = dest (kernel stack frame bottom).
        // rcx, rsi, rdi are caller-saved so clobbering them is fine here
        // (we're about to restore them from the frame we're copying anyway).
        "mov rcx, 20",
        "mov rsi, rsp",
        "mov rdi, rax",
        "cld",
        "rep movsq",

        // Switch to the kernel stack (pointing at the copied frame bottom).
        "mov rsp, rax",

        // Call schedule_preempt(). This puts the current thread back on
        // the runqueue and calls schedule(). schedule() → __switch_to_asm
        // saves RSP here (pointing just past the frame) and switches to
        // the next thread. When this thread is re-scheduled, __switch_to_asm
        // restores RSP and returns here → schedule_preempt returns here → falls
        // through to the restore path below.
        "call schedule_preempt",

        // === Restore path ===
        // RSP points at the bottom of the 15 GP regs (either on the IST1
        // stack if no preemption, or on the kernel stack after preemption).
        // Pop all 15 GP regs, then restore RFLAGS, RSP, and jump to user RIP
        // — see the module-level doc comment for why this can't be a plain
        // `iretq` (sumi runs every ring at CPL0, so IRET's SS:RSP pop, which
        // is conditional on an actual CPL change, never fires).
        //
        // The strategy (IF=0 for the entire critical window):
        //   1. Pop all 15 GP regs (restoring rax last).
        //   2. Push the RFLAGS slot from the frame onto the stack (copy).
        //   3. Clear IF (bit 9) in the copy — keeps interrupts disabled
        //      through `popfq` so we can safely use `gs:[saved_rsp_off]`.
        //   4. `popfq` — restores all flags except IF=0.
        //   5. Save user rax to `gs:[saved_rsp_off]` (stale after syscall
        //      entry; never read between syscall entry and here).
        //   6. Load RIP_user from frame into rax.
        //   7. Switch RSP to RSP_user (from frame).
        //   8. Push RIP_user onto user stack.
        //   9. Restore rax from `gs:[saved_rsp_off]`.
        //  10. `sti` — re-enable interrupts (user code runs with IF=1).
        //  11. `ret` — jump to RIP_user with RSP = RSP_user.
        "2:",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdi",
        "pop rsi",
        "pop rbp",
        "pop rbx",
        "pop rdx",
        "pop rcx",
        "pop rax",
        // RSP: [RIP][CS][RFLAGS][RSP_user][SS]
        // Restore RFLAGS with IF cleared so the rest of the sequence runs
        // with interrupts disabled.
        "push qword ptr [rsp + 16]",         // push RFLAGS_user copy
        "and dword ptr [rsp], 0xFFFFFDFF",   // clear IF (bit 9)
        "popfq",                              // restore flags (IF=0); RSP net unchanged
        // RSP: [RIP][CS][RFLAGS][RSP_user][SS] (back to same position)
        "mov qword ptr gs:[{saved_rsp_off}], rax",  // stash user rax
        "mov rax, [rsp]",                           // rax = RIP_user
        "mov rsp, [rsp + 24]",                      // RSP = RSP_user
        "push rax",                                  // push RIP_user onto user stack
        "mov rax, qword ptr gs:[{saved_rsp_off}]",  // restore user rax
        "sti",                                       // re-enable interrupts
        "ret",                                       // jump to RIP_user; RSP = RSP_user

        preempt_off         = const percpu::PREEMPT_COUNT_OFFSET,
        need_resched_off    = const core::mem::offset_of!(percpu::PerCpu, need_resched),
        kstack_off          = const KERNEL_STACK_TOP_OFFSET,
        current_thread_off  = const percpu::CURRENT_THREAD_OFFSET,
        saved_rsp_off        = const percpu::SAVED_USER_RSP_OFFSET,
    );
}

// ---- IPI ISR trampoline --------------------------------------------------

/// IPI ISR (vector 0x41). Fires when a peer CPU kicks this CPU via
/// `hypercall::kick_cpu`. We just ACK and return; the CPU will re-check
/// its runqueue in `idle_loop` after the `hlt` resumes.
///
/// Same restore-path constraint as `isr_timer`: not a plain `iretq` (see
/// its doc comment) — `kick_cpu` almost always interrupts a `hlt` in
/// `idle_loop`, and leaving RSP wrong there would corrupt that CPU's next
/// bit of Rust execution the moment it touches the stack.
///
/// # Safety
/// Called only by the CPU via the IDT. Not to be called directly.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn isr_ipi() {
    naked_asm!(
        "push rax",
        "push rcx",
        "push rdx",
        "call ipi_interrupt",
        "pop rdx",
        "pop rcx",
        "pop rax",
        // RSP: [RIP][CS][RFLAGS][RSP][SS] — same manual restore as isr_timer.
        "push qword ptr [rsp + 16]",
        "and dword ptr [rsp], 0xFFFFFDFF",
        "popfq",
        "mov qword ptr gs:[{saved_rsp_off}], rax",
        "mov rax, [rsp]",
        "mov rsp, [rsp + 24]",
        "push rax",
        "mov rax, qword ptr gs:[{saved_rsp_off}]",
        "sti",
        "ret",

        saved_rsp_off = const percpu::SAVED_USER_RSP_OFFSET,
    );
}
