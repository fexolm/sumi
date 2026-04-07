#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let path = b"/tmp/sumi_int_openat.txt\0";
    let fd = sys_openat(AT_FDCWD, path, O_RDWR | O_CREAT | O_TRUNC, 0o644);
    check!(fd >= 0);
    check_eq!(sys_write(fd, b"openat-cwd"), 10);
    check_eq!(sys_close(fd), 0);

    // Re-open via openat(AT_FDCWD, …) and read back.
    let fd2 = sys_openat(AT_FDCWD, path, O_RDONLY, 0);
    check!(fd2 >= 0);
    let mut buf = [0u8; 10];
    check_eq!(sys_read(fd2, &mut buf), 10);
    check_eq!(buf[0] as i64, b'o' as i64);
    check_eq!(sys_close(fd2), 0);
    pass!();
}
