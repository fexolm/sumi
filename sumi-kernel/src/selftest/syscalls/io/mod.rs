use crate::selftest::SelfTest;

mod close;
mod read;
mod write;

pub(crate) const TESTS: [SelfTest; 3] = [
    SelfTest {
        name: "write_console",
        func: write::test_write_console,
    },
    SelfTest {
        name: "read_console_eof",
        func: read::test_read_console_eof,
    },
    SelfTest {
        name: "close_bad_fd",
        func: close::test_close_bad_fd,
    },
];
