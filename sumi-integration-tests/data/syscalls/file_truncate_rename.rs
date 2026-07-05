#![no_std]
#![no_main]

include!("../common.rs");

const ENOENT: i64 = -2;

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let old_path = b"/tmp/sumi_int_truncate_old.bin\0";
    let new_path = b"/tmp/sumi_int_truncate_new.bin\0";
    let _ = sys_unlink(old_path);
    let _ = sys_unlink(new_path);

    let fd = sys_open(old_path, O_RDWR | O_CREAT | O_TRUNC, 0o644);
    check!(fd >= 0);
    check_eq!(sys_pwrite64(fd, b"abcdef", 0), 6);
    check_eq!(sys_ftruncate(fd, 2), 0);

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
    check_eq!(sys_stat(old_path, &mut st as *mut _ as *mut u8), 0);
    check_eq!(st.st_size, 2);

    check_eq!(sys_ftruncate(fd, 6), 0);
    let mut zeros = [0x55u8; 4];
    check_eq!(sys_pread64(fd, &mut zeros, 2), 4);
    for b in zeros {
        check_eq!(b, 0);
    }
    check_eq!(sys_close(fd), 0);

    check_eq!(sys_truncate(old_path, 1), 0);
    check_eq!(sys_stat(old_path, &mut st as *mut _ as *mut u8), 0);
    check_eq!(st.st_size, 1);

    check_eq!(sys_rename(old_path, new_path), 0);
    check_eq!(sys_access(old_path, 0), ENOENT);
    check_eq!(sys_access(new_path, 0), 0);
    check_eq!(sys_unlink(new_path), 0);

    pass!();
}
