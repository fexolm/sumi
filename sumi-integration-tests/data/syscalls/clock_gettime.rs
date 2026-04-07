#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let mut ts = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };

    // CLOCK_REALTIME
    check_eq!(sys_clock_gettime(CLOCK_REALTIME, &mut ts), 0);
    check!(ts.tv_sec > 0);
    check!(ts.tv_nsec >= 0 && ts.tv_nsec < 1_000_000_000);

    // CLOCK_MONOTONIC: must advance.
    let mut a = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    check_eq!(sys_clock_gettime(CLOCK_MONOTONIC, &mut a), 0);

    // Spin a little to give the TSC time to advance.
    for _ in 0..200_000 {
        unsafe { core::arch::asm!("pause", options(nomem, nostack)) };
    }

    let mut b = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    check_eq!(sys_clock_gettime(CLOCK_MONOTONIC, &mut b), 0);

    let delta = (b.tv_sec - a.tv_sec) * 1_000_000_000 + (b.tv_nsec - a.tv_nsec);
    check!(delta > 0);
    pass!();
}
