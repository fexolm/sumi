use crate::syscall::errno::*;
use crate::syscall::{SyscallArgs, SyscallResult};

/// Kernel's sigaction struct layout (matches Linux kernel, NOT libc).
/// musl's rt_sigaction syscall wrapper passes this directly.
#[repr(C)]
struct KernelSigaction {
    sa_handler: u64,
    sa_flags: u64,
    sa_restorer: u64,
    sa_mask: u64, // simplified: first 64 bits of signal mask
}

const SIG_DFL: u64 = 0;

/// Pretend to install signal handlers. We store nothing — signals are never
/// delivered in the unikernel — but we must write old_act when requested,
/// otherwise callers read garbage and may spin.
pub fn sys_rt_sigaction(args: &SyscallArgs) -> SyscallResult {
    let _signum = args.arg0;
    let _new_act = args.arg1 as *const KernelSigaction;
    let old_act = args.arg2 as *mut KernelSigaction;

    if !old_act.is_null() {
        // SAFETY: User passed a valid pointer for the old action.
        unsafe {
            (*old_act).sa_handler = SIG_DFL;
            (*old_act).sa_flags = 0;
            (*old_act).sa_restorer = 0;
            (*old_act).sa_mask = 0;
        }
    }
    0
}

const SIG_BLOCK: u64 = 0;
const SIG_UNBLOCK: u64 = 1;
const SIG_SETMASK: u64 = 2;

/// No-op: single-threaded unikernel, signal mask is meaningless — but `how`
/// and `sigsetsize` are validated exactly like Linux does. mysqld's startup
/// sanity probe deliberately calls this with an invalid `how` (`~0`) and
/// aborts if it doesn't get EINVAL back, so silently succeeding on garbage
/// input is not an option here. When old_set is provided, write an empty
/// mask.
pub fn sys_rt_sigprocmask(args: &SyscallArgs) -> SyscallResult {
    let how = args.arg0;
    let old_set = args.arg2 as *mut u64;
    let sigsetsize = args.arg3;

    if how != SIG_BLOCK && how != SIG_UNBLOCK && how != SIG_SETMASK {
        return EINVAL;
    }
    if sigsetsize != 8 {
        return EINVAL;
    }

    if !old_set.is_null() {
        // SAFETY: User passed a valid pointer for the old signal mask.
        unsafe {
            *old_set = 0;
        }
    }
    0
}

pub fn sys_rt_sigreturn(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_pause(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_kill(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_rt_sigsuspend(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_rt_sigpending(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

/// No-op: unikernel has no signal delivery.
pub fn sys_tkill(_args: &SyscallArgs) -> SyscallResult {
    0
}

/// No-op: unikernel has no signal delivery.
pub fn sys_tgkill(_args: &SyscallArgs) -> SyscallResult {
    0
}

/// No-op: unikernel has no alternate signal stack.
/// When old_ss is provided, report SS_DISABLE.
pub fn sys_sigaltstack(args: &SyscallArgs) -> SyscallResult {
    let old_ss = args.arg1 as *mut StackT;
    if !old_ss.is_null() {
        // SAFETY: User passed a valid pointer for the old stack.
        unsafe {
            (*old_ss).ss_sp = 0;
            (*old_ss).ss_flags = SS_DISABLE;
            (*old_ss).ss_size = 0;
        }
    }
    0
}

#[repr(C)]
struct StackT {
    ss_sp: u64,
    ss_flags: i32,
    ss_size: u64,
}

const SS_DISABLE: i32 = 2;
