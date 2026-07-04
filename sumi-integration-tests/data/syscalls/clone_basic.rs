#![no_std]
#![no_main]
include!("../common.rs");

// The child runs on this stack. 16 KB is plenty for a tight loop that
// only calls sys_sched_yield. Stack grows downward; child_stack passed to
// clone() is the high end (exclusive, like pthread_attr_setstack).
#[repr(C, align(16))]
struct ChildStack([u8; 16384]);

static mut CHILD_STACK: ChildStack = ChildStack([0; 16384]);

// CLONE_VM makes parent and child share this memory directly — child
// writes SENTINEL, parent observes via a volatile read.
static mut SHARED: u64 = 0;

const SENTINEL: u64 = 0xCAFE_BABE_DEAD_BEEF;

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    // Compute a 16-aligned top-of-stack pointer for the child.
    let stack_ptr = unsafe {
        let base = core::ptr::addr_of_mut!(CHILD_STACK) as *mut u8;
        let top  = base.add(core::mem::size_of::<ChildStack>());
        // align-down to 16
        ((top as usize) & !0xF) as *mut u8
    };

    let tid = sys_clone(
        CLONE_REQUIRED,
        stack_ptr as u64,
        core::ptr::null_mut(),
        core::ptr::null_mut(),
        0,
    );

    if tid == 0 {
        // ------------- CHILD -------------
        // Publish the sentinel, then spin yielding so the parent gets
        // scheduled and observes the write. This test has no per-thread
        // exit yet; an infinite yield loop is the documented placeholder.
        unsafe {
            core::ptr::write_volatile(core::ptr::addr_of_mut!(SHARED), SENTINEL);
        }
        loop {
            sys_sched_yield();
        }
    } else {
        // ------------- PARENT -------------
        check!(tid > 0, b"sys_clone returned an error");
        // Parent's own TID must still be 1 (BSP main).
        check_eq!(sys_gettid(), 1);
        // Child's TID (returned by clone) must be different from parent's.
        check!(tid != 1, b"child TID collides with main");

        // Poll SHARED under a bounded spin budget. 200k sched_yields is
        // comfortably more than the ~10 needed for the child to publish
        // the sentinel; if we exceed it, something is wrong with the
        // scheduler or trampoline.
        let mut spins: i64 = 0;
        let max_spins: i64 = 200_000;
        loop {
            let v = unsafe {
                core::ptr::read_volatile(core::ptr::addr_of!(SHARED))
            };
            if v == SENTINEL {
                break;
            }
            sys_sched_yield();
            spins += 1;
            check!(spins < max_spins, b"clone_basic: child never published sentinel");
        }
        pass!();
    }
}
