#![no_std]
#![no_main]

include!("../common.rs");

const EINVAL: i64 = -22;

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    // Zero flags: none of the required pthread-style bits set.
    let r = sys_clone(0, 0x1000, core::ptr::null_mut(), core::ptr::null_mut(), 0);
    check_eq!(r, EINVAL);

    // CLONE_VM alone: missing CLONE_FS|CLONE_FILES|CLONE_SIGHAND|CLONE_THREAD.
    let r = sys_clone(CLONE_VM, 0x1000, core::ptr::null_mut(), core::ptr::null_mut(), 0);
    check_eq!(r, EINVAL);

    // Full required set but a null child stack.
    let r = sys_clone(CLONE_REQUIRED, 0, core::ptr::null_mut(), core::ptr::null_mut(), 0);
    check_eq!(r, EINVAL);

    pass!();
}
