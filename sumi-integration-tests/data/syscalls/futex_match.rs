#![no_std]
#![no_main]

include!("../common.rs");

const EAGAIN: i64 = -11;
const ENOSYS: i64 = -38;
const FUTEX_PRIVATE_FLAG: u64 = 128;

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let word: u32 = 7;

    // FUTEX_WAIT with mismatched value → EAGAIN (value changed before we
    // could queue). This does NOT block.
    check_eq!(sys_futex(&word, FUTEX_WAIT, 99), EAGAIN);

    // PRIVATE_FLAG stripped, mismatch → EAGAIN.
    check_eq!(sys_futex(&word, FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 99), EAGAIN);

    // FUTEX_WAKE → returns 0 (no waiters on this address).
    check_eq!(sys_futex(&word, FUTEX_WAKE, 1), 0);

    // Unknown op → ENOSYS.
    check_eq!(sys_futex(&word, 99, 0), ENOSYS);
    pass!();
}
