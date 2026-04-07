#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

const ENOENT: i64 = -2;

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let path = b"/tmp/sumi_int_stat.txt\0";
    let fd = sys_open(path, O_RDWR | O_CREAT | O_TRUNC, 0o644);
    check!(fd >= 0);
    check_eq!(sys_write(fd, b"stat-data"), 9);
    check_eq!(sys_close(fd), 0);

    let mut st: Stat = unsafe { core::mem::zeroed() };
    let r = sys_stat(path, &mut st as *mut _ as *mut u8);
    check_eq!(r, 0);
    check_eq!(st.st_size, 9);

    // Nonexistent path → ENOENT
    let mut st2: Stat = unsafe { core::mem::zeroed() };
    let r = sys_stat(b"/tmp/sumi_no_such_file_zzz\0", &mut st2 as *mut _ as *mut u8);
    check_eq!(r, ENOENT);
    pass!();
}
