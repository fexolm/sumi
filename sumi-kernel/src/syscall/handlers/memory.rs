use crate::syscall::{ENOSYS, SyscallArgs, SyscallResult};

pub fn sys_mmap(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_mprotect(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_munmap(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_brk(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_mremap(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_msync(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_mincore(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_madvise(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}
