#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let path = b"/tmp/sumi_int_dup2.txt\0";
    let fd = sys_open(path, O_RDWR | O_CREAT | O_TRUNC, 0o644);
    check!(fd >= 0);
    check_eq!(sys_write(fd, b"dup2-data"), 9);

    // dup2 to a fixed fd 100.
    check_eq!(sys_dup2(fd, 100), 100);

    // Read via the new fd.
    check_eq!(sys_lseek(100, 0, SEEK_SET), 0);
    let mut buf = [0u8; 9];
    check_eq!(sys_read(100, &mut buf), 9);
    check_eq!(buf[0] as i64, b'd' as i64);
    check_eq!(buf[8] as i64, b'a' as i64);

    // dup2 to itself is a no-op and returns new_fd.
    check_eq!(sys_dup2(100, 100), 100);

    check_eq!(sys_close(fd), 0);
    check_eq!(sys_close(100), 0);
    pass!();
}
