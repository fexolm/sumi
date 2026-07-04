//! Thread descriptor and associated layout types.
//!
//! See `docs/design/multithreading-v2.md` §3.1.

extern crate alloc;

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, AtomicU64};

use sumi_abi::address::{PhysicalAddr, VirtualAddr};

#[cfg(not(test))]
use alloc::sync::Arc;

/// Linux-compatible thread id. 0 reserved; main thread = 1; kthreads start at 2.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(transparent)]
pub struct Tid(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum ThreadState {
    New = 0,
    Runnable = 1,
    Running = 2,
    Blocked = 3,
    Exited = 4,
}

/// FXSAVE/FXRSTOR area: legacy x87/MMX/SSE register state, 512 bytes,
/// 16-byte aligned (Intel SDM Vol.1 §10.5.1).
///
/// Not a full XSAVE area: `sumi-vm` deliberately leaves `CR4.OSXSAVE` clear
/// so the guest CPUID mask can hide AVX from glibc's IFUNC resolvers (see
/// `sumi-vm/src/arch/x86_64/kvm/mod.rs`), and `XSAVE`/`XRSTOR` themselves
/// `#UD` without `OSXSAVE`. `FXSAVE`/`FXRSTOR` only need `CR4.OSFXSR` (set
/// at vCPU init) and cover every FPU/SSE register the guest can reach given
/// that mask (F7).
#[repr(C, align(16))]
pub struct FxsaveArea([u8; 512]);

impl FxsaveArea {
    /// Default legacy FPU/SSE state: x87 control word 0x037F and MXCSR
    /// 0x1F80 — round-to-nearest with every FP/SSE exception masked, the
    /// same state the CPU establishes after `fninit`. Every other byte
    /// (tag word, ST/MM/XMM registers) is zero, which `FXRSTOR` accepts:
    /// an abridged tag word of 0 marks every x87 register empty.
    ///
    /// This matters: restoring an all-zero image would leave MXCSR = 0,
    /// i.e. every SSE exception *unmasked*, so the first inexact-result
    /// FP op (extremely common) would raise an unhandled #XM.
    pub const fn new() -> Self {
        let mut buf = [0u8; 512];
        buf[0] = 0x7F;
        buf[1] = 0x03; // FCW = 0x037F
        buf[24] = 0x80;
        buf[25] = 0x1F; // MXCSR = 0x1F80
        Self(buf)
    }
}

impl Default for FxsaveArea {
    fn default() -> Self {
        Self::new()
    }
}

/// Callee-saved CPU state, saved and restored by `__switch_to_asm`.
///
/// Layout is fixed — the asm uses hard-coded offsets. Adding fields must
/// go after `fxsave_area` and update the asm correspondingly.
#[repr(C)]
pub struct ThreadContext {
    pub rsp: u64,    // 0x00
    pub rbp: u64,    // 0x08
    pub rbx: u64,    // 0x10
    pub r12: u64,    // 0x18
    pub r13: u64,    // 0x20
    pub r14: u64,    // 0x28
    pub r15: u64,    // 0x30
    pub rflags: u64, // 0x38
    pub fxsave_area: FxsaveArea, // 0x40, 512 bytes, 16-aligned (F7).
}

/// Intrusive doubly-linked list node used by `RunQueue`.
/// Mutated only while holding `RunQueue.inner` lock.
#[repr(C)]
pub struct RunLink {
    pub inner: UnsafeCell<RunLinkInner>,
}

#[repr(C)]
pub struct RunLinkInner {
    pub next: *mut Thread,
    pub prev: *mut Thread,
}

impl Default for RunLink {
    fn default() -> Self {
        Self::new()
    }
}

impl RunLink {
    pub const fn new() -> Self {
        Self {
            inner: UnsafeCell::new(RunLinkInner {
                next: core::ptr::null_mut(),
                prev: core::ptr::null_mut(),
            }),
        }
    }
}

/// Intrusive node for futex wait queues. Phase 5 fills in the semantics.
/// Kept here for layout stability so Phase 5 does not shift Thread offsets.
#[repr(C)]
pub struct WaitLink {
    // Phase 5 fills these in.
    pub next: AtomicPtr<Thread>,
    pub uaddr: AtomicU64,
    pub bitset: AtomicU32,
}

impl Default for WaitLink {
    fn default() -> Self {
        Self::new()
    }
}

impl WaitLink {
    pub const fn new() -> Self {
        Self {
            next: AtomicPtr::new(core::ptr::null_mut()),
            uaddr: AtomicU64::new(0),
            bitset: AtomicU32::new(0),
        }
    }
}

/// Thread descriptor. Always heap-allocated behind `Arc<Thread>`.
///
/// Fields beyond `run_link`/`wait_link` that are only relevant in later
/// phases are included now for layout stability — adding them later would
/// require touching every `Thread::new_*` constructor.
#[repr(C, align(64))]
pub struct Thread {
    pub tid: Tid,
    pub tgid: Tid,
    pub state: AtomicU32,
    pub exit_code: AtomicI32,

    /// Callee-saved register context. Only `__switch_to_asm` touches this
    /// field (from the owning CPU); no lock needed for the switch path.
    pub ctx: UnsafeCell<ThreadContext>,

    pub kernel_stack_top: VirtualAddr,
    pub kernel_stack_phys: PhysicalAddr,
    pub kernel_stack_size: usize,
    /// Phase 7: true if kernel_stack_phys was allocated from PAGE_ALLOCATOR
    /// and should be freed by the reaper when this thread is destroyed.
    pub kernel_stack_freeable: bool,

    /// Zero for kthreads. Populated by clone() in Phase 4.
    pub user_stack_base: VirtualAddr,
    pub user_stack_size: usize,

    /// Per-thread FS base for TLS. Phase 6: written by arch_prctl / clone.
    pub fs_base: AtomicU64,

    /// Phase 7: address to write 0 on thread exit (set_tid_address).
    pub clear_child_tid: AtomicU64,

    /// Phase 7: robust futex list head (set via prctl/clone flags).
    pub robust_list_head: AtomicU64,

    /// Last CPU this thread ran on. `u32::MAX` = "never ran".
    pub cpu: AtomicU32,

    /// True from the moment this thread is chosen as `next` in `schedule()`
    /// until `__switch_to_asm` finishes capturing its context (F3/F4,
    /// mirrors Linux's `p->on_cpu`). Consumers that might hand this
    /// thread's `ctx.rsp` to a *different* CPU's `__switch_to_asm` — or
    /// free its kernel stack — must spin until this is `false`:
    /// `wake_blocked` (F3), `try_steal_work` (F3), `reap_zombies` (F4).
    pub on_cpu: AtomicBool,

    pub run_link: RunLink,
    pub wait_link: WaitLink,

    // Phase 3 kthread trampoline payload. Kthreads set these at spawn time;
    // `kthread_trampoline` reads them once on first schedule-in. User
    // threads leave both at zero; Phase 4 clone() does not touch them.
    pub entry_fn: AtomicU64, // extern "C" fn(u64) -> ! as u64, or 0
    pub entry_arg: AtomicU64,
}

/// Byte offset of `Thread.kernel_stack_top` within `Thread`.
///
/// Referenced by the `syscall_entry` asm in `arch::x86_64::syscall` via the
/// `const` operand of `global_asm!`. The asm uses this offset in
/// `mov rsp, [rsp + {kstack_off}]` to switch to the per-thread kernel stack.
/// Any reorder of `Thread` fields is caught at compile time by the assert below.
pub const KERNEL_STACK_TOP_OFFSET: usize = core::mem::offset_of!(Thread, kernel_stack_top);

const _: () = {
    // The disp8-encoding threshold this used to enforce (< 256) no longer
    // holds now that `ctx` embeds a 512-byte FXSAVE area (F7) — the asm
    // emits a disp32 instead, which works identically, just one byte
    // larger per access. This bound is now just a sanity canary against
    // unexpected further Thread growth.
    assert!(
        KERNEL_STACK_TOP_OFFSET < 4096,
        "kernel_stack_top offset >= 4096; Thread grew unexpectedly",
    );
};

// SAFETY: every mutable field is either atomic, accessed only under
// `RunQueue.inner` lock (`run_link`), or accessed only by `__switch_to_asm`
// from the owning CPU with no concurrent access (`ctx`).
unsafe impl Sync for Thread {}
// SAFETY: Thread is always accessed via Arc; there is no per-CPU unboxed
// Thread. Moving the Arc across threads is safe for the same reason `Sync` is.
unsafe impl Send for Thread {}

#[cfg(test)]
impl Thread {
    /// Construct a minimal Thread for unit tests. Does NOT allocate a real
    /// kernel stack — `kernel_stack_top` and `kernel_stack_phys` are left
    /// as zero sentinels. Tests that inspect layout or queue behaviour use
    /// this constructor.
    pub fn new_test(tid: u32) -> Self {
        Self {
            tid: Tid(tid),
            tgid: Tid(tid),
            state: AtomicU32::new(ThreadState::Runnable as u32),
            exit_code: AtomicI32::new(0),
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
            kernel_stack_top: VirtualAddr::new(0),
            kernel_stack_phys: PhysicalAddr::new(0),
            kernel_stack_size: 0,
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
            entry_fn: AtomicU64::new(0),
            entry_arg: AtomicU64::new(0),
        }
    }
}

// ---- Kernel-thread construction (F17: moved out of sched/mod.rs) ---------

/// Build a `Thread` for a kernel thread, with a `kthread_trampoline` return
/// address written at the top of its stack so the first `__switch_to_asm`
/// restore jumps there. Shared by the BSP idle thread constructor (the only
/// remaining caller now that `kthread_spawn` is gone — see F12).
#[cfg(not(test))]
fn build_kthread_arc(
    entry_fn: extern "C" fn(u64) -> !,
    arg: u64,
    tid: Tid,
    stack_phys: PhysicalAddr,
    stack_top_virt: VirtualAddr,
    stack_size: usize,
) -> Thread {
    debug_assert_eq!(stack_top_virt.as_usize() % 16, 0,
        "stack top must be 16-aligned");

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
        state:     AtomicU32::new(ThreadState::Runnable as u32),
        exit_code: AtomicI32::new(0),
        ctx: UnsafeCell::new(ThreadContext {
            rsp: slot,
            rbp: 0, rbx: 0, r12: 0, r13: 0, r14: 0, r15: 0,
            // EFLAGS: bit 1 (reserved, always 1) | bit 9 (IF=1). __switch_to_asm
            // forces IF back to 0 for a thread's first switch-in regardless
            // (F2); this only matters for the reserved bit.
            rflags: 0x202,
            fxsave_area: FxsaveArea::new(),
        }),
        kernel_stack_top:  stack_top_virt,
        kernel_stack_phys: stack_phys,
        kernel_stack_size: stack_size,
        kernel_stack_freeable: false,
        user_stack_base: VirtualAddr::new(0),
        user_stack_size: 0,
        fs_base:          AtomicU64::new(0),
        clear_child_tid:  AtomicU64::new(0),
        robust_list_head: AtomicU64::new(0),
        cpu:              AtomicU32::new(u32::MAX),
        on_cpu:           AtomicBool::new(false),
        run_link:         RunLink::new(),
        wait_link:        WaitLink::new(),
        entry_fn:         AtomicU64::new(entry_fn as *const () as u64),
        entry_arg:        AtomicU64::new(arg),
    }
}

/// Build the BSP "main thread" that wraps the existing boot stack.
///
/// The BSP is already executing on this stack when we construct the Thread,
/// so `ctx` is left zeroed — it will be populated the first time `schedule()`
/// switches away from this thread (saving live RSP/RBP/etc. into `ctx`).
#[cfg(not(test))]
pub(super) fn build_current_main_thread() -> Arc<Thread> {
    use sumi_abi::arch::layout::{KERNEL_STACK, KERNEL_STACK_SIZE};

    Arc::new(Thread {
        tid:  Tid(1),
        tgid: Tid(1),
        state:     AtomicU32::new(ThreadState::Running as u32),
        exit_code: AtomicI32::new(0),
        // ctx will be populated by the first __switch_to_asm save.
        ctx: UnsafeCell::new(ThreadContext {
            rsp: 0, rbp: 0, rbx: 0,
            r12: 0, r13: 0, r14: 0, r15: 0,
            rflags: 0, fxsave_area: FxsaveArea::new(),
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
        kernel_stack_top:  KERNEL_STACK.to_virtual(crate::KERNEL_ALLOCATOR.direct_map()),
        kernel_stack_phys: PhysicalAddr::new(KERNEL_STACK.as_usize() - KERNEL_STACK_SIZE),
        kernel_stack_size: KERNEL_STACK_SIZE,
        kernel_stack_freeable: false,
        user_stack_base: VirtualAddr::new(0),
        user_stack_size: 0,
        fs_base:          AtomicU64::new(0),
        clear_child_tid:  AtomicU64::new(0),
        robust_list_head: AtomicU64::new(0),
        cpu:              AtomicU32::new(0), // BSP is always cpu 0
        on_cpu:           AtomicBool::new(true), // already executing
        run_link:         RunLink::new(),
        wait_link:        WaitLink::new(),
        entry_fn:         AtomicU64::new(0),
        entry_arg:        AtomicU64::new(0),
    })
}

/// Build the BSP idle thread. Allocates a fresh 2 MB page as the stack and
/// sets up a trampoline frame for `idle_loop_entry`.
#[cfg(not(test))]
pub(super) fn build_idle_thread() -> Arc<Thread> {
    use sumi_abi::arch::layout::PAGE_SIZE;
    use crate::{KERNEL_ALLOCATOR, PAGE_ALLOCATOR};

    let stack_phys = match PAGE_ALLOCATOR.alloc(1) {
        Ok(p) => p,
        Err(_) => {
            crate::kprintln!("[sched] init_phase3_bsp: out of memory allocating idle stack");
            crate::arch::x86_64::hypercall::shutdown(1);
        }
    };
    let stack_top_virt = stack_phys
        .add(PAGE_SIZE)
        .to_virtual(KERNEL_ALLOCATOR.direct_map());

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

/// Build an idle thread for an AP that reuses its `AP_BOOT_STACKS[cpu_id]`
/// slot. The AP is already running on that stack, so `ctx.rsp = 0` — the
/// first `schedule()` call on the AP will save the live RSP into `ctx`.
#[cfg(not(test))]
pub(super) fn build_idle_thread_for_ap_reusing_boot_stack(cpu_id: u32) -> Arc<Thread> {
    use crate::sched::percpu::{AP_BOOT_STACK_SIZE, AP_BOOT_STACKS};

    // cpu_id < MAX_VCPUS (checked by ap_main_rust). Pointer arithmetic
    // avoids indexing the static array directly, which would trigger a
    // bounds-check in debug builds for a raw addr-of.
    let base = core::ptr::addr_of!(AP_BOOT_STACKS) as u64
        + (cpu_id as u64) * AP_BOOT_STACK_SIZE as u64;
    let stack_top_virt: u64 = base + AP_BOOT_STACK_SIZE as u64;

    let tid = super::registry::alloc_tid();
    Arc::new(Thread {
        tid,
        tgid: tid,
        state:     AtomicU32::new(ThreadState::Runnable as u32),
        exit_code: AtomicI32::new(0),
        // rsp = 0: the AP is live on this stack; schedule() will save
        // the real RSP on first switch-away.
        ctx: UnsafeCell::new(ThreadContext {
            rsp: 0, rbp: 0, rbx: 0,
            r12: 0, r13: 0, r14: 0, r15: 0,
            rflags: 0x202, fxsave_area: FxsaveArea::new(),
        }),
        kernel_stack_top:  VirtualAddr::new(stack_top_virt as usize),
        // AP boot stacks are not physical-memory objects tracked by palloc;
        // use zero as the physical address sentinel.
        kernel_stack_phys: PhysicalAddr::new(0),
        kernel_stack_size: AP_BOOT_STACK_SIZE,
        kernel_stack_freeable: false,
        user_stack_base: VirtualAddr::new(0),
        user_stack_size: 0,
        fs_base:          AtomicU64::new(0),
        clear_child_tid:  AtomicU64::new(0),
        robust_list_head: AtomicU64::new(0),
        cpu:              AtomicU32::new(cpu_id),
        on_cpu:           AtomicBool::new(true), // already executing
        run_link:         RunLink::new(),
        wait_link:        WaitLink::new(),
        entry_fn:         AtomicU64::new(0),
        entry_arg:        AtomicU64::new(0),
    })
}

/// Entry point for the idle thread. `extern "C" fn(u64) -> !` matches the
/// signature expected by `kthread_trampoline`. The `_arg` is unused.
#[cfg(not(test))]
extern "C" fn idle_loop_entry(_arg: u64) -> ! {
    super::idle_loop()
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
