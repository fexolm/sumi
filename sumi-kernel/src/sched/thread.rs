//! Thread descriptor and associated layout types.
//!
//! See `docs/design/multithreading-v2.md`.

extern crate alloc;

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, AtomicU64};

use sumi_abi::address::{PhysicalAddr, VirtualAddr};

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
/// that mask.
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
    pub rsp: u64,                // 0x00
    pub rbp: u64,                // 0x08
    pub rbx: u64,                // 0x10
    pub r12: u64,                // 0x18
    pub r13: u64,                // 0x20
    pub r14: u64,                // 0x28
    pub r15: u64,                // 0x30
    pub rflags: u64,             // 0x38
    pub fxsave_area: FxsaveArea, // 0x40, 512 bytes, 16-aligned.
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

/// Intrusive node for futex wait queues.
#[repr(C)]
pub struct WaitLink {
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
    /// True if kernel_stack_phys was allocated from KERNEL_ALLOCATOR and should
    /// be freed by the reaper when this thread is destroyed.
    pub kernel_stack_freeable: bool,

    /// Zero for kthreads. Populated by clone().
    pub user_stack_base: VirtualAddr,
    pub user_stack_size: usize,

    /// Per-thread FS base for TLS, written by arch_prctl / clone.
    pub fs_base: AtomicU64,

    /// Address to write 0 on thread exit (set_tid_address).
    pub clear_child_tid: AtomicU64,

    /// Robust futex list head (set_robust_list).
    pub robust_list_head: AtomicU64,

    /// Last CPU this thread ran on. `u32::MAX` = "never ran".
    pub cpu: AtomicU32,

    /// True from the moment this thread is chosen as `next` in `schedule()`
    /// until `__switch_to_asm` finishes capturing its context. Consumers that might hand this
    /// thread's `ctx.rsp` to a *different* CPU's `__switch_to_asm` — or
    /// free its kernel stack — must spin until this is `false`:
    /// `wake_blocked`, `try_steal_work`, and `reap_zombies`.
    pub on_cpu: AtomicBool,

    pub run_link: RunLink,
    pub wait_link: WaitLink,

    // Kthread trampoline payload. Kthreads set these at spawn time;
    // `kthread_trampoline` reads them once on first schedule-in. User
    // threads leave both at zero; clone() does not touch them.
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
    // holds now that `ctx` embeds a 512-byte FXSAVE area; the asm
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
