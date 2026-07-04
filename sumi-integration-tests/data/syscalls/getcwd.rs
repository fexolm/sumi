#![no_std]
#![no_main]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let mut buf = [0u8; 64];
    let r = sys_getcwd(&mut buf);
    check!(r > 0);

    // The unikernel always reports cwd = "/".
    check_eq!(buf[0] as i64, b'/' as i64);
    check_eq!(buf[1] as i64, 0);
    pass!();
}
