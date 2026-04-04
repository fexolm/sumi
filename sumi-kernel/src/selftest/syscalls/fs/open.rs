use crate::fs::virtio_fs::VirtioFsClient;
use crate::selftest::{fs, syscall};
use sumi_abi::fuse::FUSE_ROOT_ID;

pub(super) fn test_open_read_close() -> bool {
    let fs = fs();
    let flags: u32 = 2 | 0o100 | 0o1000;
    let (entry, open) = match fs.create(FUSE_ROOT_ID, b"syscall_orc.txt", flags, 0o644) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let data = *b"syscall works\n";
    let data_phys = VirtioFsClient::v2p(data.as_ptr());
    let _ = fs.write(open.fh, 0, data_phys, data.len() as u32);
    fs.release(open.fh);
    fs.forget(entry.nodeid, 1);

    let path = *b"/syscall_orc.txt\0\0";
    let fd = syscall(2, path.as_ptr() as u64, 0, 0);
    if fd < 0 {
        return false;
    }

    let buf = [0u8; 64];
    let n = syscall(0, fd as u64, buf.as_ptr() as u64, 64);
    if n != data.len() as i64 {
        syscall(3, fd as u64, 0, 0);
        return false;
    }

    let ret = syscall(3, fd as u64, 0, 0);
    if ret != 0 {
        return false;
    }

    &buf[..7] == b"syscall"
}

pub(super) fn test_open_enoent() -> bool {
    let path = b"/nonexistent_file.txt\0";
    let fd = syscall(2, path.as_ptr() as u64, 0, 0);
    fd == -2 // -ENOENT
}
