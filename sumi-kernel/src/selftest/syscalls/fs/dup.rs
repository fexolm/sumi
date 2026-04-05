use crate::fs::virtio_fs::VirtioFsClient;
use crate::selftest::{fs, syscall};
use sumi_abi::fuse::FUSE_ROOT_ID;

pub(super) fn test_dup2_read() -> bool {
    let fs = fs();
    let flags: u32 = 2 | 0o100 | 0o1000;
    let (entry, open) = match fs.create(FUSE_ROOT_ID, b"selftest_dup.txt", flags, 0o644) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let data = *b"dup works";
    let data_phys = VirtioFsClient::v2p(data.as_ptr());
    let _ = fs.write(open.fh, 0, data_phys, data.len() as u32);
    fs.release(open.fh);
    fs.forget(entry.nodeid, 1);

    let path = b"/selftest_dup.txt\0";
    let fd = syscall(2, path.as_ptr() as u64, 0, 0);
    if fd < 0 {
        return false;
    }

    // dup2(fd, 100)
    let new_fd = syscall(33, fd as u64, 100, 0);
    if new_fd != 100 {
        syscall(3, fd as u64, 0, 0);
        return false;
    }

    // Read from the duped fd
    let buf = [0u8; 32];
    let n = syscall(0, 100, buf.as_ptr() as u64, 32);
    syscall(3, fd as u64, 0, 0);
    syscall(3, 100, 0, 0);

    n == data.len() as i64 && &buf[..9] == b"dup works"
}
