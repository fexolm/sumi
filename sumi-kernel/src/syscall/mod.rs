pub mod errno;
pub mod handlers;

#[repr(C)]
pub struct SyscallArgs {
    pub nr: u64,   // 0
    pub arg0: u64, // 8
    pub arg1: u64, // 16
    pub arg2: u64, // 24
    pub arg3: u64, // 32
    pub arg4: u64, // 40
    pub arg5: u64, // 48
    /// User RIP at the time of `syscall` (CPU saves rcx = next RIP).
    pub caller_rip: u64, // 56
    /// User RFLAGS at the time of `syscall` (CPU saves r11 = RFLAGS).
    pub caller_rflags: u64, // 64
}

// The syscall_entry asm hard-codes these offsets via `lea` arithmetic.
// A bad field reorder must be a build-time error.
const _: () = {
    assert!(core::mem::offset_of!(SyscallArgs, nr) == 0);
    assert!(core::mem::offset_of!(SyscallArgs, arg0) == 8);
    assert!(core::mem::offset_of!(SyscallArgs, arg1) == 16);
    assert!(core::mem::offset_of!(SyscallArgs, arg2) == 24);
    assert!(core::mem::offset_of!(SyscallArgs, arg3) == 32);
    assert!(core::mem::offset_of!(SyscallArgs, arg4) == 40);
    assert!(core::mem::offset_of!(SyscallArgs, arg5) == 48);
    assert!(core::mem::offset_of!(SyscallArgs, caller_rip) == 56);
    assert!(core::mem::offset_of!(SyscallArgs, caller_rflags) == 64);
    assert!(core::mem::size_of::<SyscallArgs>() == 72);
};

pub type SyscallResult = i64;

#[unsafe(no_mangle)]
pub extern "C" fn syscall_dispatch(args: &SyscallArgs) -> SyscallResult {
    let ret = match args.nr {
        0 => handlers::io::sys_read(args),
        1 => handlers::io::sys_write(args),
        2 => handlers::io::sys_open(args),
        3 => handlers::io::sys_close(args),
        7 => handlers::io::sys_poll(args),
        8 => handlers::io::sys_lseek(args),
        16 => handlers::io::sys_ioctl(args),
        72 => handlers::io::sys_fcntl(args),
        17 => handlers::io::sys_pread64(args),
        18 => handlers::io::sys_pwrite64(args),
        19 => handlers::io::sys_readv(args),
        20 => handlers::io::sys_writev(args),
        22 => handlers::io::sys_pipe(args),
        293 => handlers::io::sys_pipe2(args),
        23 => handlers::io::sys_select(args),
        32 => handlers::io::sys_dup(args),
        33 => handlers::io::sys_dup2(args),
        4 => handlers::fs::sys_stat(args),
        5 => handlers::fs::sys_fstat(args),
        6 => handlers::fs::sys_lstat(args),
        21 => handlers::fs::sys_access(args),
        269 => handlers::fs::sys_faccessat(args),
        439 => handlers::fs::sys_faccessat2(args),
        78 => handlers::fs::sys_getdents(args),
        79 => handlers::fs::sys_getcwd(args),
        80 => handlers::fs::sys_chdir(args),
        81 => handlers::fs::sys_fchdir(args),
        82 => handlers::fs::sys_rename(args),
        83 => handlers::fs::sys_mkdir(args),
        84 => handlers::fs::sys_rmdir(args),
        85 => handlers::fs::sys_creat(args),
        86 => handlers::fs::sys_link(args),
        87 => handlers::fs::sys_unlink(args),
        88 => handlers::fs::sys_symlink(args),
        89 => handlers::fs::sys_readlink(args),
        217 => handlers::fs::sys_getdents64(args),
        257 => handlers::fs::sys_openat(args),
        262 => handlers::fs::sys_newfstatat(args),
        263 => handlers::fs::sys_unlinkat(args),
        9 => handlers::memory::sys_mmap(args),
        10 => handlers::memory::sys_mprotect(args),
        11 => handlers::memory::sys_munmap(args),
        12 => handlers::memory::sys_brk(args),
        25 => handlers::memory::sys_mremap(args),
        26 => handlers::memory::sys_msync(args),
        27 => handlers::memory::sys_mincore(args),
        28 => handlers::memory::sys_madvise(args),
        39 => handlers::process::sys_getpid(args),
        60 => handlers::process::sys_exit(args),
        74 => handlers::io::sys_fsync(args),
        75 => handlers::io::sys_fdatasync(args),
        95 => handlers::process::sys_umask(args),
        99 => handlers::process::sys_sysinfo(args),
        102 => handlers::process::sys_getuid(args),
        104 => handlers::process::sys_getgid(args),
        107 => handlers::process::sys_geteuid(args),
        108 => handlers::process::sys_getegid(args),
        110 => handlers::process::sys_getppid(args),
        157 => handlers::process::sys_prctl(args),
        158 => handlers::process::sys_arch_prctl(args),
        186 => handlers::process::sys_gettid(args),
        231 => handlers::process::sys_exit_group(args),
        13 => handlers::signal::sys_rt_sigaction(args),
        14 => handlers::signal::sys_rt_sigprocmask(args),
        15 => handlers::signal::sys_rt_sigreturn(args),
        34 => handlers::signal::sys_pause(args),
        62 => handlers::signal::sys_kill(args),
        129 => handlers::signal::sys_rt_sigsuspend(args),
        130 => handlers::signal::sys_rt_sigpending(args),
        131 => handlers::signal::sys_sigaltstack(args),
        200 => handlers::signal::sys_tkill(args),
        234 => handlers::signal::sys_tgkill(args),
        35 => handlers::time::sys_nanosleep(args),
        96 => handlers::time::sys_gettimeofday(args),
        201 => handlers::time::sys_time(args),
        97 => handlers::time::sys_getrlimit(args),
        228 => handlers::time::sys_clock_gettime(args),
        229 => handlers::time::sys_clock_getres(args),
        230 => handlers::time::sys_clock_nanosleep(args),
        63 => handlers::process::sys_uname(args),
        218 => handlers::process::sys_set_tid_address(args),
        302 => handlers::time::sys_prlimit64(args),
        24 => handlers::thread::sys_sched_yield(args),
        202 => handlers::thread::sys_futex(args),
        204 => handlers::thread::sys_sched_getaffinity(args),
        273 => handlers::thread::sys_set_robust_list(args),
        318 => handlers::random::sys_getrandom(args),
        334 => errno::ENOSYS, // rseq: glibc falls back gracefully
        41 => handlers::net::sys_socket(args),
        42 => handlers::net::sys_connect(args),
        43 => handlers::net::sys_accept(args),
        44 => handlers::net::sys_sendto(args),
        45 => handlers::net::sys_recvfrom(args),
        46 => handlers::net::sys_sendmsg(args),
        47 => handlers::net::sys_recvmsg(args),
        48 => handlers::net::sys_shutdown(args),
        49 => handlers::net::sys_bind(args),
        50 => handlers::net::sys_listen(args),
        51 => handlers::net::sys_getsockname(args),
        52 => handlers::net::sys_getpeername(args),
        54 => handlers::net::sys_setsockopt(args),
        55 => handlers::net::sys_getsockopt(args),
        56 => handlers::clone::sys_clone(args),
        213 => handlers::epoll::sys_epoll_create(args),
        232 => handlers::epoll::sys_epoll_wait(args),
        233 => handlers::epoll::sys_epoll_ctl(args),
        281 => handlers::epoll::sys_epoll_pwait(args),
        288 => handlers::net::sys_accept4(args),
        291 => handlers::epoll::sys_epoll_create1(args),
        435 => handlers::clone::sys_clone3(args),
        nr => {
            crate::kprintln!("[syscall] unhandled nr={}", nr);
            errno::ENOSYS
        }
    };

    // If clone() or wake_blocked set need_resched while we were in the
    // handler, schedule a context switch before returning to user code.
    {
        use core::sync::atomic::Ordering;
        let cpu = crate::sched::percpu::this_cpu();

        // If another CPU cleared or restored page-table entries
        // (mprotect/munmap) and bumped TLB_GENERATION, flush our local TLB
        // by reloading CR3 before returning to user code. The timer-tick
        // handler performs the same check for the preemption return path.
        // No-op under test (see `PerCpu::reload_tlb_if_stale`'s host stand-in).
        cpu.reload_tlb_if_stale();

        if cpu.need_resched.swap(false, Ordering::AcqRel) {
            let current = crate::sched::current_thread();
            let idle_ptr = cpu.idle_thread.load(Ordering::Relaxed);
            if !core::ptr::eq(current as *const _, idle_ptr as *const _) {
                cpu.runqueue.push(current);
            }
            crate::sched::schedule();
        }
    }

    ret
}
