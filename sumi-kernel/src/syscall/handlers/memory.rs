use sumi_abi::address::VirtualAddr;
use sumi_abi::arch::layout::PAGE_SIZE;

use crate::exec::{align_up_2mb, zero_page};
use crate::syscall::errno::*;
use crate::syscall::{SyscallArgs, SyscallResult};

const MAP_ANONYMOUS: i32 = 0x20;
const MAP_FIXED: i32 = 0x10;

pub fn sys_mmap(args: &SyscallArgs) -> SyscallResult {
    let addr_hint = args.arg0;
    let len = args.arg1 as usize;
    let _prot = args.arg2 as i32;
    let flags = args.arg3 as i32;
    let _fd = args.arg4 as i32;
    let _offset = args.arg5;

    if flags & MAP_ANONYMOUS == 0 {
        return ENOSYS; // file-backed not supported
    }

    if len == 0 {
        return EINVAL;
    }

    let aligned_len = align_up_2mb(len as u64) as usize;
    let pages = aligned_len / PAGE_SIZE;

    // Determine virtual address. For non-fixed mappings, reserve the region
    // from MMAP_NEXT but remember the old value so we can restore on failure.
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

    // Allocate and map pages; on failure, unmap+free everything and restore MMAP_NEXT.
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
                if let Some(old) = saved_next {
                    *crate::MMAP_NEXT.lock() = old;
                }
                return ENOMEM;
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
            if let Some(old) = saved_next {
                *crate::MMAP_NEXT.lock() = old;
            }
            return ENOMEM;
        }
    }

    vaddr.as_u64() as SyscallResult
}

pub fn sys_mprotect(_args: &SyscallArgs) -> SyscallResult {
    // No-op: 2 MB pages are all RWX in ring 0
    0
}

pub fn sys_munmap(args: &SyscallArgs) -> SyscallResult {
    let addr = args.arg0 as usize;
    let len = args.arg1 as usize;

    let aligned_start = addr & !(PAGE_SIZE - 1);
    let aligned_end = (addr + len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);

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
        // Grow: allocate and map new pages. On failure, rollback all pages
        // added in this call so the mapping stays consistent with BRK_CURRENT.
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
        // Shrink: unmap and free pages
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
