use crate::syscall::errno::*;
use crate::syscall::{SyscallArgs, SyscallResult};

pub fn sys_nanosleep(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_gettimeofday(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_getrlimit(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_clock_gettime(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_clock_getres(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_clock_nanosleep(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_prlimit64(args: &SyscallArgs) -> SyscallResult {
    #[repr(C)]
    struct Rlimit {
        rlim_cur: u64,
        rlim_max: u64,
    }

    const RLIMIT_STACK: u64 = 3;
    const RLIMIT_NOFILE: u64 = 7;

    let _pid = args.arg0;
    let resource = args.arg1;
    let _new_limit = args.arg2 as *const Rlimit;
    let old_limit = args.arg3 as *mut Rlimit;

    if !old_limit.is_null() {
        let val = match resource {
            RLIMIT_STACK => Rlimit {
                rlim_cur: 8 * 1024 * 1024,
                rlim_max: u64::MAX,
            },
            RLIMIT_NOFILE => Rlimit {
                rlim_cur: 256,
                rlim_max: 256,
            },
            _ => Rlimit {
                rlim_cur: u64::MAX,
                rlim_max: u64::MAX,
            },
        };
        // SAFETY: User passed a valid pointer for the result.
        unsafe {
            *old_limit = val;
        }
    }
    0
}
