#![no_std]
#![no_main]
include!("../common.rs");

use core::sync::atomic::{AtomicI64, Ordering};

#[repr(C, align(16))]
struct ChildStack([u8; 16384]);
static mut CHILD_STACK: ChildStack = ChildStack([0; 16384]);

static CHILD_ABOUT_TO_EXIT: AtomicI64 = AtomicI64::new(0);

fn stack_top(s: *mut ChildStack) -> u64 {
    let base = s as *mut u8;
    // SAFETY: `s` points at a valid, live `ChildStack`; `add` stays within
    // (one past) that object, which is legal for pointer arithmetic.
    let top = unsafe { base.add(core::mem::size_of::<ChildStack>()) };
    ((top as usize) & !0xF) as u64
}

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let tid = sys_clone(
        CLONE_REQUIRED,
        stack_top(core::ptr::addr_of_mut!(CHILD_STACK)),
        core::ptr::null_mut(),
        core::ptr::null_mut(),
        0,
    );
    if tid == 0 {
        // ------------- CHILD -------------
        // A single-thread `exit` (syscall 60) must terminate only this
        // thread, not the whole VM — if it did, the parent below would
        // never get to run and the harness would time out instead of
        // seeing a clean `[exit] code=0`.
        CHILD_ABOUT_TO_EXIT.store(1, Ordering::Release);
        exit_thread(0);
    }

    // ------------- PARENT -------------
    check!(tid > 0, b"clone failed");
    let mut spins: i64 = 0;
    let max_spins: i64 = 200_000;
    while CHILD_ABOUT_TO_EXIT.load(Ordering::Acquire) == 0 {
        sys_sched_yield();
        spins += 1;
        check!(spins < max_spins, b"exit_one_thread: child never ran");
    }

    // The parent must still be alive and schedulable well after the
    // child's per-thread exit — prove it by yielding a few more rounds
    // before exiting the whole VM successfully.
    for _ in 0..100 {
        sys_sched_yield();
    }
    check_eq!(sys_gettid(), 1);

    pass!();
}
