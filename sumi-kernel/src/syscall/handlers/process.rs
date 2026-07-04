use crate::kprintln;
use crate::syscall::errno::*;
use crate::syscall::{SyscallArgs, SyscallResult};

const ARCH_SET_FS: u64 = 0x1002;
const ARCH_GET_FS: u64 = 0x1003;
#[cfg(not(test))]
const IA32_FS_BASE: u32 = 0xC000_0100;

/// Reject non-canonical user addresses. Writing a non-canonical value to
/// IA32_FS_BASE raises #GP on the `wrmsr` inside __switch_to_asm. Linux
/// returns EINVAL for the same condition.
#[inline]
fn is_canonical_user(val: u64) -> bool {
    // sumi runs user code in the lower half. We accept addresses with
    // bits 47..63 all zero; sign-extended kernel addresses are rejected.
    (val >> 47) == 0
}

pub fn sys_getpid(_args: &SyscallArgs) -> SyscallResult {
    #[cfg(not(test))]
    { crate::sched::current_thread().tgid.0 as i64 }
    #[cfg(test)]
    { 1 }
}

pub fn sys_exit(args: &SyscallArgs) -> SyscallResult {
    #[cfg(not(test))]
    {
        use core::sync::atomic::{AtomicU32, Ordering};
        use crate::sched::{self, current_thread, reaper, registry, thread::ThreadState};

        let code = args.arg0 as i32;
        let me = current_thread();
        me.exit_code.store(code, Ordering::Release);

        // 1. CLONE_CHILD_CLEARTID handshake: zero *ctid and futex_wake.
        //    Without this, pthread_join hangs forever.
        let clear = me.clear_child_tid.swap(0, Ordering::AcqRel);
        if clear != 0 {
            // SAFETY: sumi is a unikernel — kernel and user share the same
            // address space. The address was written by set_tid_address and
            // points at a u32 in the user's thread-local storage. A bad
            // pointer will fault via #PF, which is acceptable for a dying thread.
            unsafe {
                AtomicU32::from_ptr(clear as *mut u32).store(0, Ordering::Release);
            }
            let _ = crate::sched::futex::wake(clear as *const u32, 1);
        }

        // 2. Grab an Arc from the registry (for the zombie list).
        let me_arc = match registry::lookup(me.tid) {
            Some(a) => a,
            None => {
                // Running thread not in registry is a hard invariant violation.
                kprintln!("[exit] BUG: current thread tid={} not in registry", me.tid.0);
                crate::arch::x86_64::hypercall::shutdown(1);
            }
        };

        // 3. Last live *user* thread → exit_group semantics (terminate VM).
        // registry::alive_count() also counts idle threads and unreaped
        // zombies, so it can never actually reach 1 here (F8); LIVE_USER_THREADS
        // tracks only the BSP main thread + clone() children.
        if registry::LIVE_USER_THREADS.fetch_sub(1, Ordering::AcqRel) == 1 {
            kprintln!("[exit] code={}", code);
            crate::arch::x86_64::hypercall::shutdown(code);
        }

        // 4. Transfer Arc to zombie list. Reaper will unregister + free.
        reaper::push_zombie(me_arc);

        // 5. Mark Exited so schedule's CAS doesn't re-enqueue us.
        me.state.store(ThreadState::Exited as u32, Ordering::Release);

        // 6. Final schedule — never returns to this thread.
        sched::schedule();
        panic!("schedule() returned to Exited thread tid={}", me.tid.0);
    }
    #[cfg(test)]
    {
        let _ = args;
        panic!("sys_exit reached in host test build");
    }
}

pub fn sys_getuid(_args: &SyscallArgs) -> SyscallResult {
    0
}

pub fn sys_getgid(_args: &SyscallArgs) -> SyscallResult {
    0
}

pub fn sys_geteuid(_args: &SyscallArgs) -> SyscallResult {
    0
}

pub fn sys_getegid(_args: &SyscallArgs) -> SyscallResult {
    0
}

pub fn sys_getppid(_args: &SyscallArgs) -> SyscallResult {
    0
}

pub fn sys_arch_prctl(args: &SyscallArgs) -> SyscallResult {
    let code = args.arg0;
    let addr = args.arg1;

    match code {
        ARCH_SET_FS => {
            if !is_canonical_user(addr) {
                return EINVAL;
            }
            #[cfg(not(test))]
            {
                use core::sync::atomic::Ordering;
                // Update the authoritative per-thread value FIRST so a
                // racing context switch (Phase 9: IRQ-driven preemption)
                // never reloads a stale fs_base.
                crate::sched::current_thread()
                    .fs_base
                    .store(addr, Ordering::Relaxed);
                // SAFETY: ring 0, valid MSR, canonical value verified above.
                unsafe {
                    crate::arch::x86_64::syscall::wrmsr(IA32_FS_BASE, addr);
                }
            }
            #[cfg(test)]
            { let _ = addr; }
            0
        }
        ARCH_GET_FS => {
            if addr == 0 {
                return EFAULT;
            }
            #[cfg(not(test))]
            {
                use core::sync::atomic::Ordering;
                let v = crate::sched::current_thread()
                    .fs_base
                    .load(Ordering::Relaxed);
                // SAFETY: unikernel shares the address space; bad pointer
                // traps via #PF.
                unsafe { *(addr as *mut u64) = v; }
            }
            0
        }
        _ => EINVAL,
    }
}

pub fn sys_gettid(_args: &SyscallArgs) -> SyscallResult {
    #[cfg(not(test))]
    { crate::sched::current_thread().tid.0 as i64 }
    #[cfg(test)]
    { 1 }
}

pub fn sys_exit_group(args: &SyscallArgs) -> SyscallResult {
    let code = args.arg0 as i32;
    kprintln!("[exit] code={}", code);
    #[cfg(not(test))]
    {
        crate::arch::x86_64::hypercall::shutdown(code);
    }
    #[cfg(test)]
    {
        panic!("sys_exit_group({}) reached in host test build", code);
    }
}

#[repr(C)]
struct UtsName {
    sysname: [u8; 65],
    nodename: [u8; 65],
    release: [u8; 65],
    version: [u8; 65],
    machine: [u8; 65],
    domainname: [u8; 65],
}

pub fn sys_uname(args: &SyscallArgs) -> SyscallResult {
    let buf = args.arg0 as *mut UtsName;
    if buf.is_null() {
        return EFAULT;
    }
    // SAFETY: Single-process unikernel; the user pointer lives in the same
    // address space as the kernel. Caller is responsible for the buffer being
    // a writable UtsName-sized region.
    unsafe {
        core::ptr::write_bytes(buf, 0, 1);
        write_field(&mut (*buf).sysname, b"Linux");
        write_field(&mut (*buf).nodename, b"sumi");
        write_field(&mut (*buf).release, b"6.6.0-sumi");
        write_field(&mut (*buf).version, b"#1 SMP sumi");
        write_field(&mut (*buf).machine, b"x86_64");
        write_field(&mut (*buf).domainname, b"(none)");
    }
    0
}

fn write_field(field: &mut [u8; 65], val: &[u8]) {
    let len = core::cmp::min(val.len(), 64);
    field[..len].copy_from_slice(&val[..len]);
}

pub fn sys_set_tid_address(args: &SyscallArgs) -> SyscallResult {
    #[cfg(not(test))]
    {
        use core::sync::atomic::Ordering;
        let t = crate::sched::current_thread();
        t.clear_child_tid.store(args.arg0, Ordering::Relaxed);
        t.tid.0 as i64
    }
    #[cfg(test)]
    { let _ = args; 1 }
}

// PR_SET_VMA: glibc 2.34+ calls prctl(PR_SET_VMA, PR_SET_VMA_ANON_NAME, ...)
// to label anonymous VMAs. We don't track VMA names — accept it as a
// successful no-op so glibc startup stays quiet. Every other prctl op is
// rejected with EINVAL: returning 0 from getter-style ops (PR_GET_NAME etc.)
// would silently leave the caller's output buffer untouched and they would
// then read uninitialised data.
const PR_SET_VMA: u64 = 0x53564D41;

pub fn sys_prctl(args: &SyscallArgs) -> SyscallResult {
    match args.arg0 {
        PR_SET_VMA => 0,
        _ => EINVAL,
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn make_args(arg0: u64, arg1: u64) -> SyscallArgs {
        SyscallArgs {
            nr: 158,
            arg0, arg1,
            arg2: 0, arg3: 0, arg4: 0, arg5: 0,
            caller_rip: 0, caller_rflags: 0,
        }
    }

    #[test]
    fn arch_prctl_set_fs_noncanonical_rejected() {
        // Bit 47 set → non-canonical.
        let args = make_args(ARCH_SET_FS, 0xFFFF_8000_0000_0000);
        assert_eq!(sys_arch_prctl(&args), EINVAL);
    }

    #[test]
    fn arch_prctl_set_fs_canonical_accepted_in_test_build() {
        // In test build, the cfg(not(test)) branch is skipped; just the
        // canonical check + return 0.
        let args = make_args(ARCH_SET_FS, 0x7FFF_FFFF_0000);
        assert_eq!(sys_arch_prctl(&args), 0);
    }

    #[test]
    fn arch_prctl_get_fs_null_rejected() {
        let args = make_args(ARCH_GET_FS, 0);
        assert_eq!(sys_arch_prctl(&args), EFAULT);
    }

    #[test]
    fn arch_prctl_unknown_code_returns_einval() {
        let args = make_args(0x9999, 0);
        assert_eq!(sys_arch_prctl(&args), EINVAL);
    }
}

