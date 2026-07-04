#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

// Phase 2/10 test: exit_group() must tear down the whole VM even while
// other threads (including the one calling it) are still alive and
// runnable. Main spawns 4 children; every child spins forever except one,
// which calls exit_group(7) after a short delay. If exit_group only
// killed the calling thread, the other 3 children (and main) would spin
// forever and the harness would time out instead of observing
// `[exit] code=7`.

#[repr(C, align(16))]
struct ChildStack([u8; 16384]);
static mut STACKS: [ChildStack; 4] = [
    ChildStack([0; 16384]),
    ChildStack([0; 16384]),
    ChildStack([0; 16384]),
    ChildStack([0; 16384]),
];

fn stack_top(s: *mut ChildStack) -> u64 {
    let base = s as *mut u8;
    // SAFETY: `s` points at a valid, live `ChildStack`; `add` stays within
    // (one past) that object, which is legal for pointer arithmetic.
    let top = unsafe { base.add(core::mem::size_of::<ChildStack>()) };
    ((top as usize) & !0xF) as u64
}

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    for i in 0..4 {
        let stack = unsafe { core::ptr::addr_of_mut!(STACKS[i]) };
        let tid = sys_clone(
            CLONE_REQUIRED,
            stack_top(stack),
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            0,
        );
        if tid == 0 {
            // ------------- CHILD -------------
            if i == 1 {
                // Give the other children (and main) a real chance to
                // start spinning before the VM goes down.
                for _ in 0..20 {
                    sys_sched_yield();
                }
                unsafe {
                    let _ = syscall1(SYS_EXIT_GROUP, 7);
                }
                loop {}
            }
            loop {
                sys_sched_yield();
            }
        }
        check!(tid > 0, b"clone failed");
    }

    // ------------- PARENT -------------
    // Spins forever; only exit_group's whole-VM teardown can stop this.
    loop {
        sys_sched_yield();
    }
}
