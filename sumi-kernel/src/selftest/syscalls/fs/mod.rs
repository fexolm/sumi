use crate::selftest::SelfTest;

mod lseek;
mod open;
mod pread;

pub(crate) const TESTS: [SelfTest; 4] = [
    SelfTest {
        name: "open_read_close",
        func: open::test_open_read_close,
    },
    SelfTest {
        name: "write_pread",
        func: pread::test_write_pread,
    },
    SelfTest {
        name: "lseek",
        func: lseek::test_lseek,
    },
    SelfTest {
        name: "open_enoent",
        func: open::test_open_enoent,
    },
];
