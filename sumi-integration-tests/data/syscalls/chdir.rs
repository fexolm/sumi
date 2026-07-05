#![no_std]
#![no_main]

include!("../common.rs");

const ENOENT: i64 = -2;

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    // Existing path succeeds.
    check_eq!(sys_chdir(b"/tmp\0"), 0);
    let mut cwd = [0u8; 64];
    check!(sys_getcwd(&mut cwd) > 0);
    check_eq!(cwd[0] as i64, b'/' as i64);
    check_eq!(cwd[1] as i64, b't' as i64);
    check_eq!(cwd[2] as i64, b'm' as i64);
    check_eq!(cwd[3] as i64, b'p' as i64);
    check_eq!(cwd[4] as i64, 0);

    // Dot components are normalized before virtio-fs lookup.
    check_eq!(sys_chdir(b"/tmp/../tmp/.\0"), 0);

    // Nonexistent path fails with ENOENT.
    check_eq!(sys_chdir(b"/no/such/path/foo\0"), ENOENT);
    pass!();
}
