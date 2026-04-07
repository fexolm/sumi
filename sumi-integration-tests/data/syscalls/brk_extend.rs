#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    // Query the current break by passing 0.
    let cur = sys_brk(0);
    check!(cur > 0);

    // Extend by 1 MB.
    let new = (cur as u64) + (1 << 20);
    let got = sys_brk(new);
    check_eq!(got as u64, new);

    // The freshly mapped pages must be readable & writable.
    let p = (cur as u64) as *mut u64;
    unsafe {
        for i in 0..16 {
            *p.add(i) = (i as u64) * 0x1111_1111;
        }
        for i in 0..16 {
            check_eq!(*p.add(i), (i as u64) * 0x1111_1111);
        }
    }

    // Shrink back.
    let shrunk = sys_brk(cur as u64);
    check_eq!(shrunk, cur);
    pass!();
}
