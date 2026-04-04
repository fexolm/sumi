use crate::syscall::{ENOSYS, SyscallArgs, SyscallResult};

pub fn sys_rt_sigaction(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_rt_sigprocmask(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_rt_sigreturn(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_pause(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_kill(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_rt_sigsuspend(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_rt_sigpending(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}
