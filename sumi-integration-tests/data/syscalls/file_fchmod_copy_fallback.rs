#![no_std]
#![no_main]

include!("../common.rs");

const SYS_SENDFILE: u64 = 40;
const SYS_FCHMOD: u64 = 91;
const SYS_COPY_FILE_RANGE: u64 = 326;

const EBADF: i64 = 9;
const ENOSYS: i64 = 38;

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let path = b"/tmp/sumi_int_fchmod_copy_fallback.txt\0";
    let fd = sys_open(path, O_CREAT | O_RDWR | O_TRUNC, 0o644);
    check!(fd >= 0);

    let fchmod_ok = unsafe { syscall2(SYS_FCHMOD, fd as u64, 0o600) };
    check_eq!(fchmod_ok, 0);

    let fchmod_bad = unsafe { syscall2(SYS_FCHMOD, u64::MAX, 0o600) };
    check_eq!(fchmod_bad, -EBADF);

    let mut offset: i64 = 0;
    let sendfile = unsafe {
        syscall4(
            SYS_SENDFILE,
            fd as u64,
            fd as u64,
            (&mut offset as *mut i64) as u64,
            1,
        )
    };
    check_eq!(sendfile, -ENOSYS);

    let mut off_in: i64 = 0;
    let mut off_out: i64 = 0;
    let copy_file_range = unsafe {
        syscall6(
            SYS_COPY_FILE_RANGE,
            fd as u64,
            (&mut off_in as *mut i64) as u64,
            fd as u64,
            (&mut off_out as *mut i64) as u64,
            1,
            0,
        )
    };
    check_eq!(copy_file_range, -ENOSYS);

    check_eq!(sys_close(fd), 0);
    pass!();
}
