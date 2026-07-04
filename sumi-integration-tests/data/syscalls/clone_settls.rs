#![no_std]
#![no_main]
include!("../common.rs");

use core::sync::atomic::{AtomicI64, Ordering};

const CLONE_SETTLS: u64 = 0x0008_0000;
const SENTINEL: u64 = 0x1122_3344_5566_7788;

#[repr(C, align(16))]
struct ChildStack([u8; 16384]);
static mut CHILD_STACK: ChildStack = ChildStack([0; 16384]);

// The TLS area handed to clone() via `tls=`. clone() loads fs_base to this
// address; the child must observe `fs:[0] == SENTINEL` once it runs.
#[repr(C, align(16))]
struct TlsArea([u64; 4]);
static mut TLS_AREA: TlsArea = TlsArea([0; 4]);

// Result the child reports back: 0 = not run yet, 1 = fs:0 matched, 2 = mismatch.
static RESULT: AtomicI64 = AtomicI64::new(0);

fn stack_top(s: *mut ChildStack) -> u64 {
    let base = s as *mut u8;
    // SAFETY: `s` points at a valid, live `ChildStack`; `add` stays within
    // (one past) that object, which is legal for pointer arithmetic.
    let top = unsafe { base.add(core::mem::size_of::<ChildStack>()) };
    ((top as usize) & !0xF) as u64
}

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    unsafe {
        core::ptr::write_volatile(core::ptr::addr_of_mut!(TLS_AREA.0[0]), SENTINEL);
    }
    let tls_ptr = unsafe { core::ptr::addr_of!(TLS_AREA) as u64 };

    let tid = sys_clone(
        CLONE_REQUIRED | CLONE_SETTLS,
        stack_top(core::ptr::addr_of_mut!(CHILD_STACK)),
        core::ptr::null_mut(),
        core::ptr::null_mut(),
        tls_ptr,
    );
    if tid == 0 {
        // ------------- CHILD -------------
        let mut got: u64;
        // SAFETY: reads the qword at fs:[0], which clone() set up to point
        // at TLS_AREA via fs_base = tls_ptr.
        unsafe {
            asm!("mov {0}, fs:[0]", out(reg) got, options(nostack, preserves_flags));
        }
        RESULT.store(if got == SENTINEL { 1 } else { 2 }, Ordering::Release);
        loop {
            sys_sched_yield();
        }
    }

    // ------------- PARENT -------------
    check!(tid > 0, b"clone with CLONE_SETTLS failed");
    let mut spins: i64 = 0;
    let max_spins: i64 = 200_000;
    loop {
        let r = RESULT.load(Ordering::Acquire);
        if r != 0 {
            check_eq!(r, 1);
            break;
        }
        sys_sched_yield();
        spins += 1;
        check!(spins < max_spins, b"clone_settls: child never reported");
    }

    pass!();
}
