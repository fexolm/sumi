#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

const EINVAL: i64 = -22;

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let mut ts = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    check_eq!(sys_clock_getres(CLOCK_REALTIME, &mut ts), 0);
    // Resolution must be a positive number of nanoseconds, less than one second.
    check!(ts.tv_nsec >= 1 && ts.tv_nsec < 1_000_000_000);
    check_eq!(ts.tv_sec, 0);

    // Unknown clock_id is rejected.
    check_eq!(sys_clock_getres(99, &mut ts), EINVAL);
    pass!();
}
