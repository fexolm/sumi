use sumi_abi::address::VirtualAddr;
use sumi_abi::arch::layout::{
    BASE_PAGE_SIZE, DAX_SLOT_COUNT, DAX_WINDOW_BASE, DAX_WINDOW_SIZE, PAGE_SIZE, USER_MMAP_BASE,
};

use crate::exec::{align_up_2mb, zero_page};
use crate::fs::FdKind;
use crate::memory::vma::{MappingBacking, Vma};
use crate::syscall::errno::*;
use crate::syscall::handlers::io::fs_transfer_chunked;
use crate::syscall::{SyscallArgs, SyscallResult};

mod memory_fixed_anon;
use memory_fixed_anon::map_fixed_anon;

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
    let mut mmap_guard = if flags & MAP_FIXED == 0 {
        Some(crate::MEMORY_STATE.lock())
    } else {
        None
    };

    if len == 0 {
        return EINVAL;
    }

    // File-backed MAP_FIXED: fast path for dynamic linker segment placement.
    // Accepts 4KB-aligned addresses (the reported AT_PAGESZ). Pages are managed
    // at 2MB granularity internally, but the requested 4KB-aligned address is
    // returned to the caller for sub-page data placement.
    if flags & MAP_FIXED != 0 && flags & MAP_ANONYMOUS == 0 && fd >= 0 {
        if !addr_hint.is_multiple_of(4096) {
            return EINVAL;
        }
        let (fuse_fh, _fuse_nodeid) = {
            let table = crate::FD_TABLE.lock();
            match table.get(fd as usize) {
                Some(d) => match d.kind {
                    FdKind::File {
                        fuse_fh,
                        fuse_nodeid,
                        ..
                    } => (fuse_fh, fuse_nodeid),
                    _ => return EBADF,
                },
                None => return EBADF,
            }
        };
        return map_fixed_file(addr_hint, len, fuse_fh, offset);
    }

    // Anonymous MAP_FIXED: fast path that zeros [addr, addr+len) inside existing
    // (or newly allocated) 2 MB pages. Accepts 4 KB-aligned addresses because
    // glibc's ld.so issues these to zero BSS tails at sub-2MB-page granularity.
    if flags & MAP_FIXED != 0 && flags & MAP_ANONYMOUS != 0 {
        if !(addr_hint as usize).is_multiple_of(4096) {
            return EINVAL;
        }
        return map_fixed_anon(addr_hint, len);
    }

    if flags & MAP_FIXED == 0
        && flags & MAP_ANONYMOUS != 0
        && crate::USER_MMAP_ALLOCATOR.can_allocate_small(len)
    {
        let (vaddr, aligned_len) = match crate::USER_MMAP_ALLOCATOR.alloc(len) {
            Ok(allocation) => allocation,
            Err(_) => return ENOMEM,
        };
        crate::VMA_TABLE.lock().insert(Vma {
            start: vaddr,
            end: VirtualAddr::new(vaddr.as_usize() + aligned_len),
            backing: MappingBacking::AnonymousSmall,
        });
        return vaddr.as_u64() as SyscallResult;
    }

    // For non-file-backed MAP_FIXED (non-anonymous fallthrough), require 2MB alignment.
    if flags & MAP_FIXED != 0 && !(addr_hint as usize).is_multiple_of(PAGE_SIZE) {
        return EINVAL;
    }

    // Sub-page file offset: how far into the first 2MB page the file data starts.
    // Must be computed before aligned_len so the extra partial-page is included.
    let sub_page_offset = if flags & MAP_ANONYMOUS == 0 {
        offset % PAGE_SIZE
    } else {
        0
    };
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
        let mmap_high = crate::USER_MMAP_ALLOCATOR
            .lowest_arena_base()
            .unwrap_or(USER_MMAP_BASE);
        let base = match crate::VMA_TABLE.lock().find_free_downward_aligned(
            mmap_high,
            aligned_len,
            PAGE_SIZE,
        ) {
            Some(base) => base,
            None => return ENOMEM,
        };
        if let Some(mem) = mmap_guard.as_mut() {
            mem.mmap_next = base;
        }
        (base, None)
    };

    // If MAP_FIXED, tear down any overlapping VMAs first.
    if flags & MAP_FIXED != 0 {
        let vaddr_end = VirtualAddr::new(vaddr.as_usize() + aligned_len);
        let removed = crate::VMA_TABLE.lock().remove_overlapping(vaddr, vaddr_end);
        for vma in removed {
            tear_down_vma(vma);
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
        crate::VMA_TABLE.lock().insert(vma);

        return vaddr.as_u64() as SyscallResult;
    }

    // File-backed mapping.
    let (fuse_fh, fuse_nodeid, file_size) = {
        let table = crate::FD_TABLE.lock();
        match table.get(fd as usize) {
            Some(d) => match d.kind {
                FdKind::File {
                    fuse_fh,
                    fuse_nodeid,
                    size,
                    ..
                } => (fuse_fh, fuse_nodeid, size),
                _ => return EBADF,
            },
            None => return EBADF,
        }
    };

    // Bytes of file content actually available from `file_page_offset`.
    // Used to (a) bound how many bytes private_copy_path reads via FUSE_READ,
    // and (b) decide whether DAX is safe — DAX must not extend past EOF or
    // the host's underlying mmap will SIGBUS on access.
    let file_content_len =
        (file_size.saturating_sub(file_page_offset as u64)).min(aligned_len as u64);

    // MAP_PRIVATE + PROT_WRITE: private copy (always alloc pages, FUSE_READ content).
    if flags & MAP_PRIVATE != 0 && prot & PROT_WRITE != 0 {
        return private_copy_path(
            vaddr,
            pages,
            saved_next,
            fuse_fh,
            fuse_nodeid,
            file_page_offset as u64,
            file_content_len,
            sub_page_offset,
        );
    }

    // MAP_PRIVATE read-only or MAP_SHARED: try DAX first, fall back to private copy.
    let dax_flags = if flags & MAP_SHARED != 0 {
        sumi_abi::fuse::FUSE_SETUPMAPPING_FLAG_READ | sumi_abi::fuse::FUSE_SETUPMAPPING_FLAG_WRITE
    } else {
        sumi_abi::fuse::FUSE_SETUPMAPPING_FLAG_READ
    };

    // DAX is only safe if every page in the mapping is fully covered by file
    // content. A partial-EOF page would let later accesses (including the
    // kernel's own DAX→private replace path during MAP_FIXED) read past the
    // host file and SIGBUS the host. Fall back to private_copy in that case.
    let dax_eligible = file_content_len == aligned_len as u64;

    if dax_eligible {
        match dax_path(
            vaddr,
            pages,
            fuse_fh,
            fuse_nodeid,
            file_page_offset as u64,
            file_content_len,
            dax_flags,
            sub_page_offset,
        ) {
            Ok(result) => return result,
            Err(_) => {
                // DAX window exhausted — fall through to private copy.
            }
        }
    }

    private_copy_path(
        vaddr,
        pages,
        saved_next,
        fuse_fh,
        fuse_nodeid,
        file_page_offset as u64,
        file_content_len,
        sub_page_offset,
    )
}

/// Handle file-backed MAP_FIXED: ensure pages are mapped, read file data.
/// This is the fast path for dynamic linker segment placement (per-segment MAP_FIXED
/// into an already-reserved address range). Does not modify VMA table — pages are
/// tracked by the reservation VMA from the initial mmap.
fn map_fixed_file(addr: u64, len: usize, fuse_fh: u64, offset: usize) -> SyscallResult {
    // Overflow check: addr + len must not wrap.
    let end_addr = match addr.checked_add(len as u64) {
        Some(e) => e,
        None => return EINVAL,
    };

    // Compute 2 MB-aligned page range covering [addr, addr + len).
    let aligned_start = crate::exec::align_down_2mb(addr);
    let aligned_end = align_up_2mb(end_addr);

    // Ensure all 2 MB pages in range are mapped.
    let mut page_addr = aligned_start;
    while page_addr < aligned_end {
        let va = VirtualAddr::new(page_addr as usize);
        let lookup = crate::KERNEL_PAGE_TABLE.lock().get_if_present(va);
        match lookup {
            Ok(Some(entry)) => {
                // Page exists. If it came from a prior DAX reservation, replace
                // it with a private copy before we overwrite part of it via
                // fs_transfer_chunked — DAX pages back the host file, and once
                // replaced any past-EOF tail in this page is gone (the
                // reservation already validated the page is fully within EOF).
                let paddr = entry.addr();
                if is_dax_page(paddr)
                    && let Err(e) = replace_dax_with_private(va, paddr)
                {
                    return e;
                }
            }
            Ok(None) => {
                // Page not mapped — allocate.
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

    // Read file data at the exact requested address.
    {
        let fs = crate::fs();
        let read_len = (len as u64).min(u32::MAX as u64) as u32;
        if fs_transfer_chunked(
            |off, pa, cnt| fs.read(fuse_fh, off, pa, cnt),
            offset as u64,
            addr,
            read_len,
        )
        .is_err()
        {
            return EIO;
        }
    }

    addr as SyscallResult
}

/// Check if a physical address falls within the DAX shared memory window.
pub(super) fn is_dax_page(paddr: sumi_abi::address::PhysicalAddr) -> bool {
    let addr = paddr.as_usize();
    let base = DAX_WINDOW_BASE.as_usize();
    addr >= base && addr < base + DAX_WINDOW_SIZE
}

/// Replace a DAX-mapped page with a private physical copy.
/// Allocates a new page, copies DAX content, remaps the virtual address.
pub(super) fn replace_dax_with_private(
    va: VirtualAddr,
    dax_paddr: sumi_abi::address::PhysicalAddr,
) -> Result<(), SyscallResult> {
    let new_paddr = crate::PAGE_ALLOCATOR.alloc(1).map_err(|_| ENOMEM)?;

    // SAFETY: Both addresses are valid mapped memory. Copy DAX content to the new page.
    let src = dax_paddr.to_virtual(&crate::KERNEL_DIRECT_MAP);
    let dst = new_paddr.to_virtual(&crate::KERNEL_DIRECT_MAP);
    unsafe {
        core::ptr::copy_nonoverlapping(src.as_ptr::<u8>(), dst.as_ptr::<u8>(), PAGE_SIZE);
    }

    // Replace mapping: unmap old, map new.
    crate::KERNEL_PAGE_TABLE
        .lock()
        .unmap_2mb(va)
        .map_err(|_| ENOMEM)?;
    if crate::KERNEL_PAGE_TABLE
        .lock()
        .map_2mb(va, new_paddr)
        .is_err()
    {
        // Try to restore the old DAX mapping to avoid leaving the page unmapped.
        let _ = crate::KERNEL_PAGE_TABLE.lock().map_2mb(va, dax_paddr);
        let _ = crate::PAGE_ALLOCATOR.free(new_paddr);
        return Err(ENOMEM);
    }

    Ok(())
}

const PROT_NONE: i32 = 0x0;

pub fn sys_mprotect(args: &SyscallArgs) -> SyscallResult {
    let addr = args.arg0 as usize;
    let len = args.arg1 as usize;
    let prot = args.arg2 as i32;

    if len == 0 {
        return 0;
    }

    // Small anonymous mmap slots are backed by shared 2 MiB arenas. Since we do
    // not provide sub-2MiB guard-page protection on that fast path, mprotect is
    // intentionally advisory there.
    if crate::USER_MMAP_ALLOCATOR.contains(VirtualAddr::new(addr & !(BASE_PAGE_SIZE - 1))) {
        return 0;
    }

    // Align range to 2MB page boundaries — we manage pages at 2MB granularity.
    let aligned_start = addr & !(PAGE_SIZE - 1);
    let aligned_end = match addr.checked_add(len) {
        Some(end) => (end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1),
        None => return EINVAL,
    };

    let pt = crate::KERNEL_PAGE_TABLE.lock();
    let mut vaddr = aligned_start;
    while vaddr < aligned_end {
        let va = VirtualAddr::new(vaddr);
        if prot == PROT_NONE {
            // Ignore errors — the page might not be mapped (guard page at start of range).
            let _ = pt.clear_present_2mb(va);
        } else {
            // Restore presence for any page that was hidden. Ignore errors similarly.
            let _ = pt.restore_present_2mb(va);
        }
        vaddr += PAGE_SIZE;
    }
    drop(pt);

    // Bump the TLB generation so all CPUs flush their TLBs at the next syscall return.
    crate::TLB_GENERATION.fetch_add(1, core::sync::atomic::Ordering::Release);

    0
}

pub fn sys_munmap(args: &SyscallArgs) -> SyscallResult {
    let addr = args.arg0 as usize;
    let len = args.arg1 as usize;

    let end = match addr.checked_add(len) {
        Some(e) => e,
        None => return EINVAL,
    };
    let small_start = addr & !(BASE_PAGE_SIZE - 1);
    let small_end = match end.checked_add(BASE_PAGE_SIZE - 1) {
        Some(v) => v & !(BASE_PAGE_SIZE - 1),
        None => return EINVAL,
    };

    let small_vma = {
        let table = crate::VMA_TABLE.lock();
        table
            .find(VirtualAddr::new(small_start))
            .and_then(|vma| match vma.backing {
                MappingBacking::AnonymousSmall => Some((vma.start, vma.end)),
                _ => None,
            })
    };

    if let Some((vma_start, vma_end)) = small_vma {
        let removed_vma = if small_start <= vma_start.as_usize() && small_end >= vma_end.as_usize()
        {
            crate::VMA_TABLE.lock().remove(vma_start)
        } else {
            None
        };
        if let Some(vma) = removed_vma {
            tear_down_vma(vma);
        }
        crate::TLB_GENERATION.fetch_add(1, core::sync::atomic::Ordering::Release);
        return 0;
    }

    let aligned_start = addr & !(PAGE_SIZE - 1);
    let aligned_end = match end.checked_add(PAGE_SIZE - 1) {
        Some(v) => v & !(PAGE_SIZE - 1),
        None => return EINVAL,
    };

    // Check if this overlaps a tracked VMA.
    let vma_info = {
        let table = crate::VMA_TABLE.lock();
        table
            .find(VirtualAddr::new(aligned_start))
            .map(|v| (v.start, v.end))
    };

    if let Some((vma_start, vma_end)) = vma_info {
        let req_start = VirtualAddr::new(aligned_start);
        let req_end = VirtualAddr::new(aligned_end);

        if req_start.as_usize() <= vma_start.as_usize() && req_end.as_usize() >= vma_end.as_usize()
        {
            // Full VMA unmap — remove and tear down.
            let removed_vma = crate::VMA_TABLE.lock().remove(vma_start);
            if let Some(vma) = removed_vma {
                tear_down_vma(vma);
            }
        } else {
            // Partial munmap: only unmap the requested 2MB pages.
            // Leave the VMA metadata intact — the pages outside the request
            // remain valid. This handles musl's pattern of unmapping trailing
            // gaps in library reservations.
            unmap_pages_in_range(aligned_start, aligned_end);
        }
        // Bump TLB generation so all CPUs flush their local TLBs at the next syscall return.
        crate::TLB_GENERATION.fetch_add(1, core::sync::atomic::Ordering::Release);
        return 0;
    }

    // No VMA found — fall back to anonymous unmap behavior.
    unmap_pages_in_range(aligned_start, aligned_end);

    // Bump TLB generation so all CPUs flush their local TLBs at the next syscall return.
    crate::TLB_GENERATION.fetch_add(1, core::sync::atomic::Ordering::Release);

    0
}

/// Unmap and free 2MB pages in [start, end), handling mixed DAX/private pages.
/// DAX pages are intentionally not freed here — they are released by VMA teardown
/// which knows the slot range to return to the DAX allocator.
fn unmap_pages_in_range(start: usize, end: usize) {
    let mut vaddr = start;
    while vaddr < end {
        if let Ok(paddr) = crate::KERNEL_PAGE_TABLE
            .lock()
            .unmap_2mb(VirtualAddr::new(vaddr))
            && !is_dax_page(paddr)
        {
            let _ = crate::PAGE_ALLOCATOR.free(paddr);
        }
        vaddr += PAGE_SIZE;
    }
}

pub fn sys_brk(args: &SyscallArgs) -> SyscallResult {
    let requested = args.arg0;
    let mut mem = crate::MEMORY_STATE.lock();

    if requested == 0 || (requested as usize) < mem.brk_base.as_usize() {
        return mem.brk_current.as_u64() as SyscallResult;
    }

    let old_end = align_up_2mb(mem.brk_current.as_u64());
    let new_end = align_up_2mb(requested);

    if new_end > old_end {
        let mut vaddr = old_end;
        while vaddr < new_end {
            let paddr = match crate::PAGE_ALLOCATOR.alloc(1) {
                Ok(p) => p,
                Err(_) => {
                    rollback_pages(old_end, vaddr);
                    return mem.brk_current.as_u64() as SyscallResult;
                }
            };
            zero_page(paddr);
            if crate::KERNEL_PAGE_TABLE
                .lock()
                .map_2mb(VirtualAddr::new(vaddr as usize), paddr)
                .is_err()
            {
                let _ = crate::PAGE_ALLOCATOR.free(paddr);
                rollback_pages(old_end, vaddr);
                return mem.brk_current.as_u64() as SyscallResult;
            }
            vaddr += PAGE_SIZE as u64;
        }
    } else if new_end < old_end {
        let mut vaddr = new_end;
        while vaddr < old_end {
            if let Ok(paddr) = crate::KERNEL_PAGE_TABLE
                .lock()
                .unmap_2mb(VirtualAddr::new(vaddr as usize))
            {
                let _ = crate::PAGE_ALLOCATOR.free(paddr);
            }
            vaddr += PAGE_SIZE as u64;
        }
    }

    mem.brk_current = VirtualAddr::new(requested as usize);
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
/// For DAX-backed VMAs, handles mixed pages: some may have been replaced with
/// private copies by `replace_dax_with_private` (COW from MAP_FIXED).
fn tear_down_vma(vma: Vma) {
    let aligned_start = vma.start.as_usize();
    let aligned_end = vma.end.as_usize();

    match vma.backing {
        MappingBacking::AnonymousSmall => {
            crate::USER_MMAP_ALLOCATOR.free(
                vma.start,
                vma.end.as_usize().saturating_sub(vma.start.as_usize()),
            );
        }
        MappingBacking::Anonymous | MappingBacking::PrivateFile { .. } => {
            // Unmap and free physical pages.
            let mut vaddr = aligned_start;
            while vaddr < aligned_end {
                if let Ok(paddr) = crate::KERNEL_PAGE_TABLE
                    .lock()
                    .unmap_2mb(VirtualAddr::new(vaddr))
                {
                    let _ = crate::PAGE_ALLOCATOR.free(paddr);
                }
                vaddr += PAGE_SIZE;
            }
        }
        MappingBacking::Dax { dax_offset, .. } => {
            // Unmap all pages. Some may be original DAX, some replaced private copies.
            // DAX pages: don't free individually — the slot range is freed below.
            let mut vaddr = aligned_start;
            while vaddr < aligned_end {
                if let Ok(paddr) = crate::KERNEL_PAGE_TABLE
                    .lock()
                    .unmap_2mb(VirtualAddr::new(vaddr))
                    && !is_dax_page(paddr)
                {
                    // This page was replaced with a private copy — free it.
                    let _ = crate::PAGE_ALLOCATOR.free(paddr);
                }
                vaddr += PAGE_SIZE;
            }
            // Ask the host to unmap from the DAX window.
            let fs = crate::fs();
            let len = (aligned_end - aligned_start) as u64;
            let _ = fs.remove_mapping(dax_offset, len);
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
                    if let Ok(p) = crate::KERNEL_PAGE_TABLE
                        .lock()
                        .unmap_2mb(vaddr.add(j * PAGE_SIZE))
                    {
                        let _ = crate::PAGE_ALLOCATOR.free(p);
                    }
                }
                return Err(ENOMEM);
            }
        };
        zero_page(paddr);
        if crate::KERNEL_PAGE_TABLE
            .lock()
            .map_2mb(page_vaddr, paddr)
            .is_err()
        {
            let _ = crate::PAGE_ALLOCATOR.free(paddr);
            for j in 0..i {
                if let Ok(p) = crate::KERNEL_PAGE_TABLE
                    .lock()
                    .unmap_2mb(vaddr.add(j * PAGE_SIZE))
                {
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
    {
        let fs = crate::fs();
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
        backing: MappingBacking::PrivateFile {
            fuse_fh,
            fuse_nodeid,
        },
    };
    crate::VMA_TABLE.lock().insert(vma);

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

    let fs = crate::fs();

    if fs
        .setup_mapping(fuse_fh, file_offset, file_len, dax_offset, dax_flags)
        .is_err()
    {
        crate::DAX_ALLOCATOR.lock().free(dax_offset, pages);
        return Err(());
    }

    // Map the DAX physical pages (DAX_WINDOW_BASE + dax_offset) into user virtual space.
    for i in 0..pages {
        let page_vaddr = vaddr.add(i * PAGE_SIZE);
        let dax_phys = DAX_WINDOW_BASE.add(dax_offset + i * PAGE_SIZE);
        if crate::KERNEL_PAGE_TABLE
            .lock()
            .map_2mb(page_vaddr, dax_phys)
            .is_err()
        {
            // Rollback already-mapped DAX pages.
            for j in 0..i {
                let _ = crate::KERNEL_PAGE_TABLE
                    .lock()
                    .unmap_2mb(vaddr.add(j * PAGE_SIZE));
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
    crate::VMA_TABLE.lock().insert(vma);

    Ok((vaddr.as_u64() + sub_page_offset as u64) as SyscallResult)
}

/// Restore the mmap_next pointer if we reserved address space but need to roll back.
fn restore_mmap_next(saved_next: Option<VirtualAddr>) {
    if let Some(old) = saved_next {
        crate::MEMORY_STATE.lock().mmap_next = old;
    }
}

/// Unmap and free all 2 MB pages in [from..to).
fn rollback_pages(from: u64, to: u64) {
    let mut v = from;
    while v < to {
        if let Ok(paddr) = crate::KERNEL_PAGE_TABLE
            .lock()
            .unmap_2mb(VirtualAddr::new(v as usize))
        {
            let _ = crate::PAGE_ALLOCATOR.free(paddr);
        }
        v += PAGE_SIZE as u64;
    }
}
