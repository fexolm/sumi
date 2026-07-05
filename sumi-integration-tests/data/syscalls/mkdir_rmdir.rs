#![no_std]
#![no_main]

include!("../common.rs");

const ENOENT: i64 = -2;
const AT_REMOVEDIR: u64 = 0x200;

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let dir = b"/tmp/sumi_int_mkdir_dir\0";
    let nested = b"/tmp/sumi_int_mkdir_dir/child\0";

    let _ = sys_rmdir(nested);
    let _ = sys_rmdir(dir);

    check_eq!(sys_mkdir(dir, 0o755), 0);
    check_eq!(sys_access(dir, 0), 0);
    check_eq!(sys_mkdir(nested, 0o700), 0);
    check_eq!(sys_access(nested, 0), 0);
    check_eq!(sys_rmdir(nested), 0);
    check_eq!(sys_access(nested, 0), ENOENT);
    check_eq!(sys_rmdir(dir), 0);
    check_eq!(sys_access(dir, 0), ENOENT);

    check_eq!(sys_mkdirat(AT_FDCWD, dir, 0o755), 0);
    check_eq!(sys_unlinkat(AT_FDCWD, dir, AT_REMOVEDIR), 0);
    check_eq!(sys_access(dir, 0), ENOENT);

    check_eq!(sys_chdir(b"/tmp\0"), 0);
    let rel = b"./sumi_int_mkdir_relative\0";
    check_eq!(sys_mkdir(rel, 0o755), 0);
    check_eq!(sys_access(rel, 0), 0);
    check_eq!(sys_rmdir(rel), 0);

    pass!();
}
