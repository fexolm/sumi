#![no_std]
#![no_main]

include!("../common.rs");

const EAGAIN: i64 = -11;

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let word: u32 = 42;

    // The comparison value doesn't match the current contents of `word`,
    // so FUTEX_WAIT must return EAGAIN immediately instead of blocking
    // (a mismatched wait would otherwise hang the test forever, since
    // nothing ever wakes this address).
    let r = sys_futex(&word, FUTEX_WAIT, 99);
    check_eq!(r, EAGAIN);

    pass!();
}
