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
    crate::sched::current_thread().tgid.0 as i64
}

/// Terminate the VM: real hypercall MMIO write in production (never
/// returns — the host tears down every vCPU thread). Host stand-in: there
/// is no VM to terminate under `cargo test`, so this panics with a
/// distinguishing message instead of silently falling through a `-> !` fn.
#[cfg(not(test))]
fn shutdown_vm(exit_code: i32) -> ! {
    crate::arch::x86_64::hypercall::shutdown(exit_code)
}

#[cfg(test)]
fn shutdown_vm(exit_code: i32) -> ! {
    panic!("hypercall shutdown({exit_code}) reached in host test build");
}

pub fn sys_exit(args: &SyscallArgs) -> SyscallResult {
    use crate::sched::{self, current_thread, reaper, registry, thread::ThreadState};
    use core::sync::atomic::{AtomicU32, Ordering};

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
            kprintln!(
                "[exit] BUG: current thread tid={} not in registry",
                me.tid.0
            );
            shutdown_vm(1);
        }
    };

    // 3. Last live *user* thread → exit_group semantics (terminate VM).
    // registry::alive_count() also counts idle threads and unreaped
    // zombies, so it can never actually reach 1 here; LIVE_USER_THREADS tracks
    // only the BSP main thread + clone() children.
    if registry::LIVE_USER_THREADS.fetch_sub(1, Ordering::AcqRel) == 1 {
        kprintln!("[exit] code={}", code);
        shutdown_vm(code);
    }

    // 4. Transfer Arc to zombie list. Reaper will unregister + free.
    reaper::push_zombie(me_arc);

    // 5. Mark Exited so schedule's CAS doesn't re-enqueue us.
    me.state
        .store(ThreadState::Exited as u32, Ordering::Release);

    // 6. Final schedule — never returns to this thread.
    sched::schedule();
    panic!("schedule() returned to Exited thread tid={}", me.tid.0);
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

/// Load IA32_FS_BASE with `addr` so %fs-relative addressing (TLS) sees the
/// new base immediately. Host stand-in: no MSR on the test host — every
/// test-reachable read of the FS base goes through `current_thread().fs_base`
/// instead, which the caller updates before calling this leaf.
#[cfg(not(test))]
fn set_fs_base_msr(addr: u64) {
    // SAFETY: ring 0, valid MSR, canonical value verified by the caller.
    unsafe {
        crate::arch::x86_64::syscall::wrmsr(IA32_FS_BASE, addr);
    }
}

#[cfg(test)]
fn set_fs_base_msr(_addr: u64) {}

pub fn sys_arch_prctl(args: &SyscallArgs) -> SyscallResult {
    let code = args.arg0;
    let addr = args.arg1;

    match code {
        ARCH_SET_FS => {
            if !is_canonical_user(addr) {
                return EINVAL;
            }
            use core::sync::atomic::Ordering;
            // Update the authoritative per-thread value FIRST so a
            // racing IRQ-driven context switch never reloads a stale fs_base.
            crate::sched::current_thread()
                .fs_base
                .store(addr, Ordering::Relaxed);
            set_fs_base_msr(addr);
            0
        }
        ARCH_GET_FS => {
            if addr == 0 {
                return EFAULT;
            }
            use core::sync::atomic::Ordering;
            let v = crate::sched::current_thread()
                .fs_base
                .load(Ordering::Relaxed);
            // SAFETY: unikernel shares the address space; bad pointer
            // traps via #PF.
            unsafe {
                *(addr as *mut u64) = v;
            }
            0
        }
        _ => EINVAL,
    }
}

pub fn sys_gettid(_args: &SyscallArgs) -> SyscallResult {
    crate::sched::current_thread().tid.0 as i64
}

pub fn sys_exit_group(args: &SyscallArgs) -> SyscallResult {
    let code = args.arg0 as i32;
    kprintln!("[exit] code={}", code);
    shutdown_vm(code);
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
    use core::sync::atomic::Ordering;
    let t = crate::sched::current_thread();
    t.clear_child_tid.store(args.arg0, Ordering::Relaxed);
    t.tid.0 as i64
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

/// `struct sysinfo` for x86_64 Linux (112 bytes) — see `man 2 sysinfo`.
/// Field order/types follow the kernel uapi definition exactly so repr(C)
/// reproduces its padding (4 bytes before `totalhigh`, 4 bytes of tail
/// padding after `mem_unit`) without listing it explicitly.
#[repr(C)]
struct Sysinfo {
    uptime: i64,
    loads: [u64; 3],
    totalram: u64,
    freeram: u64,
    sharedram: u64,
    bufferram: u64,
    totalswap: u64,
    freeswap: u64,
    procs: u16,
    pad: u16,
    totalhigh: u64,
    freehigh: u64,
    mem_unit: u32,
    _f: [u8; 0],
}

const _: () = assert!(core::mem::size_of::<Sysinfo>() == 112);

/// mysqld's startup sanity checks call `sysinfo()` for `totalram`/`freeram`.
/// `loads`/swap are always reported as zero — sumi has no load average or
/// swap. `mem_unit = 1` so `totalram`/`freeram` are plain byte counts.
pub fn sys_sysinfo(args: &SyscallArgs) -> SyscallResult {
    let buf = args.arg0 as *mut Sysinfo;
    if buf.is_null() {
        return EFAULT;
    }

    let uptime = (crate::time::monotonic_ns() / 1_000_000_000) as i64;
    let stats = crate::PAGE_ALLOCATOR.get_stats();
    let total_pages = stats.allocatable_limit_pages;
    let free_pages = total_pages.saturating_sub(stats.used_pages);
    let page_size = sumi_abi::arch::layout::PAGE_SIZE as u64;

    // SAFETY: single-process unikernel — the caller-provided pointer is a
    // valid, writable `struct sysinfo` per the syscall ABI.
    unsafe {
        core::ptr::write_bytes(buf, 0, 1);
        (*buf).uptime = uptime;
        (*buf).totalram = total_pages as u64 * page_size;
        (*buf).freeram = free_pages as u64 * page_size;
        (*buf).procs = 1;
        (*buf).mem_unit = 1;
    }
    0
}

/// `umask(mask)`: sumi has no permission model — accept and discard the
/// new mask, reporting the conventional previous value.
pub fn sys_umask(_args: &SyscallArgs) -> SyscallResult {
    0o022
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_args(arg0: u64, arg1: u64) -> SyscallArgs {
        SyscallArgs {
            nr: 158,
            arg0,
            arg1,
            arg2: 0,
            arg3: 0,
            arg4: 0,
            arg5: 0,
            caller_rip: 0,
            caller_rflags: 0,
        }
    }

    #[test]
    fn arch_prctl_set_fs_noncanonical_rejected() {
        // Bit 47 set → non-canonical.
        let args = make_args(ARCH_SET_FS, 0xFFFF_8000_0000_0000);
        assert_eq!(sys_arch_prctl(&args), EINVAL);
    }

    #[test]
    fn arch_prctl_set_fs_canonical_accepted_updates_current_thread() {
        let t: &'static crate::sched::thread::Thread =
            Box::leak(Box::new(crate::sched::thread::Thread::new_test(9101)));
        crate::sched::percpu::install_test_thread(0, t);
        let args = make_args(ARCH_SET_FS, 0x7FFF_FFFF_0000);
        assert_eq!(sys_arch_prctl(&args), 0);
        assert_eq!(
            t.fs_base.load(core::sync::atomic::Ordering::Relaxed),
            0x7FFF_FFFF_0000,
        );
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
