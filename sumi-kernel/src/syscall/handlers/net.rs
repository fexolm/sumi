use crate::syscall::errno::*;
use crate::syscall::{SyscallArgs, SyscallResult};

pub fn sys_socket(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_connect(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_accept(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_sendto(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_recvfrom(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_sendmsg(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_recvmsg(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_shutdown(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_bind(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_listen(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_getsockname(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_getpeername(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_setsockopt(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_getsockopt(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}
