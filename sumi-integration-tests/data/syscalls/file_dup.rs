#![no_std]
#![no_main]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let path = b"/tmp/sumi_int_dup.txt\0";
    let fd = sys_open(path, O_RDWR | O_CREAT | O_TRUNC, 0o644);
    check!(fd >= 0);
    check_eq!(sys_write(fd, b"dup-data"), 8);

    // dup() returns a new fd referring to the same file
    let fd2 = sys_dup(fd);
    check!(fd2 >= 0);
    check!(fd2 != fd);

    // Both fds share the same offset.
    check_eq!(sys_lseek(fd2, 0, SEEK_SET), 0);
    let mut buf = [0u8; 8];
    check_eq!(sys_read(fd2, &mut buf), 8);
    check_eq!(buf[0] as i64, b'd' as i64);
    check_eq!(buf[7] as i64, b'a' as i64);

    check_eq!(sys_close(fd), 0);
    check_eq!(sys_close(fd2), 0);
    pass!();
}
