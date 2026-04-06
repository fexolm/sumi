use crate::fs::virtio_fs::VirtioFsClient;
use crate::selftest::syscall6;
use sumi_abi::fuse::FUSE_ROOT_ID;
use sumi_abi::stat::AT_FDCWD;

pub(super) fn test_openat_fdcwd() -> bool {
    let fs = crate::fs();
    let flags: u32 = 2 | 0o100 | 0o1000;
    let (entry, open) = match fs.create(FUSE_ROOT_ID, b"selftest_openat.txt", flags, 0o644) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let data = *b"openat works";
    let data_phys = VirtioFsClient::v2p(data.as_ptr());
    let _ = fs.write(open.fh, 0, data_phys, data.len() as u32);
    fs.release(open.fh);
    fs.forget(entry.nodeid, 1);

    // openat(AT_FDCWD, "/selftest_openat.txt", O_RDONLY, 0)
    let path = b"/selftest_openat.txt\0";
    let fd = syscall6(
        257,
        AT_FDCWD as u32 as u64, // sign-extend -100 properly
        path.as_ptr() as u64,
        0, // O_RDONLY
        0,
        0,
        0,
    );
    if fd < 0 {
        return false;
    }

    let buf = [0u8; 32];
    let n = syscall6(0, fd as u64, buf.as_ptr() as u64, 32, 0, 0, 0);
    syscall6(3, fd as u64, 0, 0, 0, 0, 0);

    n == data.len() as i64 && &buf[..12] == b"openat works"
}
