#![no_std]
#![no_main]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let mut tid: i32 = 0;
    let r = sys_set_tid_address(&mut tid);
    // Returns the (single) tid in the unikernel, must be > 0.
    check!(r > 0);
    pass!();
}
