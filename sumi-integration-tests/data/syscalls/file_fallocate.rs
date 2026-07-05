#![no_std]
#![no_main]

include!("../common.rs");

const FALLOC_FL_KEEP_SIZE: u64 = 0x01;
const FALLOC_FL_PUNCH_HOLE: u64 = 0x02;

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let path = b"/tmp/sumi_int_fallocate.bin\0";
    let _ = sys_unlink(path);

    let fd = sys_open(path, O_RDWR | O_CREAT | O_TRUNC, 0o644);
    check!(fd >= 0);

    check_eq!(sys_fallocate(fd, 0, 0, 8192), 0);

    let mut st = Stat {
        st_dev: 0,
        st_ino: 0,
        st_nlink: 0,
        st_mode: 0,
        st_uid: 0,
        st_gid: 0,
        __pad0: 0,
        st_rdev: 0,
        st_size: 0,
        st_blksize: 0,
        st_blocks: 0,
        st_atime: 0,
        st_atime_nsec: 0,
        st_mtime: 0,
        st_mtime_nsec: 0,
        st_ctime: 0,
        st_ctime_nsec: 0,
        __unused: [0; 3],
    };
    check_eq!(sys_stat(path, &mut st as *mut _ as *mut u8), 0);
    check!(st.st_size >= 8192);

    let mut buf = [0x55u8; 16];
    check_eq!(sys_pread64(fd, &mut buf, 4096), buf.len() as i64);
    for b in buf {
        check_eq!(b, 0);
    }

    check_eq!(
        sys_fallocate(fd, FALLOC_FL_KEEP_SIZE | FALLOC_FL_PUNCH_HOLE, 0, 4096),
        0
    );
    check_eq!(sys_fallocate(fd, FALLOC_FL_KEEP_SIZE, 0, 4096), -95);
    check_eq!(sys_fallocate(fd, 0, 0, 0), -22);

    check_eq!(sys_close(fd), 0);
    check_eq!(sys_unlink(path), 0);
    pass!();
}
