#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

const EINVAL: i64 = -22;

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    // mmap with len=0 must fail with EINVAL.
    let r = sys_mmap(
        0,
        0,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0,
    );
    check_eq!(r, EINVAL);
    pass!();
}
