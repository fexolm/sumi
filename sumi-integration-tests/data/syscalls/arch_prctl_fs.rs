#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    // sumi runs everything in ring 0, so the kernel and user code share %fs.
    // Setting FS to a wild address would break any subsequent FS-relative
    // access (e.g. stack-canary loads through fs:0x28). Use a real, mapped
    // stack buffer big enough that fs:0x28 stays in-bounds.
    let tls_area: [u64; 32] = [0; 32];
    let target = tls_area.as_ptr() as u64;

    let r = sys_arch_prctl(ARCH_SET_FS, target);
    check_eq!(r, 0);

    let mut got: u64 = 0;
    let r = sys_arch_prctl(ARCH_GET_FS, &mut got as *mut u64 as u64);
    check_eq!(r, 0);
    check_eq!(got, target);
    pass!();
}
