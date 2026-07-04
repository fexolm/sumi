#![no_std]
#![no_main]
include!("../common.rs");

use core::sync::atomic::{AtomicI64, Ordering};

// Two worker stacks — one per cloned child. 16 KB each is plenty for a
// tight increment-and-yield loop.
#[repr(C, align(16))]
struct ChildStack([u8; 16384]);
static mut STACK_A: ChildStack = ChildStack([0; 16384]);
static mut STACK_B: ChildStack = ChildStack([0; 16384]);

static COUNTER: AtomicI64 = AtomicI64::new(0);
static DONE: AtomicI64 = AtomicI64::new(0);

fn stack_top(s: *mut ChildStack) -> u64 {
    let base = s as *mut u8;
    // SAFETY: `s` points at a valid, live `ChildStack`; `add` stays within
    // (one past) that object, which is legal for pointer arithmetic.
    let top = unsafe { base.add(core::mem::size_of::<ChildStack>()) };
    ((top as usize) & !0xF) as u64
}

/// Spawn a worker that increments COUNTER `iters` times (yielding between
/// increments to exercise sys_sched_yield's context-switch path), then
/// exits just itself (not the whole VM) via the raw `exit` syscall.
fn spawn_worker(stack: *mut ChildStack, iters: i64) -> i64 {
    let tid = sys_clone(
        CLONE_REQUIRED,
        stack_top(stack),
        core::ptr::null_mut(),
        core::ptr::null_mut(),
        0,
    );
    if tid == 0 {
        for _ in 0..iters {
            COUNTER.fetch_add(1, Ordering::Relaxed);
            sys_sched_yield();
        }
        DONE.fetch_add(1, Ordering::Release);
        exit_thread(0);
    }
    tid
}

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let iters: i64 = 100;

    let tid_a = spawn_worker(core::ptr::addr_of_mut!(STACK_A), iters);
    check!(tid_a > 0, b"clone A failed");
    let tid_b = spawn_worker(core::ptr::addr_of_mut!(STACK_B), iters);
    check!(tid_b > 0, b"clone B failed");
    check!(tid_a != tid_b, b"clone returned duplicate TIDs");

    // Poll until both workers report done. Upper bound: ~10x the expected
    // yield count plus headroom.
    let mut spins: i64 = 0;
    let max_spins: i64 = iters * 10 + 1000;
    while DONE.load(Ordering::Acquire) < 2 {
        sys_sched_yield();
        spins += 1;
        check!(spins < max_spins, b"sched_yield spun too long");
    }

    // Both workers finished: counter must equal iters * 2.
    check_eq!(COUNTER.load(Ordering::Acquire), iters * 2);

    pass!();
}
