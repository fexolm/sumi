use sumi_abi::address::VirtualAddr;
use sumi_abi::arch::layout::PAGE_SIZE;

use crate::exec::{align_up_2mb, zero_page};
use crate::syscall::SyscallResult;
use crate::syscall::errno::*;

use super::{is_dax_page, replace_dax_with_private};

/// Handle anonymous MAP_FIXED at 4 KB granularity.
///
/// glibc's ld.so uses `MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED` at 4 KB
/// alignment to zero the BSS tail of each DSO segment (the byte range
/// [file_end, segment_end) that lies beyond the last file byte). The 2 MB
/// page covering that range was already mapped by a prior file-backed mmap;
/// we must zero only the requested range and not disturb the surrounding
/// file data.
pub(super) fn map_fixed_anon(addr: u64, len: usize) -> SyscallResult {
    if len == 0 {
        return EINVAL;
    }
    let end = match (addr as usize).checked_add(len) {
        Some(v) => v,
        None => return EINVAL,
    };

    // Walk the 2 MB pages covering [addr, end). For each page:
    //   - If unmapped, allocate a fresh zeroed page and map it.
    //   - If a DAX page, replace with a private copy first (COW).
    //   - Otherwise reuse the existing page.
    let aligned_start = crate::exec::align_down_2mb(addr);
    let aligned_end = align_up_2mb(end as u64);

    let mut page_addr = aligned_start;
    while page_addr < aligned_end {
        let va = VirtualAddr::new(page_addr as usize);
        match crate::KERNEL_PAGE_TABLE.lock().get_if_present(va) {
            Ok(Some(entry)) => {
                let paddr = entry.addr();
                if is_dax_page(paddr)
                    && let Err(e) = replace_dax_with_private(va, paddr)
                {
                    return e;
                }
                // Otherwise: reuse existing page (file-backed or anonymous).
            }
            Ok(None) => {
                let paddr = match crate::PAGE_ALLOCATOR.alloc(1) {
                    Ok(p) => p,
                    Err(_) => return ENOMEM,
                };
                zero_page(paddr);
                if crate::KERNEL_PAGE_TABLE.lock().map_2mb(va, paddr).is_err() {
                    let _ = crate::PAGE_ALLOCATOR.free(paddr);
                    return ENOMEM;
                }
            }
            Err(_) => return ENOMEM,
        }
        page_addr += PAGE_SIZE as u64;
    }

    // Zero only the user-requested byte range. We must NOT zero the full 2 MB
    // pages — they may contain file-backed code/data from a prior mmap that
    // must be preserved. Only the BSS tail [addr, addr+len) is zeroed.
    // SAFETY: pages covering [addr, addr+len) are now mapped in the kernel page
    // table. addr is 4 KB-aligned and validated. len is non-zero. In sumi's
    // single-address-space model user and kernel share one virtual address space,
    // so the pointer is valid for the kernel to write.
    unsafe {
        core::ptr::write_bytes(addr as *mut u8, 0, len);
    }

    addr as SyscallResult
}
