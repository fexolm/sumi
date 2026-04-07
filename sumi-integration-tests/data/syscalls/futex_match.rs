#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

const EAGAIN: i64 = -11;
const ENOSYS: i64 = -38;
const FUTEX_PRIVATE_FLAG: u64 = 128;

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let word: u32 = 7;

    // FUTEX_WAIT with matching value: returns 0 immediately (no waiters
    // possible in single-threaded unikernel).
    check_eq!(sys_futex(&word, FUTEX_WAIT, 7), 0);

    // PRIVATE_FLAG is stripped — same behavior.
    check_eq!(sys_futex(&word, FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 7), 0);

    // FUTEX_WAIT with mismatched value → EAGAIN.
    check_eq!(sys_futex(&word, FUTEX_WAIT, 99), EAGAIN);

    // FUTEX_WAKE → returns 0 (no waiters).
    check_eq!(sys_futex(&word, FUTEX_WAKE, 1), 0);

    // Unknown op → ENOSYS.
    check_eq!(sys_futex(&word, 99, 0), ENOSYS);
    pass!();
}
