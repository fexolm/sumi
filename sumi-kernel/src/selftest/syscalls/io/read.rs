use crate::selftest::syscall;

pub(super) fn test_read_console_eof() -> bool {
    // read(0, buf, 16) -> should return 0 (EOF, no stdin)
    let mut buf = [0u8; 16];
    let ret = syscall(0, 0, buf.as_mut_ptr() as u64, 16);
    ret == 0
}
