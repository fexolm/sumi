#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    // unikernel runs as root: all id syscalls return 0.
    check_eq!(sys_getuid(), 0);
    check_eq!(sys_getgid(), 0);
    check_eq!(sys_geteuid(), 0);
    check_eq!(sys_getegid(), 0);
    pass!();
}
