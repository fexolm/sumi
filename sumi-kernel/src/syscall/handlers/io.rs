use crate::arch::KernelDirectMap;
use crate::fs::{FdKind, FileDescriptor};
use crate::syscall::errno::*;
use crate::syscall::{SyscallArgs, SyscallResult};
use sumi_abi::address::VirtualAddr;

const SEEK_SET: u64 = 0;
const SEEK_CUR: u64 = 1;
const SEEK_END: u64 = 2;

/// Translate a virtual address to physical. Works for both kernel (direct-map)
/// and user (lower-half, page-table walk) addresses.
fn translate_vaddr(vaddr: u64) -> Option<sumi_abi::address::PhysicalAddr> {
    use sumi_abi::arch::layout::{DIRECT_MAP_OFFSET, PAGE_SIZE};

    let va = VirtualAddr::new(vaddr as usize);
    if va.as_usize() >= DIRECT_MAP_OFFSET.as_usize() {
        // Kernel address — use direct map
        va.to_physical(&KernelDirectMap)
    } else {
        // User address — walk page table
        let entry = crate::KERNEL_PAGE_TABLE.get_if_present(va).ok()??;
        let page_offset = vaddr as usize & (PAGE_SIZE - 1);
        Some(entry.addr().add(page_offset))
    }
}

/// How many bytes from `vaddr` until the next 2 MB page boundary.
fn bytes_to_page_end(vaddr: u64) -> u32 {
    use sumi_abi::arch::layout::PAGE_SIZE;
    let offset = vaddr as usize & (PAGE_SIZE - 1);
    (PAGE_SIZE - offset) as u32
}

/// Transfer data between a FUSE file handle and a user buffer, splitting at
/// 2 MB page boundaries so each DMA uses the correct physical address.
/// `op` is called with (file_offset, physical_addr, chunk_size) for each chunk.
fn fs_transfer_chunked(
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
    let count = args.arg2 as u32;

    let (kind, _flags) = {
        let table = crate::FD_TABLE.lock();
        match table.get(fd_num) {
            Some(d) => (d.kind, d.flags),
            None => return EBADF,
        }
    };

    match kind {
        FdKind::Console => 0,
        FdKind::File {
            fuse_fh, offset, ..
        } => {
            let fs = match crate::VIRTIO_FS.get() {
                Some(fs) => fs,
                None => return EIO,
            };
            match fs_transfer_chunked(|off, pa, cnt| fs.read(fuse_fh, off, pa, cnt), offset, buf_vaddr, count) {
                Ok(n) => {
                    let mut table = crate::FD_TABLE.lock();
                    if let Some(desc) = table.get_mut(fd_num) {
                        if let FdKind::File {
                            ref mut offset, ..
                        } = desc.kind
                        {
                            *offset += n as u64;
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

pub fn sys_write(args: &SyscallArgs) -> SyscallResult {
    let fd_num = args.arg0 as usize;
    let buf_vaddr = args.arg1;
    let count = args.arg2 as usize;

    let (kind, _flags) = {
        let table = crate::FD_TABLE.lock();
        match table.get(fd_num) {
            Some(d) => (d.kind, d.flags),
            None => return EBADF,
        }
    };

    match kind {
        FdKind::Console => {
            for i in 0..count {
                let byte =
                    unsafe { core::ptr::read_volatile((buf_vaddr as usize + i) as *const u8) };
                crate::arch::debugcon_write_byte(byte);
            }
            count as SyscallResult
        }
        FdKind::File {
            fuse_fh, offset, ..
        } => {
            let fs = match crate::VIRTIO_FS.get() {
                Some(fs) => fs,
                None => return EIO,
            };
            match fs_transfer_chunked(|off, pa, cnt| fs.write(fuse_fh, off, pa, cnt), offset, buf_vaddr, count as u32) {
                Ok(n) => {
                    let mut table = crate::FD_TABLE.lock();
                    if let Some(desc) = table.get_mut(fd_num) {
                        if let FdKind::File {
                            ref mut offset, ..
                        } = desc.kind
                        {
                            *offset += n as u64;
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
    let path_ptr = args.arg0 as *const u8;
    let flags = args.arg1 as u32;

    let fs = match crate::VIRTIO_FS.get() {
        Some(fs) => fs,
        None => return EIO,
    };

    let path = unsafe {
        let mut len = 0;
        while core::ptr::read_volatile(path_ptr.add(len)) != 0 {
            len += 1;
            if len > 4095 {
                return -36; // ENAMETOOLONG
            }
        }
        core::slice::from_raw_parts(path_ptr, len)
    };

    let nodeid = match fs.resolve_path(path) {
        Ok(id) => id,
        Err(e) => return e as SyscallResult,
    };

    let open_out = match fs.open(nodeid, flags) {
        Ok(o) => o,
        Err(e) => return e as SyscallResult,
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
        None => EMFILE,
    }
}

pub fn sys_close(args: &SyscallArgs) -> SyscallResult {
    let fd_num = args.arg0 as usize;

    let old = {
        let mut table = crate::FD_TABLE.lock();
        table.free(fd_num)
    };

    match old {
        None => EBADF,
        Some(desc) => {
            match desc.kind {
                FdKind::Console => {}
                FdKind::File { fuse_fh, fuse_nodeid, .. }
                | FdKind::Directory { fuse_fh, fuse_nodeid, .. } => {
                    if let Some(fs) = crate::VIRTIO_FS.get() {
                        fs.release(fuse_fh);
                        fs.forget(fuse_nodeid, 1);
                    }
                }
            }
            0
        }
    }
}

pub fn sys_poll(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_lseek(args: &SyscallArgs) -> SyscallResult {
    let fd_num = args.arg0 as usize;
    let seek_offset = args.arg1 as i64;
    let whence = args.arg2;

    // Read current offset and nodeid under the lock, then release it.
    let (cur, nodeid) = {
        let table = crate::FD_TABLE.lock();
        match table.get(fd_num) {
            Some(d) => match d.kind {
                FdKind::File { offset, fuse_nodeid, .. } => (offset, fuse_nodeid),
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
            let fs = match crate::VIRTIO_FS.get() {
                Some(fs) => fs,
                None => return EIO,
            };
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
    if let Some(desc) = table.get_mut(fd_num) {
        if let FdKind::File { ref mut offset, .. } = desc.kind {
            *offset = new_offset;
            return new_offset as SyscallResult;
        }
    }
    EBADF
}

pub fn sys_ioctl(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
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
            let fs = match crate::VIRTIO_FS.get() {
                Some(fs) => fs,
                None => return EIO,
            };
            match fs_transfer_chunked(|off, pa, cnt| fs.read(fuse_fh, off, pa, cnt), offset, buf_vaddr, count) {
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
            let fs = match crate::VIRTIO_FS.get() {
                Some(fs) => fs,
                None => return EIO,
            };
            match fs_transfer_chunked(|off, pa, cnt| fs.write(fuse_fh, off, pa, cnt), offset, buf_vaddr, count) {
                Ok(n) => n as SyscallResult,
                Err(e) => e as SyscallResult,
            }
        }
        _ => EBADF,
    }
}

pub fn sys_readv(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
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
                let iov_base =
                    unsafe { core::ptr::read_volatile((iov_ptr + i * 16) as *const u64) } as usize;
                let iov_len =
                    unsafe { core::ptr::read_volatile((iov_ptr + i * 16 + 8) as *const u64) }
                        as usize;
                for j in 0..iov_len {
                    let byte = unsafe { core::ptr::read_volatile((iov_base + j) as *const u8) };
                    crate::arch::debugcon_write_byte(byte);
                }
                total += iov_len;
            }
            total as SyscallResult
        }
        _ => ENOSYS,
    }
}

pub fn sys_pipe(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_select(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_dup(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_dup2(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}
