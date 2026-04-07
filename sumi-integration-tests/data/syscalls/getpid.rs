#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    // sumi is a single-process unikernel: pid = tid = 1, ppid = 0.
    check_eq!(sys_getpid(), 1);
    check_eq!(sys_gettid(), 1);
    check_eq!(sys_getppid(), 0);
    pass!();
}
