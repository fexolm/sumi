/// Kernel selftests — run inside the actual kernel under KVM.
use crate::arch::debugcon_write_byte;
use crate::fs::virtio_fs::VirtioFsClient;
use sumi_abi::fuse::FUSE_ROOT_ID;

struct SelfTest {
    name: &'static str,
    func: fn() -> bool,
}

/// Write a string to debugcon (port 0xE9), byte by byte.
fn debugcon_puts(s: &str) {
    for &b in s.as_bytes() {
        debugcon_write_byte(b);
    }
}

/// Run all registered selftests. Returns true if all passed.
pub fn run_all() -> bool {
    let tests: &[SelfTest] = &[
        SelfTest {
            name: "virtio_fs_init",
            func: test_virtio_fs_init,
        },
        SelfTest {
            name: "virtio_fs_create_read",
            func: test_virtio_fs_create_read,
        },
        SelfTest {
            name: "virtio_fs_write_read_back",
            func: test_virtio_fs_write_read_back,
        },
        SelfTest {
            name: "virtio_fs_read_print",
            func: test_virtio_fs_read_print,
        },
        SelfTest {
            name: "fd_table_alloc_free",
            func: test_fd_table_alloc_free,
        },
    ];

    let mut all_passed = true;
    for test in tests {
        debugcon_puts("[test] ");
        debugcon_puts(test.name);
        debugcon_puts(" ... ");
        if (test.func)() {
            debugcon_puts("ok\n");
        } else {
            debugcon_puts("FAIL\n");
            all_passed = false;
        }
    }

    if all_passed {
        debugcon_puts("[test] all tests passed\n");
    } else {
        debugcon_puts("[test] SOME TESTS FAILED\n");
    }

    all_passed
}

fn fs() -> Option<&'static VirtioFsClient> {
    crate::VIRTIO_FS.get()
}

// ── Virtio-fs tests ──────────────────────────────────────────────

fn test_virtio_fs_init() -> bool {
    fs().is_some()
}

/// Create a file via FUSE_CREATE, write to it, read back and verify.
fn test_virtio_fs_create_read() -> bool {
    let fs = match fs() {
        Some(fs) => fs,
        None => return false,
    };

    // Create "selftest_create.txt" with O_RDWR | O_CREAT | O_TRUNC
    let flags: u32 = 2 | 0o100 | 0o1000;
    let (entry, open) = match fs.create(FUSE_ROOT_ID, b"selftest_create.txt", flags, 0o644) {
        Ok(v) => v,
        Err(_) => return false,
    };

    // Write content — stack buffer, not rodata
    let data = *b"create works!\n";
    let data_phys = VirtioFsClient::v2p(data.as_ptr());
    let written = match fs.write(open.fh, 0, data_phys, data.len() as u32) {
        Ok(n) => n,
        Err(_) => {
            fs.release(open.fh);
            fs.forget(entry.nodeid, 1);
            return false;
        }
    };

    // Read back
    let mut buf = [0u8; 64];
    let buf_phys = VirtioFsClient::v2p(buf.as_ptr());
    let read = match fs.read(open.fh, 0, buf_phys, buf.len() as u32) {
        Ok(n) => n,
        Err(_) => {
            fs.release(open.fh);
            fs.forget(entry.nodeid, 1);
            return false;
        }
    };

    fs.release(open.fh);
    fs.forget(entry.nodeid, 1);

    written as usize == data.len() && read as usize == data.len() && buf[..7] == *b"create "
}

/// Create a file, write "PASS", read it back and verify exact contents.
fn test_virtio_fs_write_read_back() -> bool {
    let fs = match fs() {
        Some(fs) => fs,
        None => return false,
    };

    let flags: u32 = 2 | 0o100 | 0o1000; // O_RDWR | O_CREAT | O_TRUNC
    let (entry, open) = match fs.create(FUSE_ROOT_ID, b"selftest_wrb.txt", flags, 0o644) {
        Ok(v) => v,
        Err(_) => return false,
    };

    let data = [b'P', b'A', b'S', b'S'];
    let data_phys = VirtioFsClient::v2p(data.as_ptr());
    let written = match fs.write(open.fh, 0, data_phys, 4) {
        Ok(n) => n,
        Err(_) => {
            fs.release(open.fh);
            fs.forget(entry.nodeid, 1);
            return false;
        }
    };

    let mut buf = [0u8; 4];
    let buf_phys = VirtioFsClient::v2p(buf.as_ptr());
    let read = match fs.read(open.fh, 0, buf_phys, 4) {
        Ok(n) => n,
        Err(_) => {
            fs.release(open.fh);
            fs.forget(entry.nodeid, 1);
            return false;
        }
    };

    fs.release(open.fh);
    fs.forget(entry.nodeid, 1);

    written == 4 && read == 4 && &buf == b"PASS"
}

/// Create a file, write content, read it back and print to debugcon.
fn test_virtio_fs_read_print() -> bool {
    let fs = match fs() {
        Some(fs) => fs,
        None => return false,
    };

    let flags: u32 = 2 | 0o100 | 0o1000;
    let (entry, open) = match fs.create(FUSE_ROOT_ID, b"selftest_print.txt", flags, 0o644) {
        Ok(v) => v,
        Err(_) => return false,
    };

    let msg = *b"hello from kernel selftest!\n";
    let msg_phys = VirtioFsClient::v2p(msg.as_ptr());
    if fs.write(open.fh, 0, msg_phys, msg.len() as u32).is_err() {
        fs.release(open.fh);
        fs.forget(entry.nodeid, 1);
        return false;
    }

    let mut buf = [0u8; 64];
    let buf_phys = VirtioFsClient::v2p(buf.as_ptr());
    let n = match fs.read(open.fh, 0, buf_phys, buf.len() as u32) {
        Ok(n) => n as usize,
        Err(_) => {
            fs.release(open.fh);
            fs.forget(entry.nodeid, 1);
            return false;
        }
    };

    fs.release(open.fh);
    fs.forget(entry.nodeid, 1);

    debugcon_puts("\n--- selftest_print.txt ---\n");
    for &b in &buf[..n] {
        debugcon_write_byte(b);
    }
    debugcon_puts("--- end ---\n");

    n == msg.len()
}

// ── FD table tests ───────────────────────────────────────────────

fn test_fd_table_alloc_free() -> bool {
    let mut table = crate::FD_TABLE.lock();

    if table.get(0).is_none() || table.get(1).is_none() || table.get(2).is_none() {
        return false;
    }

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
    if fd2 != 3 {
        return false;
    }

    table.free(fd2);
    true
}
