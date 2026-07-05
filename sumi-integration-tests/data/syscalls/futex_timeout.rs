#![no_std]
#![no_main]

include!("../common.rs");

const EAGAIN: i64 = -11;
const EINVAL: i64 = -22;
const ETIMEDOUT: i64 = -110;
const FUTEX_BITSET_MATCH_ANY: u64 = 0xffff_ffff;

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let word: u32 = 7;
    let timeout = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };

    check_eq!(
        sys_futex6(&word, FUTEX_WAIT, 7, &timeout, 0, 0),
        ETIMEDOUT
    );
    check_eq!(sys_futex6(&word, FUTEX_WAIT, 8, &timeout, 0, 0), EAGAIN);
    check_eq!(
        sys_futex6(
            &word,
            FUTEX_WAIT_BITSET,
            7,
            &timeout,
            0,
            FUTEX_BITSET_MATCH_ANY,
        ),
        ETIMEDOUT
    );
    check_eq!(sys_futex6(&word, FUTEX_WAIT_BITSET, 7, &timeout, 0, 0), EINVAL);

    pass!();
}
