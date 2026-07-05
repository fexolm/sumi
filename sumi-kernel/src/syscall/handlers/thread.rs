use crate::syscall::errno::*;
use crate::syscall::{SyscallArgs, SyscallResult};

/// Yield the CPU to another runnable thread.
///
/// Fast path: if the runqueue is empty, return immediately (no context
/// switch). Otherwise push self to the tail and call schedule() so the next
/// thread at the head runs.
pub fn sys_sched_yield(_args: &SyscallArgs) -> SyscallResult {
    use core::sync::atomic::Ordering;
    let cpu = crate::sched::percpu::this_cpu();
    // Fast path: nothing else is runnable locally. In SMP mode, first try
    // to pull a runnable thread back from a peer CPU; otherwise a thread that
    // repeatedly yields can spin locally while work is stranded elsewhere.
    if cpu.runqueue.load() == 0 {
        let _ = crate::sched::try_steal_work();
        if cpu.runqueue.load() == 0 {
            return 0;
        }
    }
    let current = crate::sched::current_thread();
    // The idle thread must never call sched_yield directly.
    let idle_ptr = cpu.idle_thread.load(Ordering::Relaxed);
    if core::ptr::eq(current as *const _, idle_ptr as *const _) {
        return 0;
    }
    cpu.runqueue.push(current);
    crate::sched::schedule();
    0
}

const FUTEX_WAIT: i32 = 0;
const FUTEX_WAKE: i32 = 1;
const FUTEX_WAIT_BITSET: i32 = 9;
const FUTEX_WAKE_BITSET: i32 = 10;
const FUTEX_PRIVATE_FLAG: i32 = 128;
const FUTEX_CLOCK_REALTIME: i32 = 256;
const FUTEX_CMD_MASK: i32 = !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);

/// Futex: WAIT blocks until a matching WAKE; WAKE wakes up to `val` waiters.
/// Scheduler-integrated (see `sched::futex`): `FUTEX_WAIT` on a mismatched value returns
/// `EAGAIN` immediately, same as production; actually blocking still needs
/// a second thread to wake it, so most host tests exercise the bookkeeping
/// via `sched::futex` directly rather than through this dispatch.
pub fn sys_futex(args: &SyscallArgs) -> SyscallResult {
    use core::sync::atomic::{AtomicU32, Ordering};

    let uaddr = args.arg0 as *const u32;
    let op = args.arg1 as i32;
    let val = args.arg2 as u32;
    let cmd = op & FUTEX_CMD_MASK;

    if uaddr.is_null() {
        return EFAULT;
    }

    let timed_probe = |bitset: Option<u32>| -> SyscallResult {
        if matches!(bitset, Some(0)) {
            return EINVAL;
        }
        let cur = unsafe { AtomicU32::from_ptr(uaddr as *mut u32).load(Ordering::Acquire) };
        if cur != val { EAGAIN } else { ETIMEDOUT }
    };

    match cmd {
        FUTEX_WAIT if args.arg3 != 0 => timed_probe(None),
        FUTEX_WAIT => crate::sched::futex::wait(uaddr, val),
        FUTEX_WAKE => crate::sched::futex::wake(uaddr, val),
        FUTEX_WAIT_BITSET => {
            // arg3 (r10) = timeout pointer. Timed waits do not enqueue a
            // waiter yet; report timeout after the required value check.
            // arg4 (r8)  = unused
            // arg5 (r9)  = bitset
            let bitset = args.arg5 as u32;
            if args.arg3 != 0 {
                timed_probe(Some(bitset))
            } else {
                crate::sched::futex::wait_bitset(uaddr, val, bitset)
            }
        }
        FUTEX_WAKE_BITSET => {
            // arg5 (r9) = bitset
            let bitset = args.arg5 as u32;
            crate::sched::futex::wake_bitset(uaddr, val, bitset)
        }
        _ => ENOSYS,
    }
}

/// `sched_getaffinity(pid, cpusetsize, mask)`. sumi has no CPU affinity
/// concept — every thread can run on every vCPU — so this reports a mask
/// with bits `0..cpu_count()` set, matching a system with no affinity mask
/// ever applied. `pid` must name this process (0 = caller, or our fixed
/// pid 1); anything else is ESRCH, same as `sys_prlimit64`'s pid handling.
pub fn sys_sched_getaffinity(args: &SyscallArgs) -> SyscallResult {
    let pid = args.arg0 as i64;
    let cpusetsize = args.arg1 as usize;
    let mask_ptr = args.arg2 as *mut u8;

    if pid != 0 && pid != 1 {
        return ESRCH;
    }
    if cpusetsize < 8 {
        return EINVAL;
    }
    if mask_ptr.is_null() {
        return EFAULT;
    }

    let n = crate::sched::cpu_count() as usize;
    let write_len = cpusetsize.min(128);

    // SAFETY: mask_ptr..+write_len is a valid, writable user buffer per the
    // syscall ABI (glibc's CPU_ALLOC/CPU_SET always sizes it >= cpusetsize).
    unsafe {
        core::ptr::write_bytes(mask_ptr, 0, write_len);
        for cpu in 0..n {
            let byte = cpu / 8;
            if byte >= write_len {
                break;
            }
            *mask_ptr.add(byte) |= 1u8 << (cpu % 8);
        }
    }
    8
}

pub fn sys_sched_setaffinity(args: &SyscallArgs) -> SyscallResult {
    let pid = args.arg0 as i64;
    let cpusetsize = args.arg1 as usize;
    let mask_ptr = args.arg2 as *const u8;

    if pid != 0 && pid != 1 {
        return ESRCH;
    }
    if cpusetsize > 0 && mask_ptr.is_null() {
        return EFAULT;
    }
    0
}

pub fn sys_getcpu(args: &SyscallArgs) -> SyscallResult {
    let cpu_ptr = args.arg0 as *mut u32;
    let node_ptr = args.arg1 as *mut u32;

    // sumi currently exposes one NUMA node. Returning CPU 0 is enough for
    // libraries that use getcpu() only for cache sharding or diagnostics.
    unsafe {
        if !cpu_ptr.is_null() {
            *cpu_ptr = 0;
        }
        if !node_ptr.is_null() {
            *node_ptr = 0;
        }
    }
    0
}

pub fn sys_set_robust_list(args: &SyscallArgs) -> SyscallResult {
    // glibc always passes sizeof(struct robust_list_head) = 24.
    const ROBUST_LIST_HEAD_SIZE: u64 = 24;
    if args.arg1 != ROBUST_LIST_HEAD_SIZE {
        return EINVAL;
    }
    use core::sync::atomic::Ordering;
    crate::sched::current_thread()
        .robust_list_head
        .store(args.arg0, Ordering::Relaxed);
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_args(uaddr: u64, op: u64, val: u64) -> SyscallArgs {
        SyscallArgs {
            nr: 202,
            arg0: uaddr,
            arg1: op,
            arg2: val,
            arg3: 0,
            arg4: 0,
            arg5: 0,
            caller_rip: 0,
            caller_rflags: 0,
        }
    }

    #[test]
    fn test_futex_private_flag() {
        // FUTEX_PRIVATE_FLAG (128) is stripped by FUTEX_CMD_MASK; the dispatch
        // must reach the real FUTEX_WAIT arm. Use a deliberately
        // mismatched `expected` so `wait()` takes its EAGAIN fast path
        // instead of actually blocking — there is no second thread to wake
        // it on a single OS test thread.
        let t: &'static crate::sched::thread::Thread =
            Box::leak(Box::new(crate::sched::thread::Thread::new_test(9001)));
        crate::sched::percpu::install_test_thread(0, t);
        let word: u32 = 7;
        let op = (FUTEX_WAIT | FUTEX_PRIVATE_FLAG) as u64;
        let args = make_args(&word as *const u32 as u64, op, 8);
        assert_eq!(
            sys_futex(&args),
            EAGAIN,
            "FUTEX_WAIT|FUTEX_PRIVATE_FLAG must dispatch to FUTEX_WAIT arm",
        );
    }

    #[test]
    fn test_futex_unknown_op() {
        // An unrecognized futex operation must return ENOSYS.
        let word: u32 = 0;
        let args = make_args(&word as *const u32 as u64, 99, 0);
        assert_eq!(
            sys_futex(&args),
            ENOSYS,
            "unknown futex op must return ENOSYS",
        );
    }

    #[test]
    fn set_robust_list_rejects_wrong_length() {
        let args = SyscallArgs {
            nr: 273,
            arg0: 0x1000,
            arg1: 32,
            arg2: 0,
            arg3: 0,
            arg4: 0,
            arg5: 0,
            caller_rip: 0,
            caller_rflags: 0,
        };
        assert_eq!(sys_set_robust_list(&args), EINVAL);
    }

    #[test]
    fn set_robust_list_accepts_size_24() {
        let t: &'static crate::sched::thread::Thread =
            Box::leak(Box::new(crate::sched::thread::Thread::new_test(9002)));
        crate::sched::percpu::install_test_thread(0, t);
        let args = SyscallArgs {
            nr: 273,
            arg0: 0x1000,
            arg1: 24,
            arg2: 0,
            arg3: 0,
            arg4: 0,
            arg5: 0,
            caller_rip: 0,
            caller_rflags: 0,
        };
        assert_eq!(sys_set_robust_list(&args), 0);
        assert_eq!(
            t.robust_list_head
                .load(core::sync::atomic::Ordering::Relaxed),
            0x1000,
        );
    }
}
