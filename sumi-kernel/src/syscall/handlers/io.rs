use crate::arch::KernelDirectMap;
use crate::fs::{FdKind, FileDescriptor};
use crate::syscall::errno::*;
use crate::syscall::{SyscallArgs, SyscallResult};
use sumi_abi::address::VirtualAddr;

fn console_write(data: &[u8]) -> usize {
    crate::console().write(data)
}

fn console_read(buf: &mut [u8]) -> usize {
    crate::console().read(buf)
}

/// Read an iovec entry (base, len) from user memory.
/// SAFETY: Valid in sumi's single-address-space model where user and kernel share memory.
fn read_iovec(iov_ptr: usize, i: usize) -> (u64, u64) {
    unsafe {
        let base = core::ptr::read_volatile((iov_ptr + i * 16) as *const u64);
        let len = core::ptr::read_volatile((iov_ptr + i * 16 + 8) as *const u64);
        (base, len)
    }
}

const SEEK_SET: u64 = 0;
const SEEK_CUR: u64 = 1;
const SEEK_END: u64 = 2;

/// Release FUSE resources for a file descriptor. Uses release() for files
/// and releasedir() for directories, matching the FUSE protocol.
fn release_fuse_resources(desc: &FileDescriptor) {
    let fs = crate::fs();
    match desc.kind {
        FdKind::File {
            fuse_fh,
            fuse_nodeid,
            ..
        } => {
            fs.release(fuse_fh);
            fs.forget(fuse_nodeid, 1);
        }
        FdKind::Directory {
            fuse_fh,
            fuse_nodeid,
            ..
        } => {
            fs.releasedir(fuse_fh);
            fs.forget(fuse_nodeid, 1);
        }
        FdKind::Console => {}
        // Sockets/epoll instances have no FUSE handle; their teardown is
        // routed directly through `net::close_socket`/`close_epoll` by the
        // callers below instead of through this FUSE-specific helper.
        FdKind::Socket { .. } | FdKind::Epoll { .. } => {}
    }
}

fn count_remaining_refs(table: &crate::fs::FdTable, desc: &FileDescriptor) -> usize {
    match desc.kind {
        FdKind::File { fuse_fh, .. } | FdKind::Directory { fuse_fh, .. } => {
            table.count_fh_refs(fuse_fh)
        }
        FdKind::Socket { id } => table.count_socket_refs(id),
        FdKind::Epoll { id } => table.count_epoll_refs(id),
        FdKind::Console => 0,
    }
}

/// Translate a virtual address to physical. Works for both kernel (direct-map)
/// and user (lower-half, page-table walk) addresses.
pub(crate) fn translate_vaddr(vaddr: u64) -> Option<sumi_abi::address::PhysicalAddr> {
    use sumi_abi::arch::layout::{DIRECT_MAP_OFFSET, PAGE_SIZE};

    let va = VirtualAddr::new(vaddr as usize);
    if va.as_usize() >= DIRECT_MAP_OFFSET.as_usize() {
        // Kernel address — use direct map
        va.to_physical(&KernelDirectMap)
    } else {
        // User address — walk page table
        let entry = crate::KERNEL_PAGE_TABLE.lock().get_if_present(va).ok()??;
        let page_offset = vaddr as usize & (PAGE_SIZE - 1);
        Some(entry.addr().add(page_offset))
    }
}

/// How many bytes from `vaddr` until the next 2 MB page boundary.
pub(crate) fn bytes_to_page_end(vaddr: u64) -> u32 {
    use sumi_abi::arch::layout::PAGE_SIZE;
    let offset = vaddr as usize & (PAGE_SIZE - 1);
    (PAGE_SIZE - offset) as u32
}

/// Transfer data between a FUSE file handle and a user buffer, splitting at
/// 2 MB page boundaries so each DMA uses the correct physical address.
/// `op` is called with (file_offset, physical_addr, chunk_size) for each chunk.
pub(crate) fn fs_transfer_chunked(
    op: impl Fn(u64, sumi_abi::address::PhysicalAddr, u32) -> core::result::Result<u32, i32>,
    mut file_offset: u64,
    mut buf_vaddr: u64,
    mut remaining: u32,
) -> core::result::Result<u32, i32> {
    let mut total = 0u32;
    while remaining > 0 {
        let paddr = match translate_vaddr(buf_vaddr) {
            Some(p) => p,
            None if total > 0 => return Ok(total),
            None => return Err(EFAULT as i32),
        };
        let chunk = remaining.min(bytes_to_page_end(buf_vaddr));
        match op(file_offset, paddr, chunk) {
            Ok(0) => break,
            Ok(n) => {
                let n = n.min(remaining); // Clamp to prevent underflow
                total += n;
                buf_vaddr += n as u64;
                file_offset += n as u64;
                remaining -= n;
            }
            Err(e) if total > 0 => return Ok(total),
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

pub fn sys_read(args: &SyscallArgs) -> SyscallResult {
    let fd_num = args.arg0 as usize;
    let buf_vaddr = args.arg1;
    let count = args.arg2.min(u32::MAX as u64) as u32;

    let (kind, _flags) = {
        let table = crate::FD_TABLE.lock();
        match table.get(fd_num) {
            Some(d) => (d.kind, d.flags),
            None => return EBADF,
        }
    };

    match kind {
        FdKind::Console => {
            // SAFETY: In sumi unikernel, all user virtual addresses are valid
            // kernel-mapped memory. The caller guarantees buf_vaddr points to count bytes.
            let buf =
                unsafe { core::slice::from_raw_parts_mut(buf_vaddr as *mut u8, count as usize) };
            console_read(buf) as SyscallResult
        }
        FdKind::File {
            fuse_fh, offset, ..
        } => {
            let fs = crate::fs();
            match fs_transfer_chunked(
                |off, pa, cnt| fs.read(fuse_fh, off, pa, cnt),
                offset,
                buf_vaddr,
                count,
            ) {
                Ok(n) => {
                    let mut table = crate::FD_TABLE.lock();
                    if let Some(desc) = table.get_mut(fd_num)
                        && let FdKind::File { ref mut offset, .. } = desc.kind
                    {
                        *offset += n as u64;
                    }
                    n as SyscallResult
                }
                Err(e) => e as SyscallResult,
            }
        }
        _ => EBADF,
    }
}

pub fn sys_write(args: &SyscallArgs) -> SyscallResult {
    let fd_num = args.arg0 as usize;
    let buf_vaddr = args.arg1;
    let count = args.arg2.min(u32::MAX as u64) as usize;

    let (kind, _flags) = {
        let table = crate::FD_TABLE.lock();
        match table.get(fd_num) {
            Some(d) => (d.kind, d.flags),
            None => return EBADF,
        }
    };

    match kind {
        FdKind::Console => {
            // SAFETY: In sumi unikernel, all user virtual addresses are valid
            // kernel-mapped memory. The caller guarantees buf_vaddr points to count bytes.
            let data = unsafe { core::slice::from_raw_parts(buf_vaddr as *const u8, count) };
            console_write(data) as SyscallResult
        }
        FdKind::File {
            fuse_fh, offset, ..
        } => {
            let fs = crate::fs();
            match fs_transfer_chunked(
                |off, pa, cnt| fs.write(fuse_fh, off, pa, cnt),
                offset,
                buf_vaddr,
                count as u32,
            ) {
                Ok(n) => {
                    let mut table = crate::FD_TABLE.lock();
                    if let Some(desc) = table.get_mut(fd_num)
                        && let FdKind::File {
                            ref mut offset,
                            ref mut size,
                            ..
                        } = desc.kind
                    {
                        *offset += n as u64;
                        if *offset > *size {
                            *size = *offset;
                        }
                    }
                    n as SyscallResult
                }
                Err(e) => e as SyscallResult,
            }
        }
        _ => EBADF,
    }
}

pub fn sys_open(args: &SyscallArgs) -> SyscallResult {
    // Rewrite as openat(AT_FDCWD, path, flags, mode) so O_CREAT etc. are handled uniformly.
    let openat_args = crate::syscall::SyscallArgs {
        nr: 257,
        arg0: -100i64 as u64, // AT_FDCWD
        arg1: args.arg0,      // path
        arg2: args.arg1,      // flags
        arg3: args.arg2,      // mode
        arg4: 0,
        arg5: 0,
        caller_rip: args.caller_rip,
        caller_rflags: args.caller_rflags,
    };
    crate::syscall::handlers::fs::sys_openat(&openat_args)
}

pub fn sys_close(args: &SyscallArgs) -> SyscallResult {
    let fd_num = args.arg0 as usize;

    let (old, remaining_refs) = {
        let mut table = crate::FD_TABLE.lock();
        let old = table.free(fd_num);
        let refs = old
            .as_ref()
            .map(|desc| count_remaining_refs(&table, desc))
            .unwrap_or(0);
        (old, refs)
    };

    match old {
        None => EBADF,
        Some(desc) => {
            match desc.kind {
                FdKind::Socket { id } if remaining_refs == 0 => crate::net::close_socket(id),
                FdKind::Epoll { id } if remaining_refs == 0 => crate::net::close_epoll(id),
                // Only release resources if no other fd shares this handle.
                _ if remaining_refs == 0 => release_fuse_resources(&desc),
                _ => {}
            }
            0
        }
    }
}

pub fn sys_poll(args: &SyscallArgs) -> SyscallResult {
    let fds_ptr = args.arg0 as *mut PollFd;
    let nfds = args.arg1 as usize;
    let _timeout = args.arg2 as i32;

    // Limit nfds to prevent excessive iteration
    if nfds > 256 {
        return EINVAL;
    }

    let table = crate::FD_TABLE.lock();
    let mut ready = 0i64;

    for i in 0..nfds {
        // SAFETY: User passed a valid pollfd array.
        let pfd = unsafe { &mut *fds_ptr.add(i) };
        pfd.revents = 0;

        if pfd.fd < 0 {
            continue;
        }

        match table.get(pfd.fd as usize) {
            None => {
                pfd.revents = POLLNVAL;
                ready += 1;
            }
            Some(_desc) => {
                // All fd types are always ready in our synchronous model
                let revents = pfd.events & (POLLIN | POLLOUT);
                pfd.revents = revents;
                if revents != 0 {
                    ready += 1;
                }
            }
        }
    }

    ready
}

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

const POLLIN: i16 = 0x0001;
const POLLOUT: i16 = 0x0004;
const POLLNVAL: i16 = 0x0020;

pub fn sys_lseek(args: &SyscallArgs) -> SyscallResult {
    let fd_num = args.arg0 as usize;
    let seek_offset = args.arg1 as i64;
    let whence = args.arg2;

    // Read current offset and nodeid under the lock, then release it.
    let (cur, nodeid) = {
        let table = crate::FD_TABLE.lock();
        match table.get(fd_num) {
            Some(d) => match d.kind {
                FdKind::File {
                    offset,
                    fuse_nodeid,
                    ..
                } => (offset, fuse_nodeid),
                FdKind::Console => return -29, // ESPIPE
                _ => return EBADF,
            },
            None => return EBADF,
        }
    };

    let new_offset = match whence {
        SEEK_SET => {
            if seek_offset < 0 {
                return EINVAL;
            }
            seek_offset as u64
        }
        SEEK_CUR => {
            let new = cur as i64 + seek_offset;
            if new < 0 {
                return EINVAL;
            }
            new as u64
        }
        SEEK_END => {
            let fs = crate::fs();
            let attr = match fs.getattr(nodeid) {
                Ok(a) => a,
                Err(e) => return e as SyscallResult,
            };
            let new = attr.attr.size as i64 + seek_offset;
            if new < 0 {
                return EINVAL;
            }
            new as u64
        }
        _ => return EINVAL,
    };

    // Write the new offset back under the lock.
    let mut table = crate::FD_TABLE.lock();
    if let Some(desc) = table.get_mut(fd_num)
        && let FdKind::File { ref mut offset, .. } = desc.kind
    {
        *offset = new_offset;
        return new_offset as SyscallResult;
    }
    EBADF
}

pub fn sys_ioctl(_args: &SyscallArgs) -> SyscallResult {
    // ENOTTY signals "not a terminal" so glibc's __isatty() correctly
    // detects that stdout is not a TTY. ENOSYS would be treated differently
    // and may cause glibc to alter stdout buffering mode.
    ENOTTY
}

pub fn sys_pread64(args: &SyscallArgs) -> SyscallResult {
    let fd_num = args.arg0 as usize;
    let buf_vaddr = args.arg1;
    let count = args.arg2 as u32;
    let offset = args.arg3;

    let kind = {
        let table = crate::FD_TABLE.lock();
        match table.get(fd_num) {
            Some(d) => d.kind,
            None => return EBADF,
        }
    };

    match kind {
        FdKind::File { fuse_fh, .. } => {
            let fs = crate::fs();
            match fs_transfer_chunked(
                |off, pa, cnt| fs.read(fuse_fh, off, pa, cnt),
                offset,
                buf_vaddr,
                count,
            ) {
                Ok(n) => n as SyscallResult,
                Err(e) => e as SyscallResult,
            }
        }
        _ => EBADF,
    }
}

pub fn sys_pwrite64(args: &SyscallArgs) -> SyscallResult {
    let fd_num = args.arg0 as usize;
    let buf_vaddr = args.arg1;
    let count = args.arg2 as u32;
    let offset = args.arg3;

    let kind = {
        let table = crate::FD_TABLE.lock();
        match table.get(fd_num) {
            Some(d) => d.kind,
            None => return EBADF,
        }
    };

    match kind {
        FdKind::File { fuse_fh, .. } => {
            let fs = crate::fs();
            match fs_transfer_chunked(
                |off, pa, cnt| fs.write(fuse_fh, off, pa, cnt),
                offset,
                buf_vaddr,
                count,
            ) {
                Ok(n) => {
                    // pwrite does not advance the fd offset, but it can extend
                    // the file. Refresh the cached size if so.
                    let new_end = offset + n as u64;
                    let mut table = crate::FD_TABLE.lock();
                    if let Some(desc) = table.get_mut(fd_num)
                        && let FdKind::File { ref mut size, .. } = desc.kind
                        && new_end > *size
                    {
                        *size = new_end;
                    }
                    n as SyscallResult
                }
                Err(e) => e as SyscallResult,
            }
        }
        _ => EBADF,
    }
}

pub fn sys_readv(args: &SyscallArgs) -> SyscallResult {
    let fd_num = args.arg0 as usize;
    let iov_ptr = args.arg1 as usize;
    let iovcnt = args.arg2 as usize;

    let (kind, _flags) = {
        let table = crate::FD_TABLE.lock();
        match table.get(fd_num) {
            Some(d) => (d.kind, d.flags),
            None => return EBADF,
        }
    };

    match kind {
        FdKind::Console => {
            let mut total = 0usize;
            for i in 0..iovcnt {
                let (iov_base, iov_len) = read_iovec(iov_ptr, i);
                let iov_len = iov_len as usize;
                if iov_len == 0 {
                    continue;
                }
                let buf = unsafe { core::slice::from_raw_parts_mut(iov_base as *mut u8, iov_len) };
                let n = console_read(buf);
                total += n;
                if n < iov_len {
                    break;
                }
            }
            total as SyscallResult
        }
        FdKind::File {
            fuse_fh, offset, ..
        } => {
            let fs = crate::fs();

            let mut total = 0i64;
            let mut cur_offset = offset;

            for i in 0..iovcnt {
                let (iov_base, iov_len_raw) = read_iovec(iov_ptr, i);
                let iov_len = iov_len_raw.min(u32::MAX as u64) as u32;
                if iov_len == 0 {
                    continue;
                }

                match fs_transfer_chunked(
                    |off, pa, cnt| fs.read(fuse_fh, off, pa, cnt),
                    cur_offset,
                    iov_base,
                    iov_len,
                ) {
                    Ok(0) => break,
                    Ok(n) => {
                        total += n as i64;
                        cur_offset += n as u64;
                    }
                    Err(e) if total > 0 => break,
                    Err(e) => return e as SyscallResult,
                }
            }

            // Update offset
            let mut table = crate::FD_TABLE.lock();
            if let Some(desc) = table.get_mut(fd_num)
                && let FdKind::File { ref mut offset, .. } = desc.kind
            {
                *offset = cur_offset;
            }
            total as SyscallResult
        }
        _ => EBADF,
    }
}

pub fn sys_writev(args: &SyscallArgs) -> SyscallResult {
    let fd_num = args.arg0 as usize;
    let iov_ptr = args.arg1 as usize;
    let iovcnt = args.arg2 as usize;

    let kind = {
        let table = crate::FD_TABLE.lock();
        match table.get(fd_num) {
            Some(d) => d.kind,
            None => return EBADF,
        }
    };

    match kind {
        FdKind::Console => {
            let mut total = 0usize;
            for i in 0..iovcnt {
                let (iov_base, iov_len) = read_iovec(iov_ptr, i);
                let iov_len = iov_len as usize;
                if iov_len == 0 {
                    continue;
                }
                let data = unsafe { core::slice::from_raw_parts(iov_base as *const u8, iov_len) };
                total += console_write(data);
            }
            total as SyscallResult
        }
        FdKind::File {
            fuse_fh, offset, ..
        } => {
            let fs = crate::fs();

            let mut total = 0i64;
            let mut cur_offset = offset;

            for i in 0..iovcnt {
                let (iov_base, iov_len_raw) = read_iovec(iov_ptr, i);
                let iov_len = iov_len_raw.min(u32::MAX as u64) as u32;
                if iov_len == 0 {
                    continue;
                }

                match fs_transfer_chunked(
                    |off, pa, cnt| fs.write(fuse_fh, off, pa, cnt),
                    cur_offset,
                    iov_base,
                    iov_len,
                ) {
                    Ok(0) => break,
                    Ok(n) => {
                        total += n as i64;
                        cur_offset += n as u64;
                    }
                    Err(e) if total > 0 => break,
                    Err(e) => return e as SyscallResult,
                }
            }

            // Update offset and grow cached size if we extended the file.
            let mut table = crate::FD_TABLE.lock();
            if let Some(desc) = table.get_mut(fd_num)
                && let FdKind::File {
                    ref mut offset,
                    ref mut size,
                    ..
                } = desc.kind
            {
                *offset = cur_offset;
                if cur_offset > *size {
                    *size = cur_offset;
                }
            }
            total as SyscallResult
        }
        _ => EBADF,
    }
}

pub fn sys_fcntl(args: &SyscallArgs) -> SyscallResult {
    let fd_num = args.arg0 as usize;
    let cmd = args.arg1 as i32;

    const F_GETFD: i32 = 1;
    const F_SETFD: i32 = 2;
    const F_GETFL: i32 = 3;
    const F_SETFL: i32 = 4;
    const F_DUPFD: i32 = 0;
    const F_DUPFD_CLOEXEC: i32 = 1030;
    const O_NONBLOCK: u32 = crate::net::socket::SOCK_NONBLOCK;

    match cmd {
        F_SETFL => {
            let socket_update = {
                let mut table = crate::FD_TABLE.lock();
                let Some(desc) = table.get_mut(fd_num) else {
                    return EBADF;
                };
                let nonblock = args.arg2 as u32 & O_NONBLOCK;
                desc.flags = (desc.flags & !O_NONBLOCK) | nonblock;
                match desc.kind {
                    FdKind::Socket { id } => Some((id, nonblock != 0)),
                    _ => None,
                }
            };

            if let Some((id, nonblocking)) = socket_update {
                let mut g = crate::net::lock();
                let Some(obj) = g.socket_get_mut(id) else {
                    return EBADF;
                };
                obj.nonblocking = nonblocking;
            }
            0
        }
        _ => {
            let table = crate::FD_TABLE.lock();
            match table.get(fd_num) {
                None => EBADF,
                Some(desc) => match cmd {
                    F_GETFD => 0, // no close-on-exec in unikernel
                    F_SETFD => 0, // ignore
                    F_GETFL => desc.flags as SyscallResult,
                    F_DUPFD | F_DUPFD_CLOEXEC => {
                        let new_desc = *desc;
                        drop(table);
                        let mut table = crate::FD_TABLE.lock();
                        table.alloc(new_desc) as SyscallResult
                    }
                    _ => EINVAL,
                },
            }
        }
    }
}

pub fn sys_pipe(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_select(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_dup(args: &SyscallArgs) -> SyscallResult {
    let old_fd = args.arg0 as usize;

    let mut table = crate::FD_TABLE.lock();
    let desc = match table.get(old_fd) {
        Some(d) => *d,
        None => return EBADF,
    };

    table.alloc(desc) as SyscallResult
}

pub fn sys_dup2(args: &SyscallArgs) -> SyscallResult {
    let old_fd = args.arg0 as usize;
    let new_fd = args.arg1 as usize;

    if old_fd == new_fd {
        // If old_fd is valid, return new_fd. Otherwise EBADF.
        let table = crate::FD_TABLE.lock();
        return match table.get(old_fd) {
            Some(_) => new_fd as SyscallResult,
            None => EBADF,
        };
    }

    let (evicted, remaining_refs) = {
        let mut table = crate::FD_TABLE.lock();
        let desc = match table.get(old_fd) {
            Some(d) => *d,
            None => return EBADF,
        };

        let old_occupant = table.put(new_fd, desc);

        // Check if the evicted fd's handle is still referenced by other fds.
        let refs = old_occupant
            .as_ref()
            .map(|desc| count_remaining_refs(&table, desc))
            .unwrap_or(0);
        (old_occupant, refs)
    };

    // Tear down whatever new_fd used to reference. Sockets/epoll instances
    // always close; FUSE resources only release if no other fd shares the
    // handle.
    if let Some(ref evicted) = evicted {
        match evicted.kind {
            FdKind::Socket { id } if remaining_refs == 0 => crate::net::close_socket(id),
            FdKind::Epoll { id } if remaining_refs == 0 => crate::net::close_epoll(id),
            _ if remaining_refs == 0 => release_fuse_resources(evicted),
            _ => {}
        }
    }

    new_fd as SyscallResult
}
