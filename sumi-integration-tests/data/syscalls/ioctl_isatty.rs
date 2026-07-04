#![no_std]
#![no_main]

include!("../common.rs");

const ENOTTY: i64 = -25;
const TCGETS: u64 = 0x5401;

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    // Issuing TCGETS on stdout must return ENOTTY so glibc's isatty() reports
    // false. The exact request code is irrelevant — sumi returns ENOTTY for
    // every ioctl.
    let mut buf = [0u8; 64];
    let r = sys_ioctl(1, TCGETS, buf.as_mut_ptr() as u64);
    check_eq!(r, ENOTTY);
    pass!();
}
