use crate::syscall::{SyscallArgs, SyscallResult};

const EINVAL: SyscallResult = -22;

const ARCH_SET_FS: u64 = 0x1002;
const ARCH_GET_FS: u64 = 0x1003;
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
    let addr = args.arg1;

    match code {
        ARCH_SET_FS => {
            #[cfg(not(test))]
            {
                // SAFETY: We are in ring 0 and IA32_FS_BASE is a valid MSR.
                unsafe {
                    crate::arch::x86_64::syscall::wrmsr(IA32_FS_BASE, addr);
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
                    *(addr as *mut u64) = val;
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

pub fn sys_uname(args: &SyscallArgs) -> SyscallResult {
    #[repr(C)]
    struct UtsName {
        sysname: [u8; 65],
        nodename: [u8; 65],
        release: [u8; 65],
        version: [u8; 65],
        machine: [u8; 65],
        domainname: [u8; 65],
    }

    let buf = args.arg0 as *mut UtsName;
    // SAFETY: User program passed a valid pointer to UtsName-sized buffer.
    unsafe {
        core::ptr::write_bytes(buf, 0, 1);
        write_field(&mut (*buf).sysname, b"sumi");
        write_field(&mut (*buf).nodename, b"sumi");
        write_field(&mut (*buf).release, b"0.1.0");
        write_field(&mut (*buf).version, b"0.1.0");
        write_field(&mut (*buf).machine, b"x86_64");
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

fn exit_with_code(code: i32) -> SyscallResult {
    use crate::selftest::debugcon_puts;

    if code == 0 {
        debugcon_puts("[exit] code=0\n");
    } else {
        debugcon_puts("[exit] code=");
        // Simple decimal output for the exit code
        if code < 0 {
            crate::arch::debugcon_write_byte(b'-');
            print_decimal((-code) as u32);
        } else {
            print_decimal(code as u32);
        }
        debugcon_puts("\n");
    }
    crate::arch::halt_forever()
}

fn print_decimal(n: u32) {
    if n == 0 {
        crate::arch::debugcon_write_byte(b'0');
        return;
    }
    let mut buf = [0u8; 10];
    let mut i = buf.len();
    let mut n = n;
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    for &b in &buf[i..] {
        crate::arch::debugcon_write_byte(b);
    }
}
