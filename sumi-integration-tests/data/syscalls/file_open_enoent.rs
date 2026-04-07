#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

const ENOENT: i64 = -2;

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let r = sys_open(b"/nonexistent/sumi/path/foo.bin\0", O_RDONLY, 0);
    check_eq!(r, ENOENT);
    pass!();
}
