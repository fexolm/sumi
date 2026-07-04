#![no_std]
#![no_main]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    // Empty buffer returns 0.
    let mut empty: [u8; 0] = [];
    check_eq!(sys_getrandom(&mut empty, 0), 0);

    // Fill 64 bytes.
    let mut buf = [0u8; 64];
    let r = sys_getrandom(&mut buf, 0);
    check_eq!(r, 64);

    // The buffer must contain at least one non-zero byte (probability of all-zero
    // from a healthy RNG is effectively 0).
    let mut any = false;
    for &b in &buf {
        if b != 0 {
            any = true;
            break;
        }
    }
    check!(any);
    pass!();
}
