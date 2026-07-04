#![no_std]
#![no_main]

include!("../common.rs");

const ENOENT: i64 = -2;

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    // Existing path succeeds.
    check_eq!(sys_chdir(b"/tmp\0"), 0);

    // Nonexistent path fails with ENOENT.
    check_eq!(sys_chdir(b"/no/such/path/foo\0"), ENOENT);
    pass!();
}
