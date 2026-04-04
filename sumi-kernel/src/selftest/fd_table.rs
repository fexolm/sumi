use super::SelfTest;

pub(crate) const TESTS: [SelfTest; 2] = [
    SelfTest {
        name: "console_fds_preallocated",
        func: test_console_fds,
    },
    SelfTest {
        name: "alloc_free_lowest",
        func: test_fd_alloc_free,
    },
];

fn test_console_fds() -> bool {
    let table = crate::FD_TABLE.lock();
    table.get(0).is_some() && table.get(1).is_some() && table.get(2).is_some()
}

fn test_fd_alloc_free() -> bool {
    let mut table = crate::FD_TABLE.lock();
    let desc = crate::fs::FileDescriptor {
        kind: crate::fs::FdKind::Console,
        flags: 0,
    };

    let fd = match table.alloc(desc) {
        Some(fd) => fd,
        None => return false,
    };
    if fd != 3 {
        return false;
    }
    table.free(fd);

    let fd2 = match table.alloc(desc) {
        Some(fd) => fd,
        None => return false,
    };
    table.free(fd2);
    fd2 == 3
}
