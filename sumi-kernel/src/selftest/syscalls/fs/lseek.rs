use crate::fs::virtio_fs::VirtioFsClient;
use crate::selftest::{syscall, syscall6};
use sumi_abi::fuse::FUSE_ROOT_ID;

pub(super) fn test_lseek() -> bool {
    let fs = crate::fs();
    let flags: u32 = 2 | 0o100 | 0o1000;
    let (entry, open) = match fs.create(FUSE_ROOT_ID, b"syscall_lseek.txt", flags, 0o644) {
        Ok(v) => v,
        Err(_) => return false,
    };
    // Write 10 bytes
    let data = [b'0'; 10];
    let data_phys = VirtioFsClient::v2p(data.as_ptr());
    let _ = fs.write(open.fh, 0, data_phys, 10);
    fs.release(open.fh);
    fs.forget(entry.nodeid, 1);

    let path = b"/syscall_lseek.txt\0";
    let fd = syscall(2, path.as_ptr() as u64, 0, 0);
    if fd < 0 {
        return false;
    }

    // SEEK_SET to 5
    let pos = syscall(8, fd as u64, 5, 0);
    if pos != 5 {
        syscall(3, fd as u64, 0, 0);
        return false;
    }

    // SEEK_CUR +2
    let pos = syscall(8, fd as u64, 2, 1);
    if pos != 7 {
        syscall(3, fd as u64, 0, 0);
        return false;
    }

    // SEEK_END -3
    let pos = syscall6(8, fd as u64, (-3i64) as u64, 2, 0, 0, 0);
    syscall(3, fd as u64, 0, 0);
    pos == 7 // 10 - 3 = 7
}
