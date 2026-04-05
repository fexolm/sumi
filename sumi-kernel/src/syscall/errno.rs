use super::SyscallResult;

pub const EIO: SyscallResult = -5;
pub const EBADF: SyscallResult = -9;
pub const ENOMEM: SyscallResult = -12;
pub const EFAULT: SyscallResult = -14;
pub const EINVAL: SyscallResult = -22;
pub const EMFILE: SyscallResult = -24;
pub const ENOSYS: SyscallResult = -38;
