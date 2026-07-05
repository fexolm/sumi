#![no_std]
#![no_main]

include!("../common.rs");

const F_GETFD: u64 = 1;
const F_GETFL: u64 = 3;
const F_GETLK: u64 = 5;
const F_SETLK: u64 = 6;
const F_DUPFD: u64 = 0;
const F_UNLCK: i16 = 2;

#[repr(C)]
struct Flock {
    l_type: i16,
    l_whence: i16,
    l_start: i64,
    l_len: i64,
    l_pid: i32,
}

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let path = b"/tmp/sumi_int_fcntl.txt\0";
    let fd = sys_open(path, O_RDWR | O_CREAT | O_TRUNC, 0o644);
    check!(fd >= 0);

    // F_GETFD and F_GETFL must succeed.
    check!(sys_fcntl(fd, F_GETFD, 0) >= 0);
    check!(sys_fcntl(fd, F_GETFL, 0) >= 0);

    let mut lock = Flock {
        l_type: 1,
        l_whence: SEEK_SET as i16,
        l_start: 0,
        l_len: 0,
        l_pid: 0,
    };
    check_eq!(sys_fcntl(fd, F_SETLK, &lock as *const Flock as u64), 0);
    check_eq!(sys_fcntl(fd, F_GETLK, &mut lock as *mut Flock as u64), 0);
    check_eq!(lock.l_type, F_UNLCK);

    // F_DUPFD returns a new fd referring to the same file.
    let new_fd = sys_fcntl(fd, F_DUPFD, 0);
    check!(new_fd >= 0);
    check!(new_fd != fd);

    check_eq!(sys_close(new_fd), 0);
    check_eq!(sys_close(fd), 0);
    pass!();
}
