#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let path = b"/tmp/sumi_int_fstat.txt\0";
    let fd = sys_open(path, O_RDWR | O_CREAT | O_TRUNC, 0o644);
    check!(fd >= 0);
    let payload = [b'X'; 123];
    check_eq!(sys_write(fd, &payload), 123);

    let mut st: Stat = unsafe { core::mem::zeroed() };
    let r = sys_fstat(fd, &mut st as *mut _ as *mut u8);
    check_eq!(r, 0);
    check_eq!(st.st_size, 123);
    check!(st.st_mode != 0);

    check_eq!(sys_close(fd), 0);
    pass!();
}
