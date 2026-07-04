//! Per-CPU kernel state and its static storage.
//!
//! One `PerCpu` per vCPU, kept in a static `[PerCpu; MAX_VCPUS]`. On each
//! CPU, `IA32_GS_BASE` is programmed to point at that CPU's `PerCpu`, so
//! the `syscall_entry` asm reads per-CPU scratch via `gs:<offset>` without
//! touching any global. See `docs/design/multithreading-v2.md` §3.4 and
//! §7.3 (no `swapgs` — ring 0 only, so `IA32_GS_BASE` is correct,
//! not `IA32_KERNEL_GS_BASE`).
//!
//! The byte offsets of `saved_user_rsp` and `current_thread` are hard-coded
//! in the `syscall_entry` / `isr_timer` asm (`crate::arch::x86_64::syscall`,
//! `crate::arch::x86_64::interrupt`) via the `SAVED_USER_RSP_OFFSET` /
//! `CURRENT_THREAD_OFFSET` constants below, passed in as `global_asm!`/
//! `naked_asm!` `const` operands. Reordering fields is safe as long as
//! those constants stay correct — the `const _: () = assert!(...)` block
//! below turns drift into a build-time error either way.

/// Maximum number of vCPUs. Matches the sumi-vm `--vcpus` cap (§4.1).
pub const MAX_VCPUS: usize = 64;

/// Per-CPU kernel state. GS base on each vCPU points at one of these.
/// The `syscall_entry` / `isr_timer` asm hard-codes offsets via the
/// `SAVED_USER_RSP_OFFSET` / `CURRENT_THREAD_OFFSET` / `PREEMPT_COUNT_OFFSET`
/// constants below — see the compile-time asserts at the bottom of this file.
///
/// The align(64) is kept so each PerCpu sits on a fresh cache line; size is
/// capped at 512 bytes.
#[repr(C, align(64))]
pub struct PerCpu {
    /// Self pointer; lets `this_cpu()` retrieve `&PerCpu` via a single
    /// `mov reg, gs:[0]`. Initialised by `init_for_cpu`. Offset 0.
    pub self_ptr: *const PerCpu,
    /// Scratch slot for user RSP during the syscall-entry stack switch.
    pub saved_user_rsp: u64,
    /// Logical CPU id (0..MAX_VCPUS).
    pub cpu_id: u32,
    _pad0: u32,

    /// Currently executing thread on this CPU. Non-null after
    /// `init_phase3_bsp/ap`. Cross-CPU writers use Release/AcqRel.
    pub current_thread: core::sync::atomic::AtomicPtr<super::thread::Thread>,
    /// Idle thread for this CPU. Written once at init, read many times.
    pub idle_thread: core::sync::atomic::AtomicPtr<super::thread::Thread>,
    /// Set by `wake_blocked` / a new `clone()` child to request a
    /// reschedule at the next syscall return.
    pub need_resched: core::sync::atomic::AtomicBool,
    /// True while this CPU is executing `idle_loop`. Used by `wake_blocked`
    /// to decide whether to send an IPI.
    pub is_idle: core::sync::atomic::AtomicBool,
    _pad1: [u8; 6],
    /// Runnable threads waiting for this CPU.
    pub runqueue: super::runqueue::RunQueue,
    /// Tracks the last TLB generation this CPU has flushed to. When this
    /// lags behind `TLB_GENERATION`, the CPU reloads CR3 (see
    /// `reload_tlb_if_stale`, F10).
    pub tlb_generation: core::sync::atomic::AtomicU64,

    /// Preemption depth counter. 0 = preemptible. The timer ISR increments
    /// this on entry and decrements it on exit, then checks whether to call
    /// schedule_preempt. Written only by this CPU with interrupts disabled
    /// (the ISR runs with IF=0 because it's an interrupt gate), so no
    /// atomic operations are needed — plain UnsafeCell<u32> is correct.
    pub preempt_count: core::cell::UnsafeCell<u32>,
}

impl PerCpu {
    /// All-zero initialiser for the static array. Real values are
    /// written by `init_for_cpu` on each CPU before it runs any user code.
    pub const fn zeroed() -> Self {
        Self {
            self_ptr: core::ptr::null(),
            saved_user_rsp: 0,
            cpu_id: 0,
            _pad0: 0,
            current_thread: core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()),
            idle_thread:     core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()),
            need_resched:    core::sync::atomic::AtomicBool::new(false),
            is_idle:         core::sync::atomic::AtomicBool::new(false),
            _pad1:           [0u8; 6],
            runqueue:        super::runqueue::RunQueue::new(),
            tlb_generation:  core::sync::atomic::AtomicU64::new(0),
            preempt_count:   core::cell::UnsafeCell::new(0),
        }
    }

    /// Reload CR3 if this CPU's TLB generation lags the global counter,
    /// flushing stale non-global TLB entries left by a peer CPU's
    /// `mprotect`/`munmap`. Called from both the syscall-return postamble
    /// and the timer-tick handler (F10) — anywhere this CPU is about to
    /// resume user-mode execution.
    #[cfg(not(test))]
    pub fn reload_tlb_if_stale(&self) {
        use core::sync::atomic::Ordering;
        let global_gen = crate::TLB_GENERATION.load(Ordering::Acquire);
        if self.tlb_generation.load(Ordering::Relaxed) < global_gen {
            // SAFETY: reading/writing CR3 is always valid from ring 0; we
            // reload the same page table to flush non-global TLB entries.
            unsafe {
                let cr3: u64;
                core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
                core::arch::asm!("mov cr3, {}", in(reg) cr3, options(nomem, nostack));
            }
            self.tlb_generation.store(global_gen, Ordering::Relaxed);
        }
    }
}

// SAFETY: PerCpu is only ever mutated from its owning CPU, and the
// scratch field (saved_user_rsp) is only ever read by that same CPU
// inside syscall_entry. Fields added in later phases that need cross-CPU
// access (e.g. need_resched, is_idle) must themselves use atomic types —
// this blanket Sync impl does NOT make plain fields cross-CPU-safe.
unsafe impl Sync for PerCpu {}

#[cfg(not(test))]
pub(crate) const SYSCALL_STACK_SIZE: usize = 64 * 1024;

#[cfg(not(test))]
#[repr(C, align(16))]
pub(crate) struct SyscallStackBuf([u8; SYSCALL_STACK_SIZE]);

#[cfg(not(test))]
impl SyscallStackBuf {
    pub(crate) const fn new() -> Self {
        Self([0; SYSCALL_STACK_SIZE])
    }
}

/// One `PerCpu` per vCPU. BSP uses index 0. Phase 1 brings up the rest.
#[cfg(not(test))]
pub(crate) static mut PER_CPU: [PerCpu; MAX_VCPUS] =
    [const { PerCpu::zeroed() }; MAX_VCPUS];

/// Dedicated 64 KiB interrupt stack (TSS IST1) per vCPU. Indexed by
/// `cpu_id`. Kept here (not in `arch::x86_64::syscall`) so that Phase 1 AP
/// bring-up is a no-op on this side.
#[cfg(not(test))]
pub(crate) static mut SYSCALL_STACKS: [SyscallStackBuf; MAX_VCPUS] =
    [const { SyscallStackBuf::new() }; MAX_VCPUS];

/// Dedicated boot stack per AP. The `ap_start_asm` entry stub loads
/// `rsp` from this array using the natural-ABI RDI = cpu_id passed by
/// the host before the AP has any valid stack. Index 0 is unused (the
/// BSP uses the stack set by sumi-vm from `KERNEL_STACK`).
///
/// Kept separate from `SYSCALL_STACKS` because an AP uses this stack
/// during the few instructions of `ap_main_rust` before it ever enters
/// a syscall path; mixing the two would make a stack overflow during
/// early init silently corrupt the syscall stack of the same CPU.
pub(crate) const AP_BOOT_STACK_SIZE: usize = 16 * 1024;

#[repr(C, align(16))]
pub(crate) struct ApBootStackBuf(pub(crate) [u8; AP_BOOT_STACK_SIZE]);

#[cfg(not(test))]
impl ApBootStackBuf {
    pub(crate) const fn new() -> Self {
        Self([0; AP_BOOT_STACK_SIZE])
    }
}

/// One boot stack per possible vCPU. AP `cpu_id == i` reads element
/// `i` and uses its top. Element 0 is wasted (BSP has its own stack)
/// but kept so indexing is `cpu_id * AP_BOOT_STACK_SIZE` without an
/// off-by-one.
///
/// `#[unsafe(no_mangle)]` because `ap_start_asm` addresses the symbol
/// by name via `lea rax, [rip + AP_BOOT_STACKS]`.
#[cfg(not(test))]
#[unsafe(no_mangle)]
pub(crate) static mut AP_BOOT_STACKS: [ApBootStackBuf; MAX_VCPUS] =
    [const { ApBootStackBuf::new() }; MAX_VCPUS];

/// Initialise `PER_CPU[cpu_id]` and load this CPU's `IA32_GS_BASE`
/// with the address of that slot.
///
/// Must be called exactly once per vCPU, on that vCPU, BEFORE
/// `syscall::init()`.
///
/// Gated out of `cfg(test)` because it writes an MSR (would GP-fault
/// in ring 3). Tests cover layout only; the MSR write is exercised
/// by the KVM integration tests.
#[cfg(not(test))]
pub fn init_for_cpu(cpu_id: u32) {
    /// IA32_GS_BASE MSR. Not `IA32_KERNEL_GS_BASE` (0xC000_0102): sumi
    /// runs only in ring 0, so there is no `swapgs` path.
    const IA32_GS_BASE: u32 = 0xC000_0101;

    debug_assert!((cpu_id as usize) < MAX_VCPUS);
    let idx = cpu_id as usize;

    // SAFETY: each CPU writes its own distinct array slot. For the
    // BSP this is serialized (only cpu 0 runs). For APs, each AP
    // writes `PER_CPU[cpu_id]` exactly once from its own thread of
    // execution, and no other CPU reads that slot until the owner
    // has stored `IA32_GS_BASE` and begun running. Cross-CPU
    // visibility of any field other than `self_ptr` is neither
    // needed nor claimed.
    unsafe {
        let pc_ptr: *mut PerCpu = core::ptr::addr_of_mut!(PER_CPU[idx]);

        core::ptr::write(
            pc_ptr,
            PerCpu {
                self_ptr: pc_ptr as *const PerCpu,
                saved_user_rsp: 0,
                cpu_id,
                _pad0: 0,
                current_thread: core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()),
                idle_thread:     core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()),
                need_resched:    core::sync::atomic::AtomicBool::new(false),
                is_idle:         core::sync::atomic::AtomicBool::new(false),
                _pad1:           [0u8; 6],
                runqueue:        super::runqueue::RunQueue::new(),
                tlb_generation:  core::sync::atomic::AtomicU64::new(0),
                preempt_count:   core::cell::UnsafeCell::new(0),
            },
        );

        // Ring 0, valid MSR, value is the linear address of a `'static`.
        crate::arch::x86_64::syscall::wrmsr(IA32_GS_BASE, pc_ptr as u64);
    }
}

/// Return a reference to the currently running CPU's `PerCpu`. Not used
/// in Phase 0, declared so later phases have a stable entry point.
/// Undefined behaviour if called before `init_for_cpu`.
#[cfg(not(test))]
pub fn this_cpu() -> &'static PerCpu {
    let pc: *const PerCpu;
    // SAFETY: `init_for_cpu` writes `IA32_GS_BASE` to a valid
    // `&'static PerCpu` and never changes it. Callers only read.
    unsafe {
        core::arch::asm!(
            "mov {}, gs:0",
            out(reg) pc,
            options(nostack, preserves_flags, readonly),
        );
        &*pc
    }
}

/// Return a reference to `PER_CPU[cpu_id]` if that CPU has been initialised
/// (i.e. `init_for_cpu` has run on it). Returns `None` for uninitialised slots
/// and out-of-range indices.
///
/// Used by `wake_blocked` to push to a specific CPU's runqueue.
#[cfg(not(test))]
pub fn get(cpu_id: u32) -> Option<&'static PerCpu> {
    if (cpu_id as usize) >= MAX_VCPUS {
        return None;
    }
    // SAFETY: `PER_CPU` is a `'static` array; reading at a valid index and
    // returning with `'static` lifetime is sound. The slot's `self_ptr` is
    // null until `init_for_cpu` runs; we use that as the "not yet ready"
    // sentinel.
    let pc = unsafe { &*core::ptr::addr_of!(PER_CPU[cpu_id as usize]) };
    if pc.self_ptr.is_null() { None } else { Some(pc) }
}

// ---- Compile-time layout invariants (the asm depends on these) ----

/// Byte offset of `preempt_count` within `PerCpu`. Used by the timer ISR
/// trampoline in `arch::x86_64::interrupt` via `gs:[PREEMPT_COUNT_OFFSET]`.
pub const PREEMPT_COUNT_OFFSET: usize = core::mem::offset_of!(PerCpu, preempt_count);

/// Byte offset of `saved_user_rsp` within `PerCpu`. Referenced as a
/// `global_asm!`/`naked_asm!` `const` operand by `syscall_entry`
/// (`crate::arch::x86_64::syscall`) and `isr_timer`
/// (`crate::arch::x86_64::interrupt`).
pub const SAVED_USER_RSP_OFFSET: usize = core::mem::offset_of!(PerCpu, saved_user_rsp);

/// Byte offset of `current_thread` within `PerCpu`. Referenced as a
/// `const` operand by `syscall_entry` and `isr_timer` to load
/// `*mut Thread` via `gs:[{current_thread_off}]`.
pub const CURRENT_THREAD_OFFSET: usize = core::mem::offset_of!(PerCpu, current_thread);

const _: () = {
    assert!(
        core::mem::offset_of!(PerCpu, self_ptr) == 0,
        "this_cpu() reads gs:[0] for the self pointer",
    );
    assert!(
        core::mem::offset_of!(PerCpu, cpu_id) == 16,
        "cpu_id offset is 16",
    );
    assert!(
        core::mem::size_of::<PerCpu>() <= 512,
        "PerCpu exceeds 512-byte cap",
    );
    assert!(
        core::mem::align_of::<PerCpu>() == 64,
        "PerCpu must be 64-byte aligned",
    );
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_offsets_match_asm_constants() {
        assert!(core::mem::size_of::<PerCpu>() <= 512);
        assert_eq!(core::mem::align_of::<PerCpu>(), 64);
        assert_eq!(core::mem::offset_of!(PerCpu, self_ptr), 0);
        assert_eq!(
            core::mem::offset_of!(PerCpu, saved_user_rsp),
            SAVED_USER_RSP_OFFSET,
        );
        assert_eq!(
            core::mem::offset_of!(PerCpu, current_thread),
            CURRENT_THREAD_OFFSET,
        );
        assert_eq!(core::mem::offset_of!(PerCpu, cpu_id), 16);
    }

    #[test]
    fn self_ptr_at_offset_zero() {
        assert_eq!(core::mem::offset_of!(PerCpu, self_ptr), 0);
    }

    #[test]
    fn zeroed_self_ptr_is_null() {
        let p = PerCpu::zeroed();
        assert!(p.self_ptr.is_null());
    }

    #[test]
    fn zeroed_is_zero() {
        let p = PerCpu::zeroed();
        assert_eq!(p.cpu_id, 0);
        assert_eq!(p.saved_user_rsp, 0);
    }

    #[test]
    fn max_vcpus_matches_sumi_vm_cap() {
        assert_eq!(MAX_VCPUS, 64);
    }

    #[test]
    fn phase3_fields_at_or_after_current_thread() {
        assert!(core::mem::offset_of!(PerCpu, idle_thread) >= CURRENT_THREAD_OFFSET);
        assert!(core::mem::offset_of!(PerCpu, need_resched) >= CURRENT_THREAD_OFFSET);
        assert!(core::mem::offset_of!(PerCpu, is_idle) >= CURRENT_THREAD_OFFSET);
        assert!(core::mem::offset_of!(PerCpu, runqueue) >= CURRENT_THREAD_OFFSET);
    }

    #[test]
    fn zeroed_phase3_fields_are_null_and_false() {
        use core::sync::atomic::Ordering;
        let p = PerCpu::zeroed();
        assert!(p.current_thread.load(Ordering::Relaxed).is_null());
        assert!(p.idle_thread.load(Ordering::Relaxed).is_null());
        assert!(!p.need_resched.load(Ordering::Relaxed));
        assert!(!p.is_idle.load(Ordering::Relaxed));
    }
}

// Boot-stack size must be a multiple of 16 so that rsp is 16-aligned
// after the asm stub points it at the top of the slot. This check runs
// in both production and test builds so a bad resize is caught at
// compile time regardless of test coverage.
const _: () = {
    assert!(
        core::mem::size_of::<ApBootStackBuf>().is_multiple_of(16),
        "ApBootStackBuf must be 16-aligned for SysV ABI at call sites",
    );
};

#[cfg(test)]
mod ap_tests {
    use super::*;

    #[test]
    fn max_vcpus_still_64() {
        assert_eq!(MAX_VCPUS, 64);
    }
}
