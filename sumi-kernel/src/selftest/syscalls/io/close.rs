use crate::selftest::syscall;

pub(super) fn test_close_bad_fd() -> bool {
    // close(999) -> should return -EBADF (-9)
    let ret = syscall(3, 999, 0, 0);
    ret == -9
}
