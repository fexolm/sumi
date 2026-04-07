#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

const ENOENT: i64 = -2;

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let path = b"/tmp/sumi_int_access.txt\0";
    let fd = sys_open(path, O_RDWR | O_CREAT | O_TRUNC, 0o644);
    check!(fd >= 0);
    check_eq!(sys_close(fd), 0);

    // F_OK == 0 — file exists.
    check_eq!(sys_access(path, 0), 0);

    // Nonexistent → ENOENT
    check_eq!(sys_access(b"/no/such/file/sumi\0", 0), ENOENT);
    pass!();
}
