#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

const EBADF: i64 = -9;

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    check_eq!(sys_close(999), EBADF);
    check_eq!(sys_read(999, &mut [0u8; 4]), EBADF);
    check_eq!(sys_write(999, b"x"), EBADF);
    pass!();
}
