use crate::syscall::errno::*;
use crate::syscall::{SyscallArgs, SyscallResult};

const CLOCK_REALTIME: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;
const CLOCK_MONOTONIC_RAW: i32 = 4;
const CLOCK_REALTIME_COARSE: i32 = 5;
const CLOCK_MONOTONIC_COARSE: i32 = 6;
const TIMER_ABSTIME: i32 = 1;

#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[repr(C)]
struct Timeval {
    tv_sec: i64,
    tv_usec: i64,
}

pub fn sys_nanosleep(args: &SyscallArgs) -> SyscallResult {
    let req = args.arg0 as *const Timespec;
    let rem = args.arg1 as *mut Timespec;

    if req.is_null() {
        return EFAULT;
    }

    // SAFETY: The caller (user program running in ring-0) is responsible for
    // passing valid, aligned pointers. Invalid pointers will cause a fault.
    let (sec, nsec) = unsafe { ((*req).tv_sec, (*req).tv_nsec) };

    if sec < 0 || nsec < 0 || nsec >= 1_000_000_000 {
        return EINVAL;
    }

    let total_ns = ((sec as u128) * 1_000_000_000 + nsec as u128).min(u64::MAX as u128) as u64;
    crate::time::busy_wait_ns(total_ns);

    // Write zero remainder — we never get interrupted in a single-threaded unikernel.
    if !rem.is_null() {
        // SAFETY: The caller (user program running in ring-0) is responsible for
        // passing valid, aligned pointers. Invalid pointers will cause a fault.
        unsafe {
            (*rem).tv_sec = 0;
            (*rem).tv_nsec = 0;
        }
    }

    0
}

pub fn sys_gettimeofday(args: &SyscallArgs) -> SyscallResult {
    let tv = args.arg0 as *mut Timeval;

    // Null tv is valid per POSIX — caller just wants to check the call works.
    if tv.is_null() {
        return 0;
    }

    let (sec, nsec) = crate::time::clock_realtime();
    // SAFETY: The caller (user program running in ring-0) is responsible for
    // passing valid, aligned pointers. Invalid pointers will cause a fault.
    unsafe {
        (*tv).tv_sec = sec as i64;
        (*tv).tv_usec = (nsec / 1000) as i64;
    }

    0
}

pub fn sys_getrlimit(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_clock_gettime(args: &SyscallArgs) -> SyscallResult {
    let clock_id = args.arg0 as i32;
    let tp = args.arg1 as *mut Timespec;

    if tp.is_null() {
        return EFAULT;
    }

    let (sec, nsec) = match clock_id {
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE => crate::time::clock_realtime(),
        CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE => {
            crate::time::clock_monotonic()
        }
        _ => return EINVAL,
    };

    // SAFETY: The caller (user program running in ring-0) is responsible for
    // passing valid, aligned pointers. Invalid pointers will cause a fault.
    unsafe {
        (*tp).tv_sec = sec as i64;
        (*tp).tv_nsec = nsec as i64;
    }

    0
}

pub fn sys_clock_getres(args: &SyscallArgs) -> SyscallResult {
    let clock_id = args.arg0 as i32;
    let res_tp = args.arg1 as *mut Timespec;

    // Validate clock_id first regardless of whether res_tp is null.
    match clock_id {
        CLOCK_REALTIME
        | CLOCK_MONOTONIC
        | CLOCK_MONOTONIC_RAW
        | CLOCK_REALTIME_COARSE
        | CLOCK_MONOTONIC_COARSE => {}
        _ => return EINVAL,
    }

    // Null tp is valid — caller just wants to validate the clock_id.
    if res_tp.is_null() {
        return 0;
    }

    let res_ns = crate::time::clock_resolution_ns();
    // SAFETY: The caller (user program running in ring-0) is responsible for
    // passing valid, aligned pointers. Invalid pointers will cause a fault.
    unsafe {
        (*res_tp).tv_sec = 0;
        (*res_tp).tv_nsec = res_ns as i64;
    }

    0
}

pub fn sys_clock_nanosleep(args: &SyscallArgs) -> SyscallResult {
    let clock_id = args.arg0 as i32;
    let flags = args.arg1 as i32;
    let req = args.arg2 as *const Timespec;
    let rem = args.arg3 as *mut Timespec;

    match clock_id {
        CLOCK_REALTIME | CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW => {}
        _ => return EINVAL,
    }

    if req.is_null() {
        return EFAULT;
    }

    // SAFETY: The caller (user program running in ring-0) is responsible for
    // passing valid, aligned pointers. Invalid pointers will cause a fault.
    let (sec, nsec) = unsafe { ((*req).tv_sec, (*req).tv_nsec) };

    if sec < 0 || nsec < 0 || nsec >= 1_000_000_000 {
        return EINVAL;
    }

    if flags & TIMER_ABSTIME != 0 {
        // Absolute sleep: wait until the clock reaches the target time.
        let target_ns =
            ((sec as u128) * 1_000_000_000 + nsec as u128).min(u64::MAX as u128) as u64;
        let now_ns = match clock_id {
            CLOCK_REALTIME => {
                let (s, n) = crate::time::clock_realtime();
                ((s as u128) * 1_000_000_000 + n as u128).min(u64::MAX as u128) as u64
            }
            _ => crate::time::monotonic_ns(),
        };
        if target_ns > now_ns {
            crate::time::busy_wait_ns(target_ns - now_ns);
        }
    } else {
        // Relative sleep.
        let total_ns =
            ((sec as u128) * 1_000_000_000 + nsec as u128).min(u64::MAX as u128) as u64;
        crate::time::busy_wait_ns(total_ns);

        // Write zero remainder — we never get interrupted.
        if !rem.is_null() {
            // SAFETY: The caller (user program running in ring-0) is responsible for
            // passing valid, aligned pointers. Invalid pointers will cause a fault.
            unsafe {
                (*rem).tv_sec = 0;
                (*rem).tv_nsec = 0;
            }
        }
    }

    0
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
        // SAFETY: The caller (user program running in ring-0) is responsible for
        // passing valid, aligned pointers. Invalid pointers will cause a fault.
        unsafe {
            *old_limit = val;
        }
    }
    0
}
