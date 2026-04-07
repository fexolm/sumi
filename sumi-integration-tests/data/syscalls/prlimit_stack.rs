#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

const ESRCH: i64 = -3;
const EINVAL: i64 = -22;

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let mut rl = Rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };

    // pid==0 means "current process".
    let r = sys_prlimit64(
        0,
        RLIMIT_STACK,
        core::ptr::null(),
        &mut rl as *mut _ as *mut u8,
    );
    check_eq!(r, 0);
    check!(rl.rlim_cur > 0);
    check!(rl.rlim_max >= rl.rlim_cur);

    // pid==1 (sumi's only process) is also accepted.
    let r = sys_prlimit64(
        1,
        RLIMIT_STACK,
        core::ptr::null(),
        &mut rl as *mut _ as *mut u8,
    );
    check_eq!(r, 0);

    // Other pids → ESRCH.
    let r = sys_prlimit64(
        7,
        RLIMIT_STACK,
        core::ptr::null(),
        &mut rl as *mut _ as *mut u8,
    );
    check_eq!(r, ESRCH);

    // Unknown resource → EINVAL.
    let r = sys_prlimit64(0, 9999, core::ptr::null(), core::ptr::null_mut());
    check_eq!(r, EINVAL);
    pass!();
}
