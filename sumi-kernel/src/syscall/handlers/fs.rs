use crate::fs::{FdKind, FileDescriptor};
use crate::syscall::errno::*;
use crate::syscall::{SyscallArgs, SyscallResult};
use sumi_abi::fuse::{FuseAttr, FuseDirent, fuse_dirent_align};
use sumi_abi::stat::{
    AT_FDCWD, DT_DIR, DT_REG, DT_UNKNOWN, LINUX_DIRENT64_HEADER_SIZE, Stat,
    write_linux_dirent64_header,
};

const O_CREAT: u32 = 0o100;
const O_TRUNC: u32 = 0o1000;

/// Read a null-terminated path from user memory. Returns slice excluding the null.
/// Current working directory, absolute, no trailing slash (root = "/").
/// A single global — sumi is a single-process kernel, so every thread
/// shares one cwd, exactly like threads of one Linux process.
static CWD: spin::Mutex<Option<alloc::string::String>> = spin::Mutex::new(None);

fn cwd_bytes() -> alloc::vec::Vec<u8> {
    match &*CWD.lock() {
        Some(s) => s.as_bytes().to_vec(),
        None => alloc::vec![b'/'],
    }
}

fn normalize_abs_path(path: &[u8]) -> alloc::vec::Vec<u8> {
    let mut parts: alloc::vec::Vec<&[u8]> = alloc::vec::Vec::new();
    for part in path.split(|&b| b == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }

    let mut out = alloc::vec::Vec::new();
    out.push(b'/');
    for (idx, part) in parts.iter().enumerate() {
        if idx != 0 {
            out.push(b'/');
        }
        out.extend_from_slice(part);
    }
    out
}

fn read_user_path_inner(ptr: u64, allow_empty: bool) -> Result<alloc::vec::Vec<u8>, SyscallResult> {
    let path_ptr = ptr as *const u8;
    // SAFETY: Unikernel — single address space, user and kernel share memory.
    // The pointer comes from the syscall caller and the memory will not be
    // unmapped or reclaimed during this call.
    let raw = unsafe {
        let mut len = 0;
        while core::ptr::read_volatile(path_ptr.add(len)) != 0 {
            len += 1;
            if len > 4095 {
                return Err(-36); // ENAMETOOLONG
            }
        }
        core::slice::from_raw_parts(path_ptr, len)
    };
    if raw.is_empty() {
        return if allow_empty {
            Ok(alloc::vec::Vec::new())
        } else {
            Err(-2) // ENOENT — Linux rejects the empty path
        };
    }
    let mut abs = if raw[0] == b'/' {
        raw.to_vec()
    } else {
        let mut abs = cwd_bytes();
        if abs.last() != Some(&b'/') {
            abs.push(b'/');
        }
        abs.extend_from_slice(raw);
        abs
    };
    if abs.first() != Some(&b'/') {
        abs.insert(0, b'/');
    }
    Ok(normalize_abs_path(&abs))
}

/// Read a user path and make it absolute against the current working
/// directory, normalizing `.`/`..` before FUSE component lookup.
fn read_user_path(ptr: u64) -> Result<alloc::vec::Vec<u8>, SyscallResult> {
    read_user_path_inner(ptr, false)
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
    if let Err(e) = crate::syscall::handlers::io::flush_all_write_caches() {
        return e as SyscallResult;
    }

    let fs = crate::fs();

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
    do_stat_path(&path, args.arg1)
}

pub fn sys_fstat(args: &SyscallArgs) -> SyscallResult {
    let fd_num = args.arg0 as usize;
    let buf_addr = args.arg1;

    let (nodeid, file_fh) = {
        let table = crate::FD_TABLE.lock();
        match table.get(fd_num) {
            Some(d) => match d.kind {
                FdKind::File {
                    fuse_fh,
                    fuse_nodeid,
                    ..
                } => (fuse_nodeid, Some(fuse_fh)),
                FdKind::Directory { fuse_nodeid, .. } => (fuse_nodeid, None),
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
                FdKind::Socket { .. } | FdKind::Epoll { .. } => {
                    // Minimal stat with socket mode; enough for glibc's
                    // fstat-based isatty()/buffering checks.
                    let stat = Stat {
                        st_dev: 0,
                        st_ino: 0,
                        st_nlink: 1,
                        st_mode: 0o140666, // S_IFSOCK | 0666
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
                FdKind::Pipe { .. } => {
                    // Minimal stat with FIFO mode; enough for glibc's
                    // fstat-based isatty()/buffering checks.
                    let stat = Stat {
                        st_dev: 0,
                        st_ino: 0,
                        st_nlink: 1,
                        st_mode: 0o10666, // S_IFIFO | 0666
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

    if let Some(fh) = file_fh
        && let Err(e) = crate::syscall::handlers::io::flush_write_cache(fh)
    {
        return e as SyscallResult;
    }

    let fs = crate::fs();

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

/// Shared by `sys_access`/`sys_faccessat`/`sys_faccessat2`: existence check
/// only (our FUSE layer doesn't model per-bit read/write/execute
/// permissions, so `mode` is accepted but unchecked, same as `sys_access`
/// always did). `dirfd` must be `AT_FDCWD` or `path` absolute — no
/// relative-to-fd support, the same restriction `sys_openat` enforces.
fn do_faccessat(dirfd: i32, path: &[u8]) -> SyscallResult {
    if dirfd != AT_FDCWD && path.first() != Some(&b'/') {
        return ENOSYS;
    }

    let fs = crate::fs();

    let nodeid = match fs.resolve_path(path) {
        Ok(id) => id,
        Err(e) => return e as SyscallResult,
    };

    forget_if_not_root(fs, nodeid);
    0
}

pub fn sys_access(args: &SyscallArgs) -> SyscallResult {
    let path = match read_user_path(args.arg0) {
        Ok(p) => p,
        Err(e) => return e,
    };
    do_faccessat(AT_FDCWD, &path)
}

/// `faccessat(dirfd, path, mode, flags)`. `flags` (AT_EACCESS,
/// AT_SYMLINK_NOFOLLOW) don't change our existence-only semantics, so they
/// are accepted without validation — unlike `faccessat2`, the plain
/// `faccessat` libc wrapper never validates them against the kernel either.
pub fn sys_faccessat(args: &SyscallArgs) -> SyscallResult {
    let dirfd = args.arg0 as i32;
    let path = match read_user_path(args.arg1) {
        Ok(p) => p,
        Err(e) => return e,
    };
    do_faccessat(dirfd, &path)
}

/// `faccessat2(dirfd, path, mode, flags)` — like `faccessat`, but flags are
/// a real syscall argument (not emulated by glibc), so unknown bits are
/// rejected exactly like Linux's `do_faccessat2` does.
pub fn sys_faccessat2(args: &SyscallArgs) -> SyscallResult {
    const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
    const AT_EACCESS: u32 = 0x200;

    let dirfd = args.arg0 as i32;
    let path = match read_user_path(args.arg1) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let flags = args.arg3 as u32;
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EACCESS) != 0 {
        return EINVAL;
    }
    do_faccessat(dirfd, &path)
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

    let fs = crate::fs();

    // Use a kernel-side buffer for the FUSE readdir response.
    // We read FUSE dirents and convert them to linux_dirent64 format.
    let fuse_buf = [0u8; 4096];
    let fuse_buf_phys = crate::fs::virtio_fs::VirtioFsClient::v2p(fuse_buf.as_ptr());

    let fuse_bytes = match fs.readdir(
        fuse_nodeid,
        fuse_fh,
        dir_offset,
        fuse_buf_phys,
        fuse_buf.len() as u32,
    ) {
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
            4 => DT_DIR, // DT_DIR
            8 => DT_REG, // DT_REG
            _ => DT_UNKNOWN,
        };

        let dest = (buf_addr + user_pos as u64) as *mut u8;
        // SAFETY: Writing linux_dirent64 fields and name to user buffer.
        // The buffer is checked to have `reclen` bytes available above.
        unsafe {
            write_linux_dirent64_header(dest, dirent.ino, dirent.off as i64, reclen as u16, d_type);
            // Copy name after the 19-byte header
            let name_src = fuse_buf.as_ptr().add(fuse_pos + fuse_dirent_hdr_size);
            core::ptr::copy_nonoverlapping(name_src, dest.add(LINUX_DIRENT64_HEADER_SIZE), namelen);
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
        if let Some(desc) = table.get_mut(fd_num)
            && let FdKind::Directory { ref mut offset, .. } = desc.kind
        {
            *offset = last_off;
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

    let cwd = cwd_bytes();
    if size < cwd.len() + 1 {
        return -34; // ERANGE
    }

    // SAFETY: single address space; the caller supplies a writable buffer
    // of `size` bytes, checked above to fit the cwd + NUL.
    unsafe {
        core::ptr::copy_nonoverlapping(cwd.as_ptr(), buf_addr as *mut u8, cwd.len());
        core::ptr::write_volatile((buf_addr as *mut u8).add(cwd.len()), 0);
    }
    buf_addr as SyscallResult
}

pub fn sys_chdir(args: &SyscallArgs) -> SyscallResult {
    let path = match read_user_path(args.arg0) {
        Ok(p) => p,
        Err(e) => return e,
    };

    let fs = crate::fs();

    let nodeid = match fs.resolve_path(&path) {
        Ok(id) => id,
        Err(e) => return e as SyscallResult,
    };

    forget_if_not_root(fs, nodeid);

    // Store the (absolute) path as the new cwd so later relative paths
    // resolve against it. `read_user_path` already absolutized it.
    match core::str::from_utf8(&path) {
        Ok(p) => {
            *CWD.lock() = Some(alloc::string::String::from(p));
            0
        }
        Err(_) => EINVAL,
    }
}

pub fn sys_fchdir(_args: &SyscallArgs) -> SyscallResult {
    0
}

pub fn sys_rename(args: &SyscallArgs) -> SyscallResult {
    let old_path = match read_user_path(args.arg0) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let new_path = match read_user_path(args.arg1) {
        Ok(p) => p,
        Err(e) => return e,
    };

    let fs = crate::fs();
    let (old_parent_path, old_name) = match split_path(&old_path) {
        Some(v) => v,
        None => return EINVAL,
    };
    let (new_parent_path, new_name) = match split_path(&new_path) {
        Some(v) => v,
        None => return EINVAL,
    };

    let old_parent_nodeid = match fs.resolve_path(old_parent_path) {
        Ok(id) => id,
        Err(e) => return e as SyscallResult,
    };
    let new_parent_nodeid = match fs.resolve_path(new_parent_path) {
        Ok(id) => id,
        Err(e) => {
            forget_if_not_root(fs, old_parent_nodeid);
            return e as SyscallResult;
        }
    };
    let result = fs.rename(old_parent_nodeid, old_name, new_parent_nodeid, new_name);
    forget_if_not_root(fs, old_parent_nodeid);
    forget_if_not_root(fs, new_parent_nodeid);

    match result {
        Ok(()) => 0,
        Err(e) => e as SyscallResult,
    }
}

pub fn sys_truncate(args: &SyscallArgs) -> SyscallResult {
    let path = match read_user_path(args.arg0) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let len = args.arg1 as i64;
    if len < 0 {
        return EINVAL;
    }

    let fs = crate::fs();
    let nodeid = match fs.resolve_path(&path) {
        Ok(id) => id,
        Err(e) => return e as SyscallResult,
    };
    let result = fs.setattr_size(nodeid, None, len as u64);
    forget_if_not_root(fs, nodeid);

    match result {
        Ok(_) => 0,
        Err(e) => e as SyscallResult,
    }
}

fn do_mkdirat(dirfd: i32, path: &[u8], mode: u32) -> SyscallResult {
    if dirfd != AT_FDCWD && path.first() != Some(&b'/') {
        return ENOSYS;
    }

    let fs = crate::fs();
    let (parent_path, filename) = match split_path(path) {
        Some(v) => v,
        None => return EINVAL,
    };
    let parent_nodeid = match fs.resolve_path(parent_path) {
        Ok(id) => id,
        Err(e) => return e as SyscallResult,
    };
    let result = fs.mkdir(parent_nodeid, filename, mode);
    forget_if_not_root(fs, parent_nodeid);

    match result {
        Ok(entry) => {
            forget_if_not_root(fs, entry.nodeid);
            0
        }
        Err(e) => e as SyscallResult,
    }
}

pub fn sys_mkdir(args: &SyscallArgs) -> SyscallResult {
    let path = match read_user_path(args.arg0) {
        Ok(p) => p,
        Err(e) => return e,
    };
    do_mkdirat(AT_FDCWD, &path, args.arg1 as u32)
}

fn do_rmdir(dirfd: i32, path: &[u8]) -> SyscallResult {
    if dirfd != AT_FDCWD && path.first() != Some(&b'/') {
        return ENOSYS;
    }

    let fs = crate::fs();
    let (parent_path, filename) = match split_path(path) {
        Some(v) => v,
        None => return EINVAL,
    };
    let parent_nodeid = match fs.resolve_path(parent_path) {
        Ok(id) => id,
        Err(e) => return e as SyscallResult,
    };
    let result = fs.rmdir(parent_nodeid, filename);
    forget_if_not_root(fs, parent_nodeid);

    match result {
        Ok(()) => 0,
        Err(e) => e as SyscallResult,
    }
}

pub fn sys_rmdir(args: &SyscallArgs) -> SyscallResult {
    let path = match read_user_path(args.arg0) {
        Ok(p) => p,
        Err(e) => return e,
    };
    do_rmdir(AT_FDCWD, &path)
}

pub fn sys_creat(args: &SyscallArgs) -> SyscallResult {
    // creat(path, mode) is equivalent to open(path, O_CREAT|O_WRONLY|O_TRUNC, mode)
    let path = match read_user_path(args.arg0) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let mode = args.arg1 as u32;

    let fs = crate::fs();

    // Resolve parent directory and file name
    let (parent_path, filename) = match split_path(&path) {
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
            size: 0, // freshly created via creat() — will be updated on writes
        },
        flags,
    };

    let mut table = crate::FD_TABLE.lock();
    table.alloc(desc) as SyscallResult
}

pub fn sys_link(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

fn do_unlinkat(dirfd: i32, path: &[u8], flags: u32) -> SyscallResult {
    const AT_REMOVEDIR: u32 = 0x200;

    if flags & AT_REMOVEDIR != 0 {
        return do_rmdir(dirfd, path);
    }
    if dirfd != AT_FDCWD && path.first() != Some(&b'/') {
        return ENOSYS;
    }

    let fs = crate::fs();
    let (parent_path, filename) = match split_path(path) {
        Some(v) => v,
        None => return EINVAL,
    };
    let parent_nodeid = match fs.resolve_path(parent_path) {
        Ok(id) => id,
        Err(e) => return e as SyscallResult,
    };
    let result = fs.unlink(parent_nodeid, filename);
    forget_if_not_root(fs, parent_nodeid);

    match result {
        Ok(()) => 0,
        Err(e) => e as SyscallResult,
    }
}

pub fn sys_unlink(args: &SyscallArgs) -> SyscallResult {
    let path = match read_user_path(args.arg0) {
        Ok(p) => p,
        Err(e) => return e,
    };
    do_unlinkat(AT_FDCWD, &path, 0)
}

pub fn sys_symlink(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_readlink(_args: &SyscallArgs) -> SyscallResult {
    EINVAL // No symlinks in our fs
}

pub fn sys_fchmod(args: &SyscallArgs) -> SyscallResult {
    let fd = args.arg0 as usize;
    let table = crate::FD_TABLE.lock();
    if table.get(fd).is_none() {
        return EBADF;
    }
    0
}

pub fn sys_openat(args: &SyscallArgs) -> SyscallResult {
    let dirfd = args.arg0 as i32;
    let path = match read_user_path(args.arg1) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let flags = args.arg2 as u32;
    let mode = args.arg3 as u32;

    // If path is absolute or dirfd is AT_FDCWD, use normal open logic.
    // We don't support relative-to-fd paths yet.
    if dirfd != AT_FDCWD && path.first() != Some(&b'/') {
        return ENOSYS;
    }

    let fs = crate::fs();

    // Check if this is a directory open (O_DIRECTORY = 0o200000)
    let is_dir_open = flags & 0o200000 != 0;

    let nodeid = match fs.resolve_path(&path) {
        Ok(id) => id,
        Err(e) if e == -2 && flags & O_CREAT != 0 => {
            // File doesn't exist but O_CREAT is set — create it.
            let (parent_path, filename) = match split_path(&path) {
                Some(v) => v,
                None => return EINVAL,
            };
            let parent_nodeid = match fs.resolve_path(parent_path) {
                Ok(id) => id,
                Err(e) => return e as SyscallResult,
            };
            let (entry, open) = match fs.create(parent_nodeid, filename, flags, mode) {
                Ok(v) => v,
                Err(e) => {
                    forget_if_not_root(fs, parent_nodeid);
                    return e as SyscallResult;
                }
            };
            forget_if_not_root(fs, parent_nodeid);
            crate::syscall::handlers::io::invalidate_file_read_cache(entry.nodeid);

            let desc = FileDescriptor {
                kind: FdKind::File {
                    fuse_fh: open.fh,
                    fuse_nodeid: entry.nodeid,
                    offset: 0,
                    size: 0, // O_CREAT path — file is empty
                },
                flags,
            };
            let mut table = crate::FD_TABLE.lock();
            return table.alloc(desc) as SyscallResult;
        }
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
        table.alloc(desc) as SyscallResult
    } else {
        let open_out = match fs.open(nodeid, flags) {
            Ok(o) => o,
            Err(e) => {
                forget_if_not_root(fs, nodeid);
                return e as SyscallResult;
            }
        };

        // Capture file size at open time so mmap can bound DAX setup_mapping
        // to the actual file extent (avoids SIGBUS on past-EOF DAX accesses).
        let size = if flags & O_TRUNC != 0 {
            crate::syscall::handlers::io::invalidate_file_read_cache(nodeid);
            0
        } else {
            match fs.getattr(nodeid) {
                Ok(attr) => attr.attr.size,
                Err(_) => 0,
            }
        };

        let desc = FileDescriptor {
            kind: FdKind::File {
                fuse_fh: open_out.fh,
                fuse_nodeid: nodeid,
                offset: 0,
                size,
            },
            flags,
        };

        let mut table = crate::FD_TABLE.lock();
        table.alloc(desc) as SyscallResult
    }
}

pub fn sys_newfstatat(args: &SyscallArgs) -> SyscallResult {
    let dirfd = args.arg0 as i32;
    let path = match read_user_path_inner(args.arg1, true) {
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
            caller_rip: args.caller_rip,
            caller_rflags: args.caller_rflags,
        };
        return sys_fstat(&fstat_args);
    }

    if dirfd != AT_FDCWD && path.first() != Some(&b'/') {
        return ENOSYS;
    }

    do_stat_path(&path, buf_addr)
}

pub fn sys_unlinkat(args: &SyscallArgs) -> SyscallResult {
    let dirfd = args.arg0 as i32;
    let path = match read_user_path(args.arg1) {
        Ok(p) => p,
        Err(e) => return e,
    };
    do_unlinkat(dirfd, &path, args.arg2 as u32)
}

pub fn sys_mkdirat(args: &SyscallArgs) -> SyscallResult {
    let dirfd = args.arg0 as i32;
    let path = match read_user_path(args.arg1) {
        Ok(p) => p,
        Err(e) => return e,
    };
    do_mkdirat(dirfd, &path, args.arg2 as u32)
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
        Some(0) => (b"/" as &[u8], &path[1..]),
        Some(pos) => (&path[..pos], &path[pos + 1..]),
        None => (b"" as &[u8], path),
    };
    if name.is_empty() {
        return None;
    }
    Some((parent, name))
}
