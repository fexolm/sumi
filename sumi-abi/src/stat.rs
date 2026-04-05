//! Linux x86_64 stat and directory entry structures.
//!
//! These are kernel ABI types returned by stat/fstat/getdents64 syscalls.
//! Layout must match the Linux kernel's x86_64 definitions exactly.

/// `struct stat` for x86_64 Linux (144 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Stat {
    pub st_dev: u64,
    pub st_ino: u64,
    pub st_nlink: u64,
    pub st_mode: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    pub __pad0: u32,
    pub st_rdev: u64,
    pub st_size: i64,
    pub st_blksize: i64,
    pub st_blocks: i64,
    pub st_atime: u64,
    pub st_atime_nsec: u64,
    pub st_mtime: u64,
    pub st_mtime_nsec: u64,
    pub st_ctime: u64,
    pub st_ctime_nsec: u64,
    pub __unused: [i64; 3],
}

/// Fixed-field size of `struct linux_dirent64` (before d_name):
/// d_ino (8) + d_off (8) + d_reclen (2) + d_type (1) = 19 bytes.
/// The struct is NOT representable with `#[repr(C)]` because C padding
/// would add 5 bytes after d_type, so we write fields at raw offsets.
pub const LINUX_DIRENT64_HEADER_SIZE: usize = 19;

/// Write a linux_dirent64 header at the given pointer.
/// Caller must ensure `dest` has at least `LINUX_DIRENT64_HEADER_SIZE` bytes available.
///
/// # Safety
/// `dest` must be valid for writes of `LINUX_DIRENT64_HEADER_SIZE` bytes.
pub unsafe fn write_linux_dirent64_header(
    dest: *mut u8,
    d_ino: u64,
    d_off: i64,
    d_reclen: u16,
    d_type: u8,
) {
    // SAFETY: Caller guarantees dest is valid for 19 bytes of writes.
    unsafe {
        core::ptr::write_unaligned(dest.cast::<u64>(), d_ino);
        core::ptr::write_unaligned(dest.add(8).cast::<i64>(), d_off);
        core::ptr::write_unaligned(dest.add(16).cast::<u16>(), d_reclen);
        core::ptr::write(dest.add(18), d_type);
    }
}

// File type constants for d_type
pub const DT_UNKNOWN: u8 = 0;
pub const DT_FIFO: u8 = 1;
pub const DT_CHR: u8 = 2;
pub const DT_DIR: u8 = 4;
pub const DT_BLK: u8 = 6;
pub const DT_REG: u8 = 8;
pub const DT_LNK: u8 = 10;
pub const DT_SOCK: u8 = 12;

/// AT_FDCWD: use current working directory for *at() syscalls.
pub const AT_FDCWD: i32 = -100;

/// S_IFMT mask and file type bits from mode.
pub const S_IFMT: u32 = 0o170000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFLNK: u32 = 0o120000;

/// Convert stat mode to dirent d_type.
pub fn mode_to_dtype(mode: u32) -> u8 {
    match mode & S_IFMT {
        S_IFREG => DT_REG,
        S_IFDIR => DT_DIR,
        S_IFLNK => DT_LNK,
        _ => DT_UNKNOWN,
    }
}
