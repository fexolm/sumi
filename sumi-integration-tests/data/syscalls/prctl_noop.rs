#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

const PR_SET_VMA: u64 = 0x53564D41; // glibc 2.34+ labels anon VMAs

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    // sumi accepts all prctl ops as a successful no-op so glibc startup is silent.
    let r = sys_prctl(PR_SET_VMA, 0, 0, 0, 0);
    check_eq!(r, 0);
    pass!();
}
