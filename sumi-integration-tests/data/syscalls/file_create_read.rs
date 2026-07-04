#![no_std]
#![no_main]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    // Create a file via O_CREAT, write to it, close it, then re-open and read back.
    let path = b"/tmp/sumi_int_create.txt\0";
    let data = b"create+read works\n";

    let fd = sys_open(path, O_RDWR | O_CREAT | O_TRUNC, 0o644);
    check!(fd >= 0);
    let n = sys_write(fd, data);
    check_eq!(n, data.len() as i64);
    check_eq!(sys_close(fd), 0);

    let fd2 = sys_open(path, O_RDONLY, 0);
    check!(fd2 >= 0);
    let mut buf = [0u8; 64];
    let n = sys_read(fd2, &mut buf);
    check_eq!(n, data.len() as i64);
    for i in 0..data.len() {
        check_eq!(buf[i] as i64, data[i] as i64);
    }
    check_eq!(sys_close(fd2), 0);
    pass!();
}
