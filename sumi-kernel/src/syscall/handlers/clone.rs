//! `clone(2)` and `clone3(2)` implementation.
//!
//! See `docs/design/multithreading-v2.md`.

use crate::syscall::errno::{EINVAL, ENOMEM};
use crate::syscall::{SyscallArgs, SyscallResult};

// Linux x86_64 clone() flag bits currently used by the pthread-style thread
// shape supported by sumi.
const CLONE_VM: u64 = 0x0000_0100;
const CLONE_FS: u64 = 0x0000_0200;
const CLONE_FILES: u64 = 0x0000_0400;
const CLONE_SIGHAND: u64 = 0x0000_0800;
const CLONE_THREAD: u64 = 0x0001_0000;
const CLONE_SETTLS: u64 = 0x0008_0000;
const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;
const CLONE_CHILD_SETTID: u64 = 0x0100_0000;

/// Required pthread-style flag set. Any missing bit → `EINVAL`.
const REQUIRED: u64 = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD;

/// Parent register state to restore in the child before user code runs.
///
/// The child's initial stack frame stores these so `thread_entry_trampoline`
/// can restore them. This is required for glibc's clone3 child stub, which
/// reads `rdx` (thread fn) and `r8` (thread arg) without re-loading them
/// from the child stack — exactly as the parent had them at `syscall` time.
struct ParentRegs {
    arg0: u64, // rdi
    arg1: u64, // rsi
    arg2: u64, // rdx
    arg3: u64, // r10
    arg4: u64, // r8
    arg5: u64, // r9
    caller_rip: u64,
    caller_rflags: u64,
}

/// Allocator used for a new child thread's kernel stack.
/// Real: the production `KERNEL_ALLOCATOR`, backed by KVM guest RAM. Host
/// stand-in: a `KernelAllocator<TestDirectMap>` sized to comfortably cover
/// every `sys_clone`/`sys_clone3` call across this test binary.
#[cfg(not(test))]
fn clone_thread_allocator()
-> &'static crate::memory::alloc::kmalloc::KernelAllocator<'static, crate::arch::KernelDirectMap> {
    &crate::KERNEL_ALLOCATOR
}

#[cfg(test)]
fn clone_thread_allocator() -> &'static crate::memory::alloc::kmalloc::KernelAllocator<
    'static,
    crate::memory::test_utils::TestDirectMap,
> {
    use crate::memory::{
        alloc::{kmalloc::KernelAllocator, palloc::PageAllocator},
        test_utils::TestDirectMap,
    };

    static TEST_DM: spin::Once<TestDirectMap> = spin::Once::new();
    static TEST_PA: spin::Once<PageAllocator> = spin::Once::new();
    static TEST_ALLOC: spin::Once<KernelAllocator<'static, TestDirectMap>> = spin::Once::new();

    TEST_ALLOC.call_once(|| {
        let dm = TEST_DM.call_once(|| TestDirectMap::new(32));
        let pa = TEST_PA.call_once(PageAllocator::new);
        KernelAllocator::new(dm, pa)
    })
}

/// Core clone implementation shared by `sys_clone` and `sys_clone3`.
///
/// `child_stack_top` is the child's user rsp. `regs` holds the parent's
/// register state at syscall time; it is saved in the child's initial frame
/// so the trampoline can restore it before user code runs.
///
/// All pointer and flag validation must be done by the caller before invoking
/// this function.
fn do_clone(
    flags: u64,
    child_stack_top: u64,
    ptid_ptr: *mut i32,
    ctid_ptr: *mut i32,
    tls: u64,
    regs: ParentRegs,
) -> SyscallResult {
    use crate::sched::{
        clone::{CloneError, InitialFrame, clone_create_user_thread},
        current_thread, registry,
    };
    use core::sync::atomic::Ordering;
    use sumi_abi::address::VirtualAddr;

    let parent = current_thread();
    let tid = registry::alloc_tid();
    let tgid = parent.tgid;

    let fs_base = if flags & CLONE_SETTLS != 0 { tls } else { 0 };
    // Reject non-canonical fs_base. wrmsr in __switch_to_asm would #GP.
    // Match arch_prctl(ARCH_SET_FS) semantics — return EINVAL.
    if (fs_base >> 47) != 0 {
        return EINVAL;
    }
    let clear_ctid = if flags & CLONE_CHILD_CLEARTID != 0 {
        ctid_ptr as u64
    } else {
        0
    };

    let frame = InitialFrame {
        arg0: regs.arg0,
        arg1: regs.arg1,
        arg2: regs.arg2,
        arg3: regs.arg3,
        arg4: regs.arg4,
        arg5: regs.arg5,
        user_rip: regs.caller_rip,
        user_rflags: regs.caller_rflags,
        user_rsp: child_stack_top,
    };

    let child = match clone_create_user_thread(
        tid,
        tgid,
        frame,
        fs_base,
        VirtualAddr::new(0),
        0,
        clear_ctid,
        clone_thread_allocator(),
    ) {
        Ok(t) => t,
        Err(CloneError::OutOfMemory) => return ENOMEM,
    };

    if flags & CLONE_PARENT_SETTID != 0 && !ptid_ptr.is_null() {
        // SAFETY: same-address-space unikernel; bad pointer traps via #PF.
        unsafe {
            ptid_ptr.write(tid.0 as i32);
        }
    }
    if flags & CLONE_CHILD_SETTID != 0 && !ctid_ptr.is_null() {
        // SAFETY: same as above.
        unsafe {
            ctid_ptr.write(tid.0 as i32);
        }
    }

    child.state.store(
        crate::sched::thread::ThreadState::Runnable as u32,
        Ordering::Release,
    );
    registry::register(child.clone());
    // Counts toward the "last live user thread" check in sys_exit.
    registry::LIVE_USER_THREADS.fetch_add(1, Ordering::Relaxed);

    let cpu = crate::sched::percpu::this_cpu();
    cpu.runqueue.push(&child);
    cpu.need_resched.store(true, Ordering::Release);

    tid.0 as i64
}

/// Linux `clone(2)`.
///
/// Arguments (Linux ABI):
///   rdi (arg0) : flags
///   rsi (arg1) : child_stack (user-space top)
///   rdx (arg2) : parent_tid (*int)
///   r10 (arg3) : child_tid  (*int)
///   r8  (arg4) : tls
///
/// Returns the new TID to the parent. The child observes `rax = 0` on
/// first return from the syscall — that path is driven by
/// `thread_entry_trampoline` (see `sched::clone`), NOT by this handler.
pub fn sys_clone(args: &SyscallArgs) -> SyscallResult {
    let flags = args.arg0;
    let child_rsp = args.arg1;
    let ptid_ptr = args.arg2 as *mut i32;
    let ctid_ptr = args.arg3 as *mut i32;
    let tls = args.arg4;

    if flags & REQUIRED != REQUIRED {
        return EINVAL;
    }
    if child_rsp == 0 {
        return EINVAL;
    }

    let regs = ParentRegs {
        arg0: args.arg0,
        arg1: args.arg1,
        arg2: args.arg2,
        arg3: args.arg3,
        arg4: args.arg4,
        arg5: args.arg5,
        caller_rip: args.caller_rip,
        caller_rflags: args.caller_rflags,
    };
    do_clone(flags, child_rsp, ptid_ptr, ctid_ptr, tls, regs)
}

/// Linux `clone_args` v0 (CLONE_ARGS_SIZE_VER0 = 64).
#[repr(C)]
#[derive(Clone, Copy)]
struct CloneArgs {
    flags: u64,       // +  0
    pidfd: u64,       // +  8
    child_tid: u64,   // + 16
    parent_tid: u64,  // + 24
    exit_signal: u64, // + 32
    stack: u64,       // + 40  (stack BASE for clone3, not top)
    stack_size: u64,  // + 48
    tls: u64,         // + 56
}
const CLONE_ARGS_SIZE_VER0: usize = 64;
const _: () = assert!(core::mem::size_of::<CloneArgs>() == CLONE_ARGS_SIZE_VER0);

/// Linux `clone3(2)`. Supports the v0 struct.
/// Newer trailing fields (set_tid, set_tid_size, cgroup) are ignored if
/// the user passes a larger size.
pub fn sys_clone3(args: &SyscallArgs) -> SyscallResult {
    let ca_ptr = args.arg0 as *const CloneArgs;
    let size = args.arg1 as usize;

    if ca_ptr.is_null() {
        return EINVAL;
    }
    // Linux rejects sizes smaller than the v0 struct.
    if size < CLONE_ARGS_SIZE_VER0 {
        return EINVAL;
    }

    // SAFETY: unikernel shares address space; bad pointer traps via #PF.
    // Read unaligned to be defensive — clone3 ABI doesn't guarantee
    // alignment beyond what the libc wrapper provides.
    let ca: CloneArgs = unsafe { core::ptr::read_unaligned(ca_ptr) };

    let stack_top = match ca.stack.checked_add(ca.stack_size) {
        Some(v) => v,
        None => return EINVAL,
    };

    if ca.exit_signal != 0 {
        // We don't support fork-style children that signal the parent on
        // exit. pthread_create passes 0; raw clone3 callers must too.
        return EINVAL;
    }

    if ca.flags & REQUIRED != REQUIRED {
        return EINVAL;
    }
    if stack_top == 0 {
        return EINVAL;
    }

    let ptid_ptr = ca.parent_tid as *mut i32;
    let ctid_ptr = ca.child_tid as *mut i32;

    // For clone3, the parent's syscall argument registers are in args.arg0-arg5.
    // These include rdx (thread fn) and r8 (thread arg) that glibc's
    // clone3 child stub expects to find restored when the child runs.
    let regs = ParentRegs {
        arg0: args.arg0,
        arg1: args.arg1,
        arg2: args.arg2,
        arg3: args.arg3,
        arg4: args.arg4,
        arg5: args.arg5,
        caller_rip: args.caller_rip,
        caller_rflags: args.caller_rflags,
    };
    do_clone(ca.flags, stack_top, ptid_ptr, ctid_ptr, ca.tls, regs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(flags: u64, child_stack: u64) -> SyscallArgs {
        SyscallArgs {
            nr: 56,
            arg0: flags,
            arg1: child_stack,
            arg2: 0,
            arg3: 0,
            arg4: 0,
            arg5: 0,
            caller_rip: 0xdead_beef,
            caller_rflags: 0x202,
        }
    }

    #[test]
    fn missing_clone_vm_returns_einval() {
        let a = args(
            CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD,
            0x1000,
        );
        assert_eq!(sys_clone(&a), EINVAL);
    }

    #[test]
    fn missing_clone_thread_returns_einval() {
        let a = args(CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND, 0x1000);
        assert_eq!(sys_clone(&a), EINVAL);
    }

    #[test]
    fn zero_flags_returns_einval() {
        let a = args(0, 0x1000);
        assert_eq!(sys_clone(&a), EINVAL);
    }

    #[test]
    fn null_stack_returns_einval() {
        let a = args(REQUIRED, 0);
        assert_eq!(sys_clone(&a), EINVAL);
    }

    /// Real `do_clone` needs a current thread (for `parent.tgid`) and
    /// an initialised per-cpu (for the runqueue push) — install both, like
    /// `init_phase3_bsp` does in production before any clone is reachable.
    fn install_parent() {
        let t: &'static crate::sched::thread::Thread =
            Box::leak(Box::new(crate::sched::thread::Thread::new_test(9201)));
        crate::sched::percpu::install_test_thread(0, t);
    }

    #[test]
    fn minimum_valid_flags_pass_validation() {
        install_parent();
        // Real do_clone now runs end-to-end and returns the new child's TID
        // (never 0/1, which are reserved).
        let a = args(REQUIRED, 0x1000);
        assert!(sys_clone(&a) > 1);
    }

    #[test]
    fn extra_ignored_flags_accepted() {
        install_parent();
        // CLONE_PARENT_SETTID (0x0010_0000) is allowed in addition to REQUIRED.
        let a = args(REQUIRED | 0x0010_0000, 0x1000);
        assert!(sys_clone(&a) > 1);
    }

    fn clone3_args_struct(flags: u64, stack: u64, stack_size: u64, exit_signal: u64) -> CloneArgs {
        CloneArgs {
            flags,
            pidfd: 0,
            child_tid: 0,
            parent_tid: 0,
            exit_signal,
            stack,
            stack_size,
            tls: 0,
        }
    }

    fn clone3_syscall_args(ca: &CloneArgs, size: u64) -> SyscallArgs {
        SyscallArgs {
            nr: 435,
            arg0: ca as *const CloneArgs as u64,
            arg1: size,
            arg2: 0,
            arg3: 0,
            arg4: 0,
            arg5: 0,
            caller_rip: 0xdead_beef,
            caller_rflags: 0x202,
        }
    }

    #[test]
    fn clone3_too_small_returns_einval() {
        let ca = clone3_args_struct(REQUIRED, 0x1000, 0x2000, 0);
        let args = clone3_syscall_args(&ca, 63);
        assert_eq!(sys_clone3(&args), EINVAL);
    }

    #[test]
    fn clone3_null_args_returns_einval() {
        let args = SyscallArgs {
            nr: 435,
            arg0: 0,
            arg1: 64,
            arg2: 0,
            arg3: 0,
            arg4: 0,
            arg5: 0,
            caller_rip: 0,
            caller_rflags: 0,
        };
        assert_eq!(sys_clone3(&args), EINVAL);
    }

    #[test]
    fn clone3_v0_minimum_ok() {
        install_parent();
        // Real do_clone3 now runs end-to-end and returns the new child's TID.
        let ca = clone3_args_struct(REQUIRED, 0x1000, 0x2000, 0);
        let args = clone3_syscall_args(&ca, 64);
        assert!(sys_clone3(&args) > 1);
    }

    #[test]
    fn clone3_stack_overflow_returns_einval() {
        let ca = clone3_args_struct(REQUIRED, u64::MAX, 1, 0);
        let args = clone3_syscall_args(&ca, 64);
        assert_eq!(sys_clone3(&args), EINVAL);
    }

    #[test]
    fn clone3_nonzero_exit_signal_returns_einval() {
        let ca = clone3_args_struct(REQUIRED, 0x1000, 0x2000, 17);
        let args = clone3_syscall_args(&ca, 64);
        assert_eq!(sys_clone3(&args), EINVAL);
    }

    #[test]
    fn clone3_missing_required_flags_returns_einval() {
        let ca = clone3_args_struct(0, 0x1000, 0x2000, 0);
        let args = clone3_syscall_args(&ca, 64);
        assert_eq!(sys_clone3(&args), EINVAL);
    }
}
