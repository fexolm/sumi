#![no_std]
#![no_main]
include!("../common.rs");

use core::sync::atomic::{AtomicI64, AtomicU32, Ordering};

// Two worker stacks — one per cloned child.
#[repr(C, align(16))]
struct ChildStack([u8; 16384]);
static mut STACK_A: ChildStack = ChildStack([0; 16384]);
static mut STACK_B: ChildStack = ChildStack([0; 16384]);

// Ping-pong turn variable: worker A proceeds on TURN==1, worker B on
// TURN==2. Each flips it back for the other side and wakes it.
static TURN: AtomicU32 = AtomicU32::new(1);
static DONE: AtomicI64 = AtomicI64::new(0);

fn stack_top(s: *mut ChildStack) -> u64 {
    let base = s as *mut u8;
    // SAFETY: `s` points at a valid, live `ChildStack`; `add` stays within
    // (one past) that object, which is legal for pointer arithmetic.
    let top = unsafe { base.add(core::mem::size_of::<ChildStack>()) };
    ((top as usize) & !0xF) as u64
}

fn turn_addr() -> *const u32 {
    &TURN as *const AtomicU32 as *const u32
}

/// Spawn a worker that ping-pongs `iters` times via FUTEX_WAIT/FUTEX_WAKE,
/// waiting for `TURN == wait_for` and flipping it to `wait_for`'s
/// counterpart on each round, then exits just itself.
fn spawn_worker(stack: *mut ChildStack, iters: u64, wait_for: u32, flip_to: u32) -> i64 {
    let tid = sys_clone(
        CLONE_REQUIRED,
        stack_top(stack),
        core::ptr::null_mut(),
        core::ptr::null_mut(),
        0,
    );
    if tid == 0 {
        let addr = turn_addr();
        for _ in 0..iters {
            loop {
                let v = TURN.load(Ordering::Acquire);
                if v == wait_for {
                    break;
                }
                // EAGAIN (value already changed) just retries the load.
                let _ = sys_futex(addr, FUTEX_WAIT, v as u64);
            }
            TURN.store(flip_to, Ordering::Release);
            let _ = sys_futex(addr, FUTEX_WAKE, 1);
        }
        DONE.fetch_add(1, Ordering::Release);
        exit_thread(0);
    }
    tid
}

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let iters: u64 = 1000;

    let tid_a = spawn_worker(core::ptr::addr_of_mut!(STACK_A), iters, 1, 2);
    check!(tid_a > 0, b"clone A failed");
    let tid_b = spawn_worker(core::ptr::addr_of_mut!(STACK_B), iters, 2, 1);
    check!(tid_b > 0, b"clone B failed");
    check!(tid_a != tid_b, b"clone returned duplicate TIDs");

    // Poll until both workers have finished. 50x headroom on the
    // expected ~2*iters context switches.
    let mut spins: i64 = 0;
    let max_spins: i64 = (iters as i64) * 50 + 10_000;
    while DONE.load(Ordering::Acquire) < 2 {
        sys_sched_yield();
        spins += 1;
        check!(spins < max_spins, b"futex ping-pong took too long");
    }

    pass!();
}
