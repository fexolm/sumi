#![no_std]
#![no_main]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let msg = b"hello, sumi!\n";
    let n = sys_write(1, msg);
    check_eq!(n, msg.len() as i64);
    pass!();
}
