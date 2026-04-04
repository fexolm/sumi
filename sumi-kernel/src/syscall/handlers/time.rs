use crate::syscall::{ENOSYS, SyscallArgs, SyscallResult};

pub fn sys_nanosleep(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_gettimeofday(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_getrlimit(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_clock_gettime(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_clock_getres(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_clock_nanosleep(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}
