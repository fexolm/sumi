use crate::selftest::{fs, syscall, syscall6};
use sumi_abi::fuse::FUSE_ROOT_ID;

pub(super) fn test_write_pread() -> bool {
    let fs = fs();
    let flags: u32 = 2 | 0o100 | 0o1000;
    let (entry, open) = match fs.create(FUSE_ROOT_ID, b"syscall_pread.txt", flags, 0o644) {
        Ok(v) => v,
        Err(_) => return false,
    };
    fs.release(open.fh);
    fs.forget(entry.nodeid, 1);

    // open(path, O_RDWR)
    let path = b"/syscall_pread.txt\0";
    let fd = syscall(2, path.as_ptr() as u64, 2, 0);
    if fd < 0 {
        return false;
    }

    // write(fd, "ABCDEF", 6)
    let data = [b'A', b'B', b'C', b'D', b'E', b'F'];
    let w = syscall(1, fd as u64, data.as_ptr() as u64, 6);
    if w != 6 {
        syscall(3, fd as u64, 0, 0);
        return false;
    }

    // pread64(fd, buf, 3, 2) -> should read "CDE"
    let buf = [0u8; 3];
    let n = syscall6(17, fd as u64, buf.as_ptr() as u64, 3, 2, 0, 0);
    syscall(3, fd as u64, 0, 0);

    n == 3 && &buf == b"CDE"
}
