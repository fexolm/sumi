#![no_std]
#![no_main]
include!("../common.rs");

const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;
const SENTINEL: u32 = 0xBEEF;

#[repr(C, align(16))]
struct ChildStack([u8; 16384]);
static mut CHILD_STACK: ChildStack = ChildStack([0; 16384]);

// clone()'s CLONE_CHILD_CLEARTID handshake: the child's sys_exit zeroes
// this word and futex-wakes it (see sys_exit's `clear_child_tid` path).
static mut CTID: u32 = SENTINEL;

fn stack_top(s: *mut ChildStack) -> u64 {
    let base = s as *mut u8;
    // SAFETY: `s` points at a valid, live `ChildStack`; `add` stays within
    // (one past) that object, which is legal for pointer arithmetic.
    let top = unsafe { base.add(core::mem::size_of::<ChildStack>()) };
    ((top as usize) & !0xF) as u64
}

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let ctid_ptr = core::ptr::addr_of_mut!(CTID);

    let tid = sys_clone(
        CLONE_REQUIRED | CLONE_CHILD_CLEARTID,
        stack_top(core::ptr::addr_of_mut!(CHILD_STACK)),
        core::ptr::null_mut(),
        ctid_ptr as *mut i32,
        0,
    );
    if tid == 0 {
        // ------------- CHILD -------------
        // A few yields to give the parent a real chance to reach
        // FUTEX_WAIT before we clear+wake, exercising the actual
        // block/wake path instead of always racing to EAGAIN.
        for _ in 0..50 {
            sys_sched_yield();
        }
        exit_thread(0);
    }

    // ------------- PARENT -------------
    check!(tid > 0, b"clone with CLONE_CHILD_CLEARTID failed");
    let mut spins: i64 = 0;
    let max_spins: i64 = 200_000;
    loop {
        let cur = unsafe { core::ptr::read_volatile(ctid_ptr) };
        if cur == 0 {
            break;
        }
        // Either blocks until the child's exit-time wake, or returns
        // EAGAIN if `*ctid` already changed underneath us — both cases
        // just loop back to the read_volatile check above.
        let _ = sys_futex(ctid_ptr as *const u32, FUTEX_WAIT, cur as u64);
        spins += 1;
        check!(spins < max_spins, b"clear_child_tid_wakes: never woke");
    }

    check_eq!(unsafe { core::ptr::read_volatile(ctid_ptr) }, 0);
    pass!();
}
