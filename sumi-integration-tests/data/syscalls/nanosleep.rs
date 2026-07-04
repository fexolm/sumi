#![no_std]
#![no_main]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    // 5 ms sleep — should advance CLOCK_MONOTONIC by ≥ 5 ms.
    let mut before = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    check_eq!(sys_clock_gettime(CLOCK_MONOTONIC, &mut before), 0);

    let req = Timespec {
        tv_sec: 0,
        tv_nsec: 5_000_000,
    };
    check_eq!(sys_nanosleep(&req, core::ptr::null_mut()), 0);

    let mut after = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    check_eq!(sys_clock_gettime(CLOCK_MONOTONIC, &mut after), 0);

    let delta_ns =
        (after.tv_sec - before.tv_sec) * 1_000_000_000 + (after.tv_nsec - before.tv_nsec);
    check!(delta_ns >= 5_000_000);
    pass!();
}
