#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let mut buf = [0u8; 32];
    // sumi runs the kernel with stdin connected to /dev/null, so reads
    // from stdin must return 0 (EOF) immediately.
    let n = sys_read(0, &mut buf);
    check_eq!(n, 0);
    pass!();
}
