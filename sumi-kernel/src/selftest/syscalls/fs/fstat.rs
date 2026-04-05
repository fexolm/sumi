use crate::fs::virtio_fs::VirtioFsClient;
use crate::selftest::{fs, syscall};
use sumi_abi::fuse::FUSE_ROOT_ID;
use sumi_abi::stat::Stat;

pub(super) fn test_fstat_file_size() -> bool {
    let fs = fs();
    let flags: u32 = 2 | 0o100 | 0o1000; // O_RDWR | O_CREAT | O_TRUNC
    let (entry, open) = match fs.create(FUSE_ROOT_ID, b"selftest_fstat.txt", flags, 0o644) {
        Ok(v) => v,
        Err(_) => return false,
    };

    // Write exactly 42 bytes
    let data = [b'A'; 42];
    let data_phys = VirtioFsClient::v2p(data.as_ptr());
    let _ = fs.write(open.fh, 0, data_phys, 42);
    fs.release(open.fh);
    fs.forget(entry.nodeid, 1);

    // Open via syscall, then fstat
    let path = b"/selftest_fstat.txt\0";
    let fd = syscall(2, path.as_ptr() as u64, 0, 0);
    if fd < 0 {
        return false;
    }

    let stat_buf = core::mem::MaybeUninit::<Stat>::uninit();
    let stat_ptr = stat_buf.as_ptr() as u64;
    let ret = syscall(5, fd as u64, stat_ptr, 0); // fstat
    syscall(3, fd as u64, 0, 0); // close

    if ret != 0 {
        return false;
    }

    // SAFETY: fstat wrote into stat_buf.
    let stat = unsafe { stat_buf.assume_init() };
    stat.st_size == 42
}
