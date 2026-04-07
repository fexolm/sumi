#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let path = b"/tmp/sumi_int_lseek.txt\0";
    let fd = sys_open(path, O_RDWR | O_CREAT | O_TRUNC, 0o644);
    check!(fd >= 0);

    let n = sys_write(fd, b"abcdefghij");
    check_eq!(n, 10);

    // SEEK_SET to start
    check_eq!(sys_lseek(fd, 0, SEEK_SET), 0);
    let mut buf = [0u8; 4];
    check_eq!(sys_read(fd, &mut buf), 4);
    check_eq!(buf[0] as i64, b'a' as i64);
    check_eq!(buf[3] as i64, b'd' as i64);

    // SEEK_CUR forward by 2 (now at 6 → 'g')
    check_eq!(sys_lseek(fd, 2, SEEK_CUR), 6);
    let mut buf = [0u8; 1];
    check_eq!(sys_read(fd, &mut buf), 1);
    check_eq!(buf[0] as i64, b'g' as i64);

    // SEEK_END returns the file length
    check_eq!(sys_lseek(fd, 0, SEEK_END), 10);
    check_eq!(sys_close(fd), 0);
    pass!();
}
