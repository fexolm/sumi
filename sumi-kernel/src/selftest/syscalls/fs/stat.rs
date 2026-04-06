use crate::fs::virtio_fs::VirtioFsClient;
use crate::selftest::syscall;
use sumi_abi::fuse::FUSE_ROOT_ID;
use sumi_abi::stat::Stat;

pub(super) fn test_stat_file() -> bool {
    let fs = crate::fs();
    let flags: u32 = 2 | 0o100 | 0o1000;
    let (entry, open) = match fs.create(FUSE_ROOT_ID, b"selftest_stat.txt", flags, 0o644) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let data = [b'X'; 100];
    let data_phys = VirtioFsClient::v2p(data.as_ptr());
    let _ = fs.write(open.fh, 0, data_phys, 100);
    fs.release(open.fh);
    fs.forget(entry.nodeid, 1);

    let path = b"/selftest_stat.txt\0";
    let stat_buf = core::mem::MaybeUninit::<Stat>::uninit();
    let stat_ptr = stat_buf.as_ptr() as u64;

    let ret = syscall(4, path.as_ptr() as u64, stat_ptr, 0); // stat
    if ret != 0 {
        return false;
    }

    let stat = unsafe { stat_buf.assume_init() };
    stat.st_size == 100
}

pub(super) fn test_stat_enoent() -> bool {
    let path = b"/nonexistent_stat_file.txt\0";
    let stat_buf = core::mem::MaybeUninit::<Stat>::uninit();
    let stat_ptr = stat_buf.as_ptr() as u64;
    let ret = syscall(4, path.as_ptr() as u64, stat_ptr, 0);
    ret == -2 // -ENOENT
}
