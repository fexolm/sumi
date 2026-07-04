#![no_std]
#![no_main]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let path = b"/tmp/sumi_int_newfstatat.txt\0";
    let fd = sys_open(path, O_RDWR | O_CREAT | O_TRUNC, 0o644);
    check!(fd >= 0);
    check_eq!(sys_write(fd, b"newfstatat-payload"), 18);
    check_eq!(sys_close(fd), 0);

    // Stat-by-path with AT_FDCWD.
    let mut st: Stat = unsafe { core::mem::zeroed() };
    let r = sys_newfstatat(AT_FDCWD, path, &mut st as *mut _ as *mut u8, 0);
    check_eq!(r, 0);
    check_eq!(st.st_size, 18);
    pass!();
}
