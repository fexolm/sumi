#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    // Phase 2 oracle: the kernel must emit "[exit] code=7" via the
    // hypercall::shutdown path, which the harness's
    // run_test_expect_exit asserts on.
    unsafe {
        let _ = syscall1(SYS_EXIT_GROUP, 7);
    }
    loop {}
}
