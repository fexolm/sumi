use crate::selftest::SelfTest;

mod dup;
mod fstat;
mod lseek;
mod open;
mod openat;
mod pread;
mod stat;

pub(crate) const TESTS: [SelfTest; 9] = [
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
    SelfTest {
        name: "fstat_file_size",
        func: fstat::test_fstat_file_size,
    },
    SelfTest {
        name: "stat_file",
        func: stat::test_stat_file,
    },
    SelfTest {
        name: "stat_enoent",
        func: stat::test_stat_enoent,
    },
    SelfTest {
        name: "openat_fdcwd",
        func: openat::test_openat_fdcwd,
    },
    SelfTest {
        name: "dup2_read",
        func: dup::test_dup2_read,
    },
];
