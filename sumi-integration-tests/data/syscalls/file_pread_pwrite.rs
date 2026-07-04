#![no_std]
#![no_main]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let path = b"/tmp/sumi_int_prw.txt\0";
    let fd = sys_open(path, O_RDWR | O_CREAT | O_TRUNC, 0o644);
    check!(fd >= 0);

    // pwrite at offset 0 and offset 16
    check_eq!(sys_pwrite64(fd, b"AAAAA", 0), 5);
    check_eq!(sys_pwrite64(fd, b"ZZZZZ", 16), 5);

    // pread back at the same offsets
    let mut buf = [0u8; 5];
    check_eq!(sys_pread64(fd, &mut buf, 0), 5);
    check_eq!(buf[0] as i64, b'A' as i64);
    check_eq!(buf[4] as i64, b'A' as i64);

    let mut buf = [0u8; 5];
    check_eq!(sys_pread64(fd, &mut buf, 16), 5);
    check_eq!(buf[0] as i64, b'Z' as i64);
    check_eq!(buf[4] as i64, b'Z' as i64);

    // pread/pwrite must NOT advance the file's regular offset.
    check_eq!(sys_lseek(fd, 0, SEEK_CUR), 0);

    check_eq!(sys_close(fd), 0);
    pass!();
}
