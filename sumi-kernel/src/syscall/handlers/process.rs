use crate::kprintln;
use crate::syscall::errno::*;
use crate::syscall::{SyscallArgs, SyscallResult};

const ARCH_SET_FS: u64 = 0x1002;
const ARCH_GET_FS: u64 = 0x1003;
#[cfg(not(test))]
const IA32_FS_BASE: u32 = 0xC000_0100;

pub fn sys_getpid(_args: &SyscallArgs) -> SyscallResult {
    1
}

pub fn sys_exit(args: &SyscallArgs) -> SyscallResult {
    exit_with_code(args.arg0 as i32)
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

pub fn sys_arch_prctl(args: &SyscallArgs) -> SyscallResult {
    let code = args.arg0;
    let _addr = args.arg1;

    match code {
        ARCH_SET_FS => {
            #[cfg(not(test))]
            {
                // SAFETY: We are in ring 0 and IA32_FS_BASE is a valid MSR.
                unsafe {
                    crate::arch::x86_64::syscall::wrmsr(IA32_FS_BASE, _addr);
                }
            }
            0
        }
        ARCH_GET_FS => {
            #[cfg(not(test))]
            {
                let val = unsafe { crate::arch::x86_64::syscall::rdmsr(IA32_FS_BASE) };
                // SAFETY: User passed a valid pointer for the result.
                unsafe {
                    *(_addr as *mut u64) = val;
                }
            }
            0
        }
        _ => EINVAL,
    }
}

pub fn sys_gettid(_args: &SyscallArgs) -> SyscallResult {
    1
}

pub fn sys_exit_group(args: &SyscallArgs) -> SyscallResult {
    exit_with_code(args.arg0 as i32)
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

pub fn sys_set_tid_address(_args: &SyscallArgs) -> SyscallResult {
    // Store the pointer (ignored in single-threaded unikernel).
    // Return the TID (always 1).
    1
}

// glibc 2.34+ calls prctl(PR_SET_VMA, PR_SET_VMA_ANON_NAME, ...) to label
// anonymous VMAs. We don't track VMA names; return success silently to keep
// startup log output clean. glibc tolerates failure here.
pub fn sys_prctl(_args: &SyscallArgs) -> SyscallResult {
    0
}

fn exit_with_code(code: i32) -> SyscallResult {
    kprintln!("[exit] code={}", code);
    crate::arch::halt_forever()
}

#[cfg(test)]
mod uname_tests {
    use super::*;

    fn call_uname() -> UtsName {
        let mut uts: UtsName = unsafe { core::mem::zeroed() };
        let args = SyscallArgs {
            nr: 63,
            arg0: &mut uts as *mut UtsName as u64,
            arg1: 0,
            arg2: 0,
            arg3: 0,
            arg4: 0,
            arg5: 0,
        };
        assert_eq!(sys_uname(&args), 0);
        uts
    }

    fn cstr_eq(field: &[u8], expected: &[u8]) -> bool {
        let n = field.iter().position(|&b| b == 0).unwrap_or(field.len());
        &field[..n] == expected
    }

    #[test]
    fn uname_sysname_is_linux() {
        let uts = call_uname();
        assert!(cstr_eq(&uts.sysname, b"Linux"));
    }

    #[test]
    fn uname_release_is_versioned() {
        let uts = call_uname();
        assert!(cstr_eq(&uts.release, b"6.6.0-sumi"));
    }

    #[test]
    fn uname_machine_is_x86_64() {
        let uts = call_uname();
        assert!(cstr_eq(&uts.machine, b"x86_64"));
    }

    #[test]
    fn uname_domainname_is_none_paren() {
        let uts = call_uname();
        assert!(cstr_eq(&uts.domainname, b"(none)"));
    }

    #[test]
    fn uname_returns_zero_smoke() {
        let _ = call_uname();
    }

    #[test]
    fn uname_fields_are_null_terminated_at_64() {
        let uts = call_uname();
        assert_eq!(uts.sysname[64], 0);
        assert_eq!(uts.nodename[64], 0);
        assert_eq!(uts.release[64], 0);
        assert_eq!(uts.version[64], 0);
        assert_eq!(uts.machine[64], 0);
        assert_eq!(uts.domainname[64], 0);
    }

    #[test]
    fn uname_null_buf_returns_efault() {
        let args = SyscallArgs {
            nr: 63,
            arg0: 0,
            arg1: 0,
            arg2: 0,
            arg3: 0,
            arg4: 0,
            arg5: 0,
        };
        assert_eq!(sys_uname(&args), EFAULT);
    }
}
