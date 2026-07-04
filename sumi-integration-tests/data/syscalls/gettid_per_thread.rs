#![no_std]
#![no_main]
include!("../common.rs");

use core::sync::atomic::{AtomicI64, Ordering};

#[repr(C, align(16))]
struct ChildStack([u8; 16384]);
static mut STACK_A: ChildStack = ChildStack([0; 16384]);
static mut STACK_B: ChildStack = ChildStack([0; 16384]);

// Each worker reports its own sys_gettid() result here (0 = not reported yet).
static TID_A: AtomicI64 = AtomicI64::new(0);
static TID_B: AtomicI64 = AtomicI64::new(0);

fn stack_top(s: *mut ChildStack) -> u64 {
    let base = s as *mut u8;
    // SAFETY: `s` points at a valid, live `ChildStack`; `add` stays within
    // (one past) that object, which is legal for pointer arithmetic.
    let top = unsafe { base.add(core::mem::size_of::<ChildStack>()) };
    ((top as usize) & !0xF) as u64
}

fn spawn_reporter(stack: *mut ChildStack, out: &'static AtomicI64) -> i64 {
    let tid = sys_clone(
        CLONE_REQUIRED,
        stack_top(stack),
        core::ptr::null_mut(),
        core::ptr::null_mut(),
        0,
    );
    if tid == 0 {
        out.store(sys_gettid(), Ordering::Release);
        loop {
            sys_sched_yield();
        }
    }
    tid
}

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    // Main thread (BSP) is always TID 1.
    check_eq!(sys_gettid(), 1);

    let clone_tid_a = spawn_reporter(core::ptr::addr_of_mut!(STACK_A), &TID_A);
    check!(clone_tid_a > 0, b"clone A failed");
    let clone_tid_b = spawn_reporter(core::ptr::addr_of_mut!(STACK_B), &TID_B);
    check!(clone_tid_b > 0, b"clone B failed");
    check!(clone_tid_a != clone_tid_b, b"clone returned duplicate TIDs");

    let mut spins: i64 = 0;
    let max_spins: i64 = 200_000;
    loop {
        let a = TID_A.load(Ordering::Acquire);
        let b = TID_B.load(Ordering::Acquire);
        if a != 0 && b != 0 {
            // Each worker's self-observed sys_gettid() must match the TID
            // clone() returned to the parent, and neither may collide
            // with the main thread's TID 1.
            check_eq!(a, clone_tid_a);
            check_eq!(b, clone_tid_b);
            check!(a != 1, b"worker A observed TID 1");
            check!(b != 1, b"worker B observed TID 1");
            check!(a != b, b"workers observed the same TID");
            break;
        }
        sys_sched_yield();
        spins += 1;
        check!(spins < max_spins, b"gettid_per_thread: workers never reported");
    }

    pass!();
}
