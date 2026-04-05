use sumi_abi::address::VirtualAddr;
use sumi_abi::arch::layout::{DAX_SLOT_COUNT, DAX_WINDOW_BASE, PAGE_SIZE};
use sumi_abi::fuse::{FUSE_SETUPMAPPING_FLAG_READ, FUSE_SETUPMAPPING_FLAG_WRITE};

use crate::exec::{align_up_2mb, zero_page};
use crate::fs::FdKind;
use crate::memory::vma::{MappingBacking, Vma};
use crate::syscall::errno::*;
use crate::syscall::{SyscallArgs, SyscallResult};
use crate::syscall::handlers::io::fs_transfer_chunked;

const MAP_PRIVATE: i32 = 0x02;
const MAP_SHARED: i32 = 0x01;
const PROT_WRITE: i32 = 0x2;
const MAP_ANONYMOUS: i32 = 0x20;
const MAP_FIXED: i32 = 0x10;

pub fn sys_mmap(args: &SyscallArgs) -> SyscallResult {
    let addr_hint = args.arg0;
    let len = args.arg1 as usize;
    let prot = args.arg2 as i32;
    let flags = args.arg3 as i32;
    let fd = args.arg4 as i32;
    let offset = args.arg5 as usize;

    if len == 0 {
        return EINVAL;
    }

    if flags & MAP_FIXED != 0 && !(addr_hint as usize).is_multiple_of(PAGE_SIZE) {
        return EINVAL;
    }

    // Sub-page file offset: how far into the first 2MB page the file data starts.
    // Must be computed before aligned_len so the extra partial-page is included.
    let sub_page_offset = if flags & MAP_ANONYMOUS == 0 { offset % PAGE_SIZE } else { 0 };
    let total_len = match len.checked_add(sub_page_offset) {
        Some(v) => v,
        None => return EINVAL,
    };
    let aligned_len = align_up_2mb(total_len as u64) as usize;
    let pages = aligned_len / PAGE_SIZE;
    let file_page_offset = offset - sub_page_offset;

    // Determine virtual address.
    let (vaddr, saved_next) = if flags & MAP_FIXED != 0 {
        (VirtualAddr::new(addr_hint as usize), None)
    } else {
        let mut next = crate::MMAP_NEXT.lock();
        if next.as_usize() < aligned_len {
            return ENOMEM;
        }
        let old = *next;
        let base = next.as_usize() - aligned_len;
        *next = VirtualAddr::new(base);
        (VirtualAddr::new(base), Some(old))
    };

    // If MAP_FIXED, tear down any overlapping VMAs first.
    // Loop because remove_overlapping only returns up to 4 at a time.
    if flags & MAP_FIXED != 0 {
        let vaddr_end = VirtualAddr::new(vaddr.as_usize() + aligned_len);
        loop {
            let removed = crate::VMA_TABLE.lock().remove_overlapping(vaddr, vaddr_end);
            let any_found = removed.iter().any(|r| r.is_some());
            for maybe_vma in removed.into_iter().flatten() {
                tear_down_vma(maybe_vma);
            }
            if !any_found {
                break;
            }
        }
    }

    if flags & MAP_ANONYMOUS != 0 {
        // Anonymous mapping path.
        let result = map_anonymous_pages(vaddr, pages);
        if let Err(e) = result {
            restore_mmap_next(saved_next);
            return e;
        }

        let vma = Vma {
            start: vaddr,
            end: VirtualAddr::new(vaddr.as_usize() + aligned_len),
            backing: MappingBacking::Anonymous,
        };
        if crate::VMA_TABLE.lock().insert(vma).is_err() {
            // VMA table full: roll back the pages we just mapped.
            let mut v = vaddr;
            for _ in 0..pages {
                if let Ok(p) = crate::KERNEL_PAGE_TABLE.unmap_2mb(v) {
                    let _ = crate::PAGE_ALLOCATOR.free(p);
                }
                v = v.add(PAGE_SIZE);
            }
            restore_mmap_next(saved_next);
            return ENOMEM;
        }

        return vaddr.as_u64() as SyscallResult;
    }

    // File-backed mapping.
    let (fuse_fh, fuse_nodeid) = {
        let table = crate::FD_TABLE.lock();
        match table.get(fd as usize) {
            Some(d) => match d.kind {
                FdKind::File { fuse_fh, fuse_nodeid, .. } => (fuse_fh, fuse_nodeid),
                _ => return EBADF,
            },
            None => return EBADF,
        }
    };

    // MAP_PRIVATE + PROT_WRITE: private copy (always alloc pages, FUSE_READ content).
    if flags & MAP_PRIVATE != 0 && prot & PROT_WRITE != 0 {
        return private_copy_path(
            vaddr, pages, saved_next, fuse_fh, fuse_nodeid,
            file_page_offset as u64, aligned_len as u64, sub_page_offset,
        );
    }

    // MAP_PRIVATE read-only or MAP_SHARED: try DAX first, fall back to private copy.
    let dax_flags = if flags & MAP_SHARED != 0 {
        FUSE_SETUPMAPPING_FLAG_READ | FUSE_SETUPMAPPING_FLAG_WRITE
    } else {
        FUSE_SETUPMAPPING_FLAG_READ
    };

    // Try DAX path.
    match dax_path(
        vaddr, pages, fuse_fh, fuse_nodeid,
        file_page_offset as u64, aligned_len as u64, dax_flags, sub_page_offset,
    ) {
        Ok(result) => result,
        Err(_) => {
            // DAX window exhausted — fall back to private copy (read-only content).
            private_copy_path(
                vaddr, pages, saved_next, fuse_fh, fuse_nodeid,
                file_page_offset as u64, aligned_len as u64, sub_page_offset,
            )
        }
    }
}

pub fn sys_mprotect(_args: &SyscallArgs) -> SyscallResult {
    // No-op: 2 MB pages are all RWX in ring 0
    0
}

pub fn sys_munmap(args: &SyscallArgs) -> SyscallResult {
    let addr = args.arg0 as usize;
    let len = args.arg1 as usize;

    let end = match addr.checked_add(len) {
        Some(e) => e,
        None => return EINVAL,
    };
    let aligned_start = addr & !(PAGE_SIZE - 1);
    let aligned_end = match end.checked_add(PAGE_SIZE - 1) {
        Some(v) => v & !(PAGE_SIZE - 1),
        None => return EINVAL,
    };
    let lookup_addr = VirtualAddr::new(aligned_start);

    // Try to find a tracked VMA: use find() to locate by containment, then remove by its actual start.
    let vma = {
        let mut table = crate::VMA_TABLE.lock();
        let found_start = table.find(lookup_addr).map(|v| v.start);
        found_start.and_then(|start| table.remove(start))
    };

    if let Some(vma) = vma {
        tear_down_vma(vma);
        return 0;
    }

    // No VMA found — fall back to anonymous unmap behavior.
    let mut vaddr = aligned_start;
    while vaddr < aligned_end {
        if let Ok(paddr) = crate::KERNEL_PAGE_TABLE.unmap_2mb(VirtualAddr::new(vaddr)) {
            let _ = crate::PAGE_ALLOCATOR.free(paddr);
        }
        vaddr += PAGE_SIZE;
    }

    0
}

pub fn sys_brk(args: &SyscallArgs) -> SyscallResult {
    let requested = args.arg0;
    let mut current = crate::BRK_CURRENT.lock();
    let base = *crate::BRK_BASE.lock();

    if requested == 0 || (requested as usize) < base.as_usize() {
        return current.as_u64() as SyscallResult;
    }

    let old_end = align_up_2mb(current.as_u64());
    let new_end = align_up_2mb(requested);

    if new_end > old_end {
        let mut vaddr = old_end;
        while vaddr < new_end {
            let paddr = match crate::PAGE_ALLOCATOR.alloc(1) {
                Ok(p) => p,
                Err(_) => {
                    rollback_pages(old_end, vaddr);
                    return current.as_u64() as SyscallResult;
                }
            };
            zero_page(paddr);
            if crate::KERNEL_PAGE_TABLE
                .map_2mb(VirtualAddr::new(vaddr as usize), paddr)
                .is_err()
            {
                let _ = crate::PAGE_ALLOCATOR.free(paddr);
                rollback_pages(old_end, vaddr);
                return current.as_u64() as SyscallResult;
            }
            vaddr += PAGE_SIZE as u64;
        }
    } else if new_end < old_end {
        let mut vaddr = new_end;
        while vaddr < old_end {
            if let Ok(paddr) =
                crate::KERNEL_PAGE_TABLE.unmap_2mb(VirtualAddr::new(vaddr as usize))
            {
                let _ = crate::PAGE_ALLOCATOR.free(paddr);
            }
            vaddr += PAGE_SIZE as u64;
        }
    }

    *current = VirtualAddr::new(requested as usize);
    requested as SyscallResult
}

pub fn sys_mremap(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_msync(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_mincore(_args: &SyscallArgs) -> SyscallResult {
    ENOSYS
}

pub fn sys_madvise(_args: &SyscallArgs) -> SyscallResult {
    // madvise is advisory — returning 0 is always safe
    0
}

/// Tear down a VMA: unmap pages, free DAX slot or physical pages as appropriate.
fn tear_down_vma(vma: Vma) {
    let aligned_start = vma.start.as_usize();
    let aligned_end = vma.end.as_usize();

    match vma.backing {
        MappingBacking::Anonymous | MappingBacking::PrivateFile { .. } => {
            // Unmap and free physical pages.
            let mut vaddr = aligned_start;
            while vaddr < aligned_end {
                if let Ok(paddr) = crate::KERNEL_PAGE_TABLE.unmap_2mb(VirtualAddr::new(vaddr)) {
                    let _ = crate::PAGE_ALLOCATOR.free(paddr);
                }
                vaddr += PAGE_SIZE;
            }
        }
        MappingBacking::Dax { dax_offset, fuse_fh: _, fuse_nodeid: _, file_offset: _ } => {
            // Unmap DAX pages from the page table.
            let mut vaddr = aligned_start;
            while vaddr < aligned_end {
                let _ = crate::KERNEL_PAGE_TABLE.unmap_2mb(VirtualAddr::new(vaddr));
                vaddr += PAGE_SIZE;
            }
            // Ask the host to unmap from the DAX window.
            if let Some(fs) = crate::VIRTIO_FS.get() {
                let len = (aligned_end - aligned_start) as u64;
                let _ = fs.remove_mapping(dax_offset, len);
            }
            // Return the DAX slots.
            let slot_count = (aligned_end - aligned_start) / PAGE_SIZE;
            crate::DAX_ALLOCATOR.lock().free(dax_offset, slot_count);
        }
    }
}

/// Map anonymous zeroed pages at `vaddr` for `pages` 2MB slots.
/// Returns Ok(()) or an errno on failure. On failure, any already-mapped pages are rolled back.
fn map_anonymous_pages(vaddr: VirtualAddr, pages: usize) -> Result<(), SyscallResult> {
    for i in 0..pages {
        let page_vaddr = vaddr.add(i * PAGE_SIZE);
        let paddr = match crate::PAGE_ALLOCATOR.alloc(1) {
            Ok(p) => p,
            Err(_) => {
                for j in 0..i {
                    if let Ok(p) = crate::KERNEL_PAGE_TABLE.unmap_2mb(vaddr.add(j * PAGE_SIZE)) {
                        let _ = crate::PAGE_ALLOCATOR.free(p);
                    }
                }
                return Err(ENOMEM);
            }
        };
        zero_page(paddr);
        if crate::KERNEL_PAGE_TABLE.map_2mb(page_vaddr, paddr).is_err() {
            let _ = crate::PAGE_ALLOCATOR.free(paddr);
            for j in 0..i {
                if let Ok(p) = crate::KERNEL_PAGE_TABLE.unmap_2mb(vaddr.add(j * PAGE_SIZE)) {
                    let _ = crate::PAGE_ALLOCATOR.free(p);
                }
            }
            return Err(ENOMEM);
        }
    }
    Ok(())
}

/// Private copy mapping: allocate physical pages and read file content into them.
#[allow(clippy::too_many_arguments)]
fn private_copy_path(
    vaddr: VirtualAddr,
    pages: usize,
    saved_next: Option<VirtualAddr>,
    fuse_fh: u64,
    fuse_nodeid: u64,
    file_offset: u64,
    file_len: u64,
    sub_page_offset: usize,
) -> SyscallResult {
    let aligned_len = pages * PAGE_SIZE;

    if let Err(e) = map_anonymous_pages(vaddr, pages) {
        restore_mmap_next(saved_next);
        return e;
    }

    // Read file content into the freshly mapped pages.
    if let Some(fs) = crate::VIRTIO_FS.get() {
        let read_len = file_len.min(aligned_len as u64).min(u32::MAX as u64) as u32;
        let _ = fs_transfer_chunked(
            |off, pa, cnt| fs.read(fuse_fh, off, pa, cnt),
            file_offset,
            vaddr.as_u64(),
            read_len,
        );
    }

    let vma = Vma {
        start: vaddr,
        end: VirtualAddr::new(vaddr.as_usize() + aligned_len),
        backing: MappingBacking::PrivateFile { fuse_fh, fuse_nodeid },
    };
    if crate::VMA_TABLE.lock().insert(vma).is_err() {
        // VMA table full: roll back pages and address reservation.
        let mut v = vaddr;
        for _ in 0..pages {
            if let Ok(p) = crate::KERNEL_PAGE_TABLE.unmap_2mb(v) {
                let _ = crate::PAGE_ALLOCATOR.free(p);
            }
            v = v.add(PAGE_SIZE);
        }
        restore_mmap_next(saved_next);
        return ENOMEM;
    }

    (vaddr.as_u64() + sub_page_offset as u64) as SyscallResult
}

/// DAX mapping: allocate DAX slots, ask the host to set up the mapping, map DAX pages.
#[allow(clippy::too_many_arguments)]
fn dax_path(
    vaddr: VirtualAddr,
    pages: usize,
    fuse_fh: u64,
    fuse_nodeid: u64,
    file_offset: u64,
    file_len: u64,
    dax_flags: u64,
    sub_page_offset: usize,
) -> Result<SyscallResult, ()> {
    let aligned_len = pages * PAGE_SIZE;

    // Check that `pages` does not exceed the DAX slot count.
    if pages > DAX_SLOT_COUNT {
        return Err(());
    }

    let dax_offset = crate::DAX_ALLOCATOR.lock().alloc(pages).map_err(|_| ())?;

    let fs = match crate::VIRTIO_FS.get() {
        Some(fs) => fs,
        None => {
            crate::DAX_ALLOCATOR.lock().free(dax_offset, pages);
            return Err(());
        }
    };

    if fs.setup_mapping(fuse_fh, file_offset, file_len, dax_offset, dax_flags).is_err() {
        crate::DAX_ALLOCATOR.lock().free(dax_offset, pages);
        return Err(());
    }

    // Map the DAX physical pages (DAX_WINDOW_BASE + dax_offset) into user virtual space.
    for i in 0..pages {
        let page_vaddr = vaddr.add(i * PAGE_SIZE);
        let dax_phys = DAX_WINDOW_BASE.add(dax_offset + i * PAGE_SIZE);
        if crate::KERNEL_PAGE_TABLE.map_2mb(page_vaddr, dax_phys).is_err() {
            // Rollback already-mapped DAX pages.
            for j in 0..i {
                let _ = crate::KERNEL_PAGE_TABLE.unmap_2mb(vaddr.add(j * PAGE_SIZE));
            }
            let _ = fs.remove_mapping(dax_offset, aligned_len as u64);
            crate::DAX_ALLOCATOR.lock().free(dax_offset, pages);
            return Err(());
        }
    }

    let vma = Vma {
        start: vaddr,
        end: VirtualAddr::new(vaddr.as_usize() + aligned_len),
        backing: MappingBacking::Dax {
            dax_offset,
            fuse_fh,
            fuse_nodeid,
            file_offset,
        },
    };
    if crate::VMA_TABLE.lock().insert(vma).is_err() {
        // VMA table full: roll back DAX page table mappings, host mapping, and slot allocation.
        for j in 0..pages {
            let _ = crate::KERNEL_PAGE_TABLE.unmap_2mb(vaddr.add(j * PAGE_SIZE));
        }
        let _ = fs.remove_mapping(dax_offset, aligned_len as u64);
        crate::DAX_ALLOCATOR.lock().free(dax_offset, pages);
        return Err(());
    }

    Ok((vaddr.as_u64() + sub_page_offset as u64) as SyscallResult)
}

/// Restore the MMAP_NEXT pointer if we reserved address space but need to roll back.
fn restore_mmap_next(saved_next: Option<VirtualAddr>) {
    if let Some(old) = saved_next {
        *crate::MMAP_NEXT.lock() = old;
    }
}

/// Unmap and free all 2 MB pages in [from..to).
fn rollback_pages(from: u64, to: u64) {
    let mut v = from;
    while v < to {
        if let Ok(paddr) = crate::KERNEL_PAGE_TABLE.unmap_2mb(VirtualAddr::new(v as usize)) {
            let _ = crate::PAGE_ALLOCATOR.free(paddr);
        }
        v += PAGE_SIZE as u64;
    }
}
