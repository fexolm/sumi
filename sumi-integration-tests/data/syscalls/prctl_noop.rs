#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

const PR_SET_VMA: u64 = 0x53564D41; // glibc 2.34+ labels anon VMAs
const PR_GET_NAME: u64 = 16;
const EINVAL: i64 = -22;

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    // PR_SET_VMA is the one op sumi accepts as a successful no-op so glibc
    // startup stays quiet.
    check_eq!(sys_prctl(PR_SET_VMA, 0, 0, 0, 0), 0);

    // Every other op must fail. PR_GET_NAME is a getter — silently returning
    // 0 here would leave the caller's output buffer uninitialised and they
    // would then read garbage.
    check_eq!(sys_prctl(PR_GET_NAME, 0, 0, 0, 0), EINVAL);
    check_eq!(sys_prctl(9999, 0, 0, 0, 0), EINVAL);
    pass!();
}
