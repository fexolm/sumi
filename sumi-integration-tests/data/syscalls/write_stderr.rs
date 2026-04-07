#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    // stdout: confirms test reached
    print(b"writing to fd 2\n");
    // stderr write must succeed; same console backing.
    let n = sys_write(2, b"err msg\n");
    check!(n > 0);
    pass!();
}
