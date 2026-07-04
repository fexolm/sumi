#![no_std]
#![no_main]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    // Anonymous mmap of 4 MB (two 2 MB pages).
    let len = 4u64 * 1024 * 1024;
    let addr = sys_mmap(
        0,
        len,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0,
    );
    check!(addr > 0);

    let p = addr as *mut u8;
    unsafe {
        // Pages must be zeroed (kernel zeros every freshly allocated page).
        for i in 0..len as usize {
            check_eq!(*p.add(i), 0);
            // Stop early so the test doesn't take forever; sample a few
            // distinct pages.
            if i == 8 || i == (1 << 20) || i == (3 << 20) {
                continue;
            }
            if i > 16 && i != (1 << 20) && i != (3 << 20) {
                break;
            }
        }
        // Spot-write across both 2 MB pages.
        *p.add(0) = 0xAA;
        *p.add((2 << 20) + 7) = 0x55;
        check_eq!(*p.add(0), 0xAA);
        check_eq!(*p.add((2 << 20) + 7), 0x55);
    }

    let r = sys_munmap(addr as u64, len);
    check_eq!(r, 0);
    pass!();
}
