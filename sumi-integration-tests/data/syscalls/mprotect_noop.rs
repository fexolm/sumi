#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    // sumi runs everything in ring 0 with RWX 2MB pages — mprotect is a no-op
    // that always returns 0.
    let len = 2u64 * 1024 * 1024;
    let addr = sys_mmap(
        0,
        len,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0,
    );
    check!(addr > 0);

    let r = sys_mprotect(addr as u64, len, PROT_READ);
    check_eq!(r, 0);

    // After mprotect we still must be able to read; in sumi we can also write.
    unsafe {
        *(addr as *mut u8) = 0x42;
        check_eq!(*(addr as *const u8), 0x42);
    }

    let r = sys_munmap(addr as u64, len);
    check_eq!(r, 0);
    pass!();
}
