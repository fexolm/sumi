use crate::syscall::{ENOSYS, SyscallArgs, SyscallResult};

pub fn sys_getpid(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_exit(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_getuid(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_getgid(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_geteuid(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_getegid(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_getppid(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_arch_prctl(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_gettid(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_exit_group(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}
