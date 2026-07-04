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
    // Fast path: nothing else is runnable.
    if cpu.runqueue.load() == 0 {
        return 0;
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
/// Scheduler-integrated (see `sched::futex`) — real semantics under
/// `cargo test` too (F14): `FUTEX_WAIT` on a mismatched value returns
/// `EAGAIN` immediately, same as production; actually blocking still needs
/// a second thread to wake it, so most host tests exercise the bookkeeping
/// via `sched::futex` directly rather than through this dispatch.
pub fn sys_futex(args: &SyscallArgs) -> SyscallResult {
    let uaddr = args.arg0 as *const u32;
    let op    = args.arg1 as i32;
    let val   = args.arg2 as u32;
    let cmd   = op & FUTEX_CMD_MASK;

    if uaddr.is_null() {
        return EFAULT;
    }

    match cmd {
        FUTEX_WAIT => crate::sched::futex::wait(uaddr, val),
        FUTEX_WAKE => crate::sched::futex::wake(uaddr, val),
        FUTEX_WAIT_BITSET => {
            // arg3 (r10) = timeout pointer (ignored in sumi — no timed waits)
            // arg4 (r8)  = unused
            // arg5 (r9)  = bitset
            let bitset = args.arg5 as u32;
            crate::sched::futex::wait_bitset(uaddr, val, bitset)
        }
        FUTEX_WAKE_BITSET => {
            // arg5 (r9) = bitset
            let bitset = args.arg5 as u32;
            crate::sched::futex::wake_bitset(uaddr, val, bitset)
        }
        _ => ENOSYS,
    }
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
        // must reach the real FUTEX_WAIT arm (F14). Use a deliberately
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
            nr: 273, arg0: 0x1000, arg1: 32,
            arg2: 0, arg3: 0, arg4: 0, arg5: 0,
            caller_rip: 0, caller_rflags: 0,
        };
        assert_eq!(sys_set_robust_list(&args), EINVAL);
    }

    #[test]
    fn set_robust_list_accepts_size_24() {
        let t: &'static crate::sched::thread::Thread =
            Box::leak(Box::new(crate::sched::thread::Thread::new_test(9002)));
        crate::sched::percpu::install_test_thread(0, t);
        let args = SyscallArgs {
            nr: 273, arg0: 0x1000, arg1: 24,
            arg2: 0, arg3: 0, arg4: 0, arg5: 0,
            caller_rip: 0, caller_rflags: 0,
        };
        assert_eq!(sys_set_robust_list(&args), 0);
        assert_eq!(
            t.robust_list_head.load(core::sync::atomic::Ordering::Relaxed),
            0x1000,
        );
    }
}
