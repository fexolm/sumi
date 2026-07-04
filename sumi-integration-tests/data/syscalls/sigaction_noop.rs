#![no_std]
#![no_main]

include!("../common.rs");

const SYS_RT_SIGACTION: u64 = 13;
const SYS_RT_SIGPROCMASK: u64 = 14;
const SIGUSR1: u64 = 10;

#[repr(C)]
struct KernelSigaction {
    sa_handler: u64,
    sa_flags: u64,
    sa_restorer: u64,
    sa_mask: u64,
}

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let new = KernelSigaction {
        sa_handler: 0,
        sa_flags: 0,
        sa_restorer: 0,
        sa_mask: 0,
    };
    let mut old = KernelSigaction {
        sa_handler: 0xDEAD,
        sa_flags: 0xBEEF,
        sa_restorer: 0,
        sa_mask: 0,
    };
    // sumi has no signal delivery; sigaction must succeed and write a clean
    // SIG_DFL into old_act so callers don't read garbage.
    let r = unsafe {
        syscall4(
            SYS_RT_SIGACTION,
            SIGUSR1,
            &new as *const _ as u64,
            &mut old as *mut _ as u64,
            8,
        )
    };
    check_eq!(r, 0);
    check_eq!(old.sa_handler, 0); // SIG_DFL
    check_eq!(old.sa_flags, 0);

    // sigprocmask must succeed and zero the old set if provided.
    let mut old_mask: u64 = 0xDEAD_BEEF;
    let r = unsafe {
        syscall4(
            SYS_RT_SIGPROCMASK,
            0,
            0,
            &mut old_mask as *mut _ as u64,
            8,
        )
    };
    check_eq!(r, 0);
    check_eq!(old_mask, 0);
    pass!();
}
