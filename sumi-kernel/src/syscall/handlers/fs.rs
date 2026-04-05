use crate::fs::{FdKind, FileDescriptor};
use crate::syscall::errno::*;
use crate::syscall::{SyscallArgs, SyscallResult};
use sumi_abi::fuse::{FuseAttr, FuseDirent, fuse_dirent_align};
use sumi_abi::stat::{
    AT_FDCWD, DT_DIR, DT_REG, DT_UNKNOWN, LINUX_DIRENT64_HEADER_SIZE, Stat,
    write_linux_dirent64_header,
};

/// Read a null-terminated path from user memory. Returns slice excluding the null.
fn read_user_path(ptr: u64) -> Result<&'static [u8], SyscallResult> {
    let path_ptr = ptr as *const u8;
    // SAFETY: Unikernel — single address space, user and kernel share memory.
    // The pointer comes from the syscall caller and the memory will not be
    // unmapped or reclaimed during this call ('static is valid for the process lifetime).
    unsafe {
        let mut len = 0;
        while core::ptr::read_volatile(path_ptr.add(len)) != 0 {
            len += 1;
            if len > 4095 {
                return Err(-36); // ENAMETOOLONG
            }
        }
        Ok(core::slice::from_raw_parts(path_ptr, len))
    }
}

/// Convert a FUSE attr to Linux stat struct and write it to user memory.
fn fuse_attr_to_stat(attr: &FuseAttr) -> Stat {
    Stat {
        st_dev: 0,
        st_ino: attr.ino,
        st_nlink: attr.nlink as u64,
        st_mode: attr.mode,
        st_uid: attr.uid,
        st_gid: attr.gid,
        __pad0: 0,
        st_rdev: attr.rdev as u64,
        st_size: attr.size as i64,
        st_blksize: attr.blksize as i64,
        st_blocks: attr.blocks as i64,
        st_atime: attr.atime,
        st_atime_nsec: attr.atimensec as u64,
        st_mtime: attr.mtime,
        st_mtime_nsec: attr.mtimensec as u64,
        st_ctime: attr.ctime,
        st_ctime_nsec: attr.ctimensec as u64,
        __unused: [0; 3],
    }
}

/// Write a Stat struct to user memory at the given address.
fn write_stat_to_user(stat: &Stat, buf_addr: u64) {
    // SAFETY: Writing a repr(C) struct to user-provided address.
    // Caller is responsible for ensuring the address is valid.
    unsafe {
        core::ptr::write_volatile(buf_addr as *mut Stat, *stat);
    }
}

/// Forget a nodeid, but never the root (nodeid 1 is permanent).
fn forget_if_not_root(fs: &crate::fs::virtio_fs::VirtioFsClient, nodeid: u64) {
    use sumi_abi::fuse::FUSE_ROOT_ID;
    if nodeid != FUSE_ROOT_ID {
        fs.forget(nodeid, 1);
    }
}

/// Internal stat-by-path implementation shared by stat, lstat, newfstatat.
fn do_stat_path(path: &[u8], buf_addr: u64) -> SyscallResult {
    let fs = match crate::VIRTIO_FS.get() {
        Some(fs) => fs,
        None => return EIO,
    };

    let nodeid = match fs.resolve_path(path) {
        Ok(id) => id,
        Err(e) => return e as SyscallResult,
    };

    let attr_out = match fs.getattr(nodeid) {
        Ok(a) => a,
        Err(e) => {
            forget_if_not_root(fs, nodeid);
            return e as SyscallResult;
        }
    };

    forget_if_not_root(fs, nodeid);

    let stat = fuse_attr_to_stat(&attr_out.attr);
    write_stat_to_user(&stat, buf_addr);
    0
}

pub fn sys_stat(args: &SyscallArgs) -> SyscallResult {
    let path = match read_user_path(args.arg0) {
        Ok(p) => p,
        Err(e) => return e,
    };
    do_stat_path(path, args.arg1)
}

pub fn sys_fstat(args: &SyscallArgs) -> SyscallResult {
    let fd_num = args.arg0 as usize;
    let buf_addr = args.arg1;

    let nodeid = {
        let table = crate::FD_TABLE.lock();
        match table.get(fd_num) {
            Some(d) => match d.kind {
                FdKind::File { fuse_nodeid, .. } | FdKind::Directory { fuse_nodeid, .. } => {
                    fuse_nodeid
                }
                FdKind::Console => {
                    // Console fds: return a minimal stat with char device mode
                    let stat = Stat {
                        st_dev: 0,
                        st_ino: 0,
                        st_nlink: 1,
                        st_mode: 0o20666, // S_IFCHR | 0666
                        st_uid: 0,
                        st_gid: 0,
                        __pad0: 0,
                        st_rdev: 0,
                        st_size: 0,
                        st_blksize: 4096,
                        st_blocks: 0,
                        st_atime: 0,
                        st_atime_nsec: 0,
                        st_mtime: 0,
                        st_mtime_nsec: 0,
                        st_ctime: 0,
                        st_ctime_nsec: 0,
                        __unused: [0; 3],
                    };
                    write_stat_to_user(&stat, buf_addr);
                    return 0;
                }
            },
            None => return EBADF,
        }
    };

    let fs = match crate::VIRTIO_FS.get() {
        Some(fs) => fs,
        None => return EIO,
    };

    let attr_out = match fs.getattr(nodeid) {
        Ok(a) => a,
        Err(e) => return e as SyscallResult,
    };

    let stat = fuse_attr_to_stat(&attr_out.attr);
    write_stat_to_user(&stat, buf_addr);
    0
}

pub fn sys_lstat(args: &SyscallArgs) -> SyscallResult {
    // No symlink distinction in our FUSE implementation — same as stat.
    sys_stat(args)
}

pub fn sys_access(args: &SyscallArgs) -> SyscallResult {
    let path = match read_user_path(args.arg0) {
        Ok(p) => p,
        Err(e) => return e,
    };

    let fs = match crate::VIRTIO_FS.get() {
        Some(fs) => fs,
        None => return EIO,
    };

    // Just check existence by resolving the path.
    let nodeid = match fs.resolve_path(path) {
        Ok(id) => id,
        Err(e) => return e as SyscallResult,
    };

    forget_if_not_root(fs, nodeid);
    0
}

pub fn sys_getdents(_args: &SyscallArgs) -> SyscallResult {
    // Old 32-bit getdents uses a different struct layout (32-bit d_ino).
    // Modern programs use getdents64 (syscall 217).
    ENOSYS
}

pub fn sys_getdents64(args: &SyscallArgs) -> SyscallResult {
    let fd_num = args.arg0 as usize;
    let buf_addr = args.arg1;
    let buf_size = args.arg2 as u32;

    let (fuse_fh, fuse_nodeid, dir_offset) = {
        let table = crate::FD_TABLE.lock();
        match table.get(fd_num) {
            Some(d) => match d.kind {
                FdKind::Directory {
                    fuse_fh,
                    fuse_nodeid,
                    offset,
                } => (fuse_fh, fuse_nodeid, offset),
                _ => return ENOTDIR,
            },
            None => return EBADF,
        }
    };

    let fs = match crate::VIRTIO_FS.get() {
        Some(fs) => fs,
        None => return EIO,
    };

    // Use a kernel-side buffer for the FUSE readdir response.
    // We read FUSE dirents and convert them to linux_dirent64 format.
    let fuse_buf = [0u8; 4096];
    let fuse_buf_phys = crate::fs::virtio_fs::VirtioFsClient::v2p(fuse_buf.as_ptr());

    let fuse_bytes = match fs.readdir(fuse_nodeid, fuse_fh, dir_offset, fuse_buf_phys, fuse_buf.len() as u32) {
        Ok(n) => n as usize,
        Err(e) => return e as SyscallResult,
    };

    if fuse_bytes == 0 {
        return 0; // End of directory
    }

    // Parse FUSE dirents and write linux_dirent64 entries to user buffer
    let mut fuse_pos = 0usize;
    let mut user_pos = 0usize;
    let fuse_dirent_hdr_size = core::mem::size_of::<FuseDirent>();

    while fuse_pos + fuse_dirent_hdr_size <= fuse_bytes {
        // SAFETY: Reading a repr(C) FuseDirent from the FUSE response buffer.
        let dirent = unsafe {
            core::ptr::read_volatile(fuse_buf.as_ptr().add(fuse_pos) as *const FuseDirent)
        };

        let namelen = dirent.namelen as usize;
        let fuse_entry_size = fuse_dirent_align(fuse_dirent_hdr_size + namelen);

        if fuse_pos + fuse_dirent_hdr_size + namelen > fuse_bytes {
            break;
        }

        // Calculate linux_dirent64 record size (header + name + null, aligned to 8)
        let reclen = (LINUX_DIRENT64_HEADER_SIZE + namelen + 1 + 7) & !7;

        if reclen > u16::MAX as usize || user_pos + reclen > buf_size as usize {
            break;
        }

        // Determine file type from FUSE dirent type field
        let d_type = match dirent.typ {
            4 => DT_DIR,  // DT_DIR
            8 => DT_REG,  // DT_REG
            _ => DT_UNKNOWN,
        };

        let dest = (buf_addr + user_pos as u64) as *mut u8;
        // SAFETY: Writing linux_dirent64 fields and name to user buffer.
        // The buffer is checked to have `reclen` bytes available above.
        unsafe {
            write_linux_dirent64_header(
                dest,
                dirent.ino,
                dirent.off as i64,
                reclen as u16,
                d_type,
            );
            // Copy name after the 19-byte header
            let name_src = fuse_buf.as_ptr().add(fuse_pos + fuse_dirent_hdr_size);
            core::ptr::copy_nonoverlapping(
                name_src,
                dest.add(LINUX_DIRENT64_HEADER_SIZE),
                namelen,
            );
            // Null terminator + zero padding to alignment
            for i in (LINUX_DIRENT64_HEADER_SIZE + namelen)..reclen {
                core::ptr::write(dest.add(i), 0u8);
            }
        }

        user_pos += reclen;
        fuse_pos += fuse_entry_size;
    }

    // Update directory fd offset
    if user_pos > 0 && fuse_pos > 0 {
        // Walk back to find the last dirent's offset
        let mut last_off = 0u64;
        let mut scan = 0usize;
        while scan + fuse_dirent_hdr_size <= fuse_pos.min(fuse_bytes) {
            let d = unsafe {
                core::ptr::read_volatile(fuse_buf.as_ptr().add(scan) as *const FuseDirent)
            };
            last_off = d.off;
            let entry_size = fuse_dirent_align(fuse_dirent_hdr_size + d.namelen as usize);
            scan += entry_size;
        }

        let mut table = crate::FD_TABLE.lock();
        if let Some(desc) = table.get_mut(fd_num) {
            if let FdKind::Directory {
                ref mut offset, ..
            } = desc.kind
            {
                *offset = last_off;
            }
        }
    }

    // If FUSE returned data but nothing fit in the user buffer, the buffer is too small.
    if user_pos == 0 && fuse_bytes > 0 {
        return EINVAL;
    }

    user_pos as SyscallResult
}

pub fn sys_getcwd(args: &SyscallArgs) -> SyscallResult {
    let buf_addr = args.arg0;
    let size = args.arg1 as usize;

    if size < 2 {
        return EINVAL; // Need room for "/" + null
    }

    // Unikernel: cwd is always "/"
    unsafe {
        let buf = buf_addr as *mut u8;
        core::ptr::write_volatile(buf, b'/');
        core::ptr::write_volatile(buf.add(1), 0);
    }
    buf_addr as SyscallResult
}

pub fn sys_chdir(args: &SyscallArgs) -> SyscallResult {
    // Unikernel cwd is always root, but validate the path exists.
    let path = match read_user_path(args.arg0) {
        Ok(p) => p,
        Err(e) => return e,
    };

    let fs = match crate::VIRTIO_FS.get() {
        Some(fs) => fs,
        None => return EIO,
    };

    let nodeid = match fs.resolve_path(path) {
        Ok(id) => id,
        Err(e) => return e as SyscallResult,
    };

    forget_if_not_root(fs, nodeid);
    0
}

pub fn sys_fchdir(_args: &SyscallArgs) -> SyscallResult {
    0
}

pub fn sys_rename(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_mkdir(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_rmdir(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_creat(args: &SyscallArgs) -> SyscallResult {
    // creat(path, mode) is equivalent to open(path, O_CREAT|O_WRONLY|O_TRUNC, mode)
    let path = match read_user_path(args.arg0) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let mode = args.arg1 as u32;

    let fs = match crate::VIRTIO_FS.get() {
        Some(fs) => fs,
        None => return EIO,
    };

    // Resolve parent directory and file name
    let (parent_path, filename) = match split_path(path) {
        Some(v) => v,
        None => return EINVAL,
    };

    let parent_nodeid = match fs.resolve_path(parent_path) {
        Ok(id) => id,
        Err(e) => return e as SyscallResult,
    };

    let flags: u32 = 1 | 0o100 | 0o1000; // O_WRONLY | O_CREAT | O_TRUNC
    let (entry, open) = match fs.create(parent_nodeid, filename, flags, mode) {
        Ok(v) => v,
        Err(e) => {
            forget_if_not_root(fs, parent_nodeid);
            return e as SyscallResult;
        }
    };
    forget_if_not_root(fs, parent_nodeid);

    let desc = FileDescriptor {
        kind: FdKind::File {
            fuse_fh: open.fh,
            fuse_nodeid: entry.nodeid,
            offset: 0,
        },
        flags,
    };

    let mut table = crate::FD_TABLE.lock();
    match table.alloc(desc) {
        Some(fd) => fd as SyscallResult,
        None => {
            fs.release(open.fh);
            forget_if_not_root(fs, entry.nodeid);
            EMFILE
        }
    }
}

pub fn sys_link(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_unlink(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_symlink(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_readlink(_args: &SyscallArgs) -> SyscallResult {
    EINVAL // No symlinks in our fs
}

pub fn sys_openat(args: &SyscallArgs) -> SyscallResult {
    let dirfd = args.arg0 as i32;
    let path = match read_user_path(args.arg1) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let flags = args.arg2 as u32;
    let _mode = args.arg3 as u32;

    // If path is absolute or dirfd is AT_FDCWD, use normal open logic.
    // We don't support relative-to-fd paths yet.
    if dirfd != AT_FDCWD && !(path.first() == Some(&b'/')) {
        return ENOSYS;
    }

    let fs = match crate::VIRTIO_FS.get() {
        Some(fs) => fs,
        None => return EIO,
    };

    // Check if this is a directory open (O_DIRECTORY = 0o200000)
    let is_dir_open = flags & 0o200000 != 0;

    let nodeid = match fs.resolve_path(path) {
        Ok(id) => id,
        Err(e) => return e as SyscallResult,
    };

    if is_dir_open {
        let open_out = match fs.opendir(nodeid) {
            Ok(o) => o,
            Err(e) => {
                forget_if_not_root(fs, nodeid);
                return e as SyscallResult;
            }
        };

        let desc = FileDescriptor {
            kind: FdKind::Directory {
                fuse_fh: open_out.fh,
                fuse_nodeid: nodeid,
                offset: 0,
            },
            flags,
        };

        let mut table = crate::FD_TABLE.lock();
        match table.alloc(desc) {
            Some(fd) => fd as SyscallResult,
            None => {
                fs.releasedir(open_out.fh);
                forget_if_not_root(fs, nodeid);
                EMFILE
            }
        }
    } else {
        let open_out = match fs.open(nodeid, flags) {
            Ok(o) => o,
            Err(e) => {
                forget_if_not_root(fs, nodeid);
                return e as SyscallResult;
            }
        };

        let desc = FileDescriptor {
            kind: FdKind::File {
                fuse_fh: open_out.fh,
                fuse_nodeid: nodeid,
                offset: 0,
            },
            flags,
        };

        let mut table = crate::FD_TABLE.lock();
        match table.alloc(desc) {
            Some(fd) => fd as SyscallResult,
            None => {
                fs.release(open_out.fh);
                forget_if_not_root(fs, nodeid);
                EMFILE
            }
        }
    }
}

pub fn sys_newfstatat(args: &SyscallArgs) -> SyscallResult {
    let dirfd = args.arg0 as i32;
    let path = match read_user_path(args.arg1) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let buf_addr = args.arg2;
    let flags = args.arg3 as u32;

    // If path is empty and AT_EMPTY_PATH (0x1000) is set, use fstat on dirfd
    if path.is_empty() && flags & 0x1000 != 0 {
        let fstat_args = SyscallArgs {
            nr: 5,
            arg0: dirfd as u64,
            arg1: buf_addr,
            arg2: 0,
            arg3: 0,
            arg4: 0,
            arg5: 0,
        };
        return sys_fstat(&fstat_args);
    }

    if dirfd != AT_FDCWD && !(path.first() == Some(&b'/')) {
        return ENOSYS;
    }

    do_stat_path(path, buf_addr)
}

pub fn sys_unlinkat(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

/// Split a path into (parent, filename). E.g. "/foo/bar" -> ("/foo", "bar").
/// For "/bar" -> ("", "bar"). Returns None for empty paths or empty filenames.
fn split_path(path: &[u8]) -> Option<(&[u8], &[u8])> {
    // Trim trailing slashes
    let path = match path.iter().rposition(|&b| b != b'/') {
        Some(last) => &path[..=last],
        None => return None, // all slashes or empty
    };
    if path.is_empty() {
        return None;
    }
    let (parent, name) = match path.iter().rposition(|&b| b == b'/') {
        Some(pos) => (&path[..pos], &path[pos + 1..]),
        None => (b"" as &[u8], path),
    };
    if name.is_empty() {
        return None;
    }
    Some((parent, name))
}
