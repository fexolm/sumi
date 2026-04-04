use crate::syscall::{ENOSYS, SyscallArgs, SyscallResult};

pub fn sys_read(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_write(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_open(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_close(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_poll(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_lseek(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_ioctl(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_pread64(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_pwrite64(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_readv(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_writev(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_pipe(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_select(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_dup(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_dup2(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}
