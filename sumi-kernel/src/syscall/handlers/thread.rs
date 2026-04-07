use crate::syscall::errno::*;
use crate::syscall::{SyscallArgs, SyscallResult};

const FUTEX_WAIT: i32 = 0;
const FUTEX_WAKE: i32 = 1;
const FUTEX_PRIVATE_FLAG: i32 = 128;
const FUTEX_CLOCK_REALTIME: i32 = 256;
const FUTEX_CMD_MASK: i32 = !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);

/// Single-threaded futex. WAIT returns 0 if *uaddr==val (no blocking),
/// EAGAIN if mismatch. WAKE returns 0 (no waiters).
pub fn sys_futex(args: &SyscallArgs) -> SyscallResult {
    let uaddr = args.arg0 as *const u32;
    let op = args.arg1 as i32;
    let val = args.arg2 as u32;
    let cmd = op & FUTEX_CMD_MASK;
    match cmd {
        FUTEX_WAIT => {
            if uaddr.is_null() {
                return EFAULT;
            }
            // SAFETY: User passed a valid aligned u32 pointer.
            let current = unsafe { core::ptr::read_volatile(uaddr) };
            if current == val { 0 } else { EAGAIN }
        }
        FUTEX_WAKE => 0,
        _ => ENOSYS,
    }
}

// Single-threaded unikernel: pretend the robust list is registered. We never
// walk it because we never exit a thread other than the main one. Returning 0
// (instead of ENOSYS via the dispatch fall-through) silences glibc startup spam.
pub fn sys_set_robust_list(_args: &SyscallArgs) -> SyscallResult {
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
        }
    }

    #[test]
    fn test_futex_wait_match() {
        // FUTEX_WAIT with val matching *uaddr must return 0 (no blocking needed).
        let word: u32 = 42;
        let args = make_args(&word as *const u32 as u64, FUTEX_WAIT as u64, 42);
        assert_eq!(
            sys_futex(&args),
            0,
            "FUTEX_WAIT with matching value must return 0"
        );
    }

    #[test]
    fn test_futex_wait_mismatch() {
        // FUTEX_WAIT with val not matching *uaddr must return EAGAIN.
        let word: u32 = 42;
        let args = make_args(&word as *const u32 as u64, FUTEX_WAIT as u64, 99);
        assert_eq!(
            sys_futex(&args),
            EAGAIN,
            "FUTEX_WAIT with mismatched value must return EAGAIN"
        );
    }

    #[test]
    fn test_futex_wake() {
        // FUTEX_WAKE has no waiters in a single-threaded kernel; must return 0.
        let word: u32 = 0;
        let args = make_args(&word as *const u32 as u64, FUTEX_WAKE as u64, 1);
        assert_eq!(sys_futex(&args), 0, "FUTEX_WAKE must return 0 (no waiters)");
    }

    #[test]
    fn test_futex_private_flag() {
        // FUTEX_PRIVATE_FLAG (128) is stripped by FUTEX_CMD_MASK; the underlying
        // command is still FUTEX_WAIT and must behave identically.
        let word: u32 = 7;
        let op = (FUTEX_WAIT | FUTEX_PRIVATE_FLAG) as u64;
        let args = make_args(&word as *const u32 as u64, op, 7);
        assert_eq!(
            sys_futex(&args),
            0,
            "FUTEX_WAIT|FUTEX_PRIVATE_FLAG with matching value must return 0"
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
            "unknown futex op must return ENOSYS"
        );
    }
}
