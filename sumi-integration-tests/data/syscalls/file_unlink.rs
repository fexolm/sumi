#![no_std]
#![no_main]

include!("../common.rs");

const ENOENT: i64 = -2;

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let path = b"/tmp/sumi_int_unlink.txt\0";
    let fd = sys_open(path, O_RDWR | O_CREAT | O_TRUNC, 0o644);
    check!(fd >= 0);
    check_eq!(sys_close(fd), 0);
    check_eq!(sys_access(path, 0), 0);

    check_eq!(sys_unlink(path), 0);
    check_eq!(sys_access(path, 0), ENOENT);
    check_eq!(sys_unlink(path), ENOENT);

    let fd2 = sys_open(path, O_RDWR | O_CREAT | O_TRUNC, 0o644);
    check!(fd2 >= 0);
    check_eq!(sys_close(fd2), 0);
    check_eq!(sys_unlinkat(AT_FDCWD, path, 0), 0);
    check_eq!(sys_access(path, 0), ENOENT);

    check_eq!(sys_chdir(b"/tmp\0"), 0);
    let rel = b"./sumi_int_unlink_relative.txt\0";
    let fd3 = sys_open(rel, O_RDWR | O_CREAT | O_TRUNC, 0o644);
    check!(fd3 >= 0);
    check_eq!(sys_close(fd3), 0);
    check_eq!(sys_unlink(rel), 0);
    check_eq!(sys_access(rel, 0), ENOENT);

    pass!();
}
