use crate::syscall::errno::*;
use crate::syscall::{SyscallArgs, SyscallResult};
use core::sync::atomic::Ordering;

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
    let new_set = args.arg1 as *const u64;
    let old_set = args.arg2 as *mut u64;
    let sigsetsize = args.arg3;

    if sigsetsize != 8 {
        return EINVAL;
    }
    if !new_set.is_null() && how != SIG_BLOCK && how != SIG_UNBLOCK && how != SIG_SETMASK {
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

/// Wait for one of the requested signals. sumi does not deliver POSIX
/// signals, but runtimes may still create a signal-handling thread and call
/// sigwait()/sigwaitinfo() forever. A timed wait reports "nothing pending";
/// an untimed wait parks the caller so it does not spin or treat ENOSYS as a
/// fatal signal subsystem failure.
pub fn sys_rt_sigtimedwait(args: &SyscallArgs) -> SyscallResult {
    let set = args.arg0 as *const u64;
    let timeout = args.arg2 as *const Timespec;
    let sigsetsize = args.arg3;

    if sigsetsize != 8 {
        return EINVAL;
    }
    if set.is_null() {
        return EFAULT;
    }

    // Force the same basic pointer contract as Linux: a bad set pointer
    // faults here instead of silently accepting an unreadable mask.
    let _mask = unsafe { core::ptr::read_volatile(set) };

    if !timeout.is_null() {
        return EAGAIN;
    }

    let me = crate::sched::current_thread();
    me.state
        .store(crate::sched::ThreadState::Blocked as u32, Ordering::Release);
    crate::sched::schedule();

    // No signal delivery exists today. If this thread is ever woken by a
    // future implementation, preserve the "nothing pending" result.
    EAGAIN
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

#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_sigprocmask_args(how: u64, new_set: u64, old_set: u64, sigsetsize: u64) -> SyscallArgs {
        SyscallArgs {
            nr: 14,
            arg0: how,
            arg1: new_set,
            arg2: old_set,
            arg3: sigsetsize,
            arg4: 0,
            arg5: 0,
            caller_rip: 0,
            caller_rflags: 0,
        }
    }

    fn make_sigtimedwait_args(set: u64, timeout: u64, sigsetsize: u64) -> SyscallArgs {
        SyscallArgs {
            nr: 128,
            arg0: set,
            arg1: 0,
            arg2: timeout,
            arg3: sigsetsize,
            arg4: 0,
            arg5: 0,
            caller_rip: 0,
            caller_rflags: 0,
        }
    }

    #[test]
    fn sigprocmask_invalid_how_with_new_set_returns_einval() {
        let set = 0u64;
        let args = make_sigprocmask_args(!0, &set as *const u64 as u64, 0, 8);
        assert_eq!(sys_rt_sigprocmask(&args), EINVAL);
    }

    #[test]
    fn sigprocmask_null_new_set_ignores_how_but_writes_old_set() {
        let mut old = 0xDEAD_BEEFu64;
        let args = make_sigprocmask_args(!0, 0, &mut old as *mut u64 as u64, 8);
        assert_eq!(sys_rt_sigprocmask(&args), 0);
        assert_eq!(old, 0);
    }

    #[test]
    fn sigprocmask_rejects_wrong_sigsetsize() {
        let args = make_sigprocmask_args(SIG_BLOCK, 0, 0, 16);
        assert_eq!(sys_rt_sigprocmask(&args), EINVAL);
    }

    #[test]
    fn sigtimedwait_timed_wait_reports_no_pending_signal() {
        let set = 1u64 << 10;
        let timeout = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let args = make_sigtimedwait_args(
            &set as *const u64 as u64,
            &timeout as *const Timespec as u64,
            8,
        );
        assert_eq!(sys_rt_sigtimedwait(&args), EAGAIN);
    }

    #[test]
    fn sigtimedwait_rejects_wrong_sigsetsize() {
        let set = 1u64;
        let timeout = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let args = make_sigtimedwait_args(
            &set as *const u64 as u64,
            &timeout as *const Timespec as u64,
            16,
        );
        assert_eq!(sys_rt_sigtimedwait(&args), EINVAL);
    }

    #[test]
    fn sigtimedwait_rejects_null_set() {
        let timeout = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let args = make_sigtimedwait_args(0, &timeout as *const Timespec as u64, 8);
        assert_eq!(sys_rt_sigtimedwait(&args), EFAULT);
    }
}
