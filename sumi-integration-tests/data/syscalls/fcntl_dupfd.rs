#![no_std]
#![no_main]

include!("../common.rs");

const F_GETFD: u64 = 1;
const F_GETFL: u64 = 3;
const F_DUPFD: u64 = 0;

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let path = b"/tmp/sumi_int_fcntl.txt\0";
    let fd = sys_open(path, O_RDWR | O_CREAT | O_TRUNC, 0o644);
    check!(fd >= 0);

    // F_GETFD and F_GETFL must succeed.
    check!(sys_fcntl(fd, F_GETFD, 0) >= 0);
    check!(sys_fcntl(fd, F_GETFL, 0) >= 0);

    // F_DUPFD returns a new fd referring to the same file.
    let new_fd = sys_fcntl(fd, F_DUPFD, 0);
    check!(new_fd >= 0);
    check!(new_fd != fd);

    check_eq!(sys_close(new_fd), 0);
    check_eq!(sys_close(fd), 0);
    pass!();
}
