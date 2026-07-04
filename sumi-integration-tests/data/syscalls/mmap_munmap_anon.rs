#![no_std]
#![no_main]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    // Allocate, write a sentinel, free, allocate again — verify reuse.
    let len = 2u64 * 1024 * 1024;
    let a = sys_mmap(
        0,
        len,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0,
    );
    check!(a > 0);
    unsafe {
        *(a as *mut u64) = 0xCAFE_BABE_DEAD_BEEF;
    }
    check_eq!(sys_munmap(a as u64, len), 0);

    // Second alloc must succeed and start zeroed.
    let b = sys_mmap(
        0,
        len,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0,
    );
    check!(b > 0);
    unsafe {
        check_eq!(*(b as *const u64), 0);
    }
    check_eq!(sys_munmap(b as u64, len), 0);
    pass!();
}
