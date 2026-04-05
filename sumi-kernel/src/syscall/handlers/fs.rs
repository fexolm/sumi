use crate::syscall::errno::*;
use crate::syscall::{SyscallArgs, SyscallResult};

pub fn sys_stat(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_fstat(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_lstat(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_access(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_getdents(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_getcwd(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_chdir(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_fchdir(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_rename(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_mkdir(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_rmdir(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_creat(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_link(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_unlink(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_symlink(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_readlink(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_openat(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_newfstatat(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_unlinkat(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}
