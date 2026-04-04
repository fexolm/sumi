use crate::selftest::syscall;

pub(super) fn test_write_console() -> bool {
    // write(1, "hi", 2) -> should return 2
    let msg = [b'h', b'i'];
    let ret = syscall(1, 1, msg.as_ptr() as u64, 2);
    ret == 2
}
