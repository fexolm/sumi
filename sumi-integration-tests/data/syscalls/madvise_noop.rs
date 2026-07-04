#![no_std]
#![no_main]

include!("../common.rs");

const SYS_MADVISE: u64 = 28;
const MADV_DONTNEED: u64 = 4;

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
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

    // madvise is advisory; sumi treats every hint as a no-op returning 0.
    let r = unsafe { syscall3(SYS_MADVISE, addr as u64, len, MADV_DONTNEED) };
    check_eq!(r, 0);

    check_eq!(sys_munmap(addr as u64, len), 0);
    pass!();
}
