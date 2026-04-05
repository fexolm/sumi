use core::ptr::copy_nonoverlapping;

use crate::memory::alloc::kmalloc::KernelAllocator;
use crate::memory::errors::{MemoryError, Result};
use sumi_abi::{
    address::{DirectMap, PhysicalAddr, VirtualAddr},
    arch::{
        address::{get_pd_index, get_pdpt_index, get_pml4_index},
        layout::{DIRECT_MAP_OFFSET, PAGE_TABLE_ENTRIES, PAGE_TABLE_SIZE},
    },
};

const PRESENT: usize = 1 << 0;
const WRITABLE: usize = 1 << 1;
const USER_ACCESSIBLE: usize = 1 << 2;
const HUGE_PAGE: usize = 1 << 7;
const ADDR_MASK: usize = 0x000F_FFFF_FFFF_F000;
const USER_PML4_LIMIT: usize = get_pml4_index(DIRECT_MAP_OFFSET);

#[derive(Clone, Copy)]
pub struct PageTableEntry(usize);

impl PageTableEntry {
    pub fn set_table(&mut self, addr: PhysicalAddr) {
        self.0 = addr.as_usize() | PRESENT | WRITABLE | USER_ACCESSIBLE;
    }

    pub fn set_paddr(&mut self, addr: PhysicalAddr) {
        self.0 = addr.as_usize() | PRESENT | WRITABLE | USER_ACCESSIBLE | HUGE_PAGE;
    }

    pub fn is_present(&self) -> bool {
        (self.0 & PRESENT) != 0
    }

    pub fn addr(&self) -> PhysicalAddr {
        PhysicalAddr::new(self.0 & ADDR_MASK)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PageTableLevel {
    Pml4,
    Pdpt,
    Pd,
}

impl PageTableLevel {
    fn next(self) -> Option<Self> {
        match self {
            Self::Pml4 => Some(Self::Pdpt),
            Self::Pdpt => Some(Self::Pd),
            Self::Pd => None,
        }
    }
}

#[repr(C, align(4096))]
struct PageTable {
    entries: [PageTableEntry; PAGE_TABLE_ENTRIES],
}

impl PageTable {
    pub unsafe fn from_paddr_mut(paddr: PhysicalAddr, map: &impl DirectMap) -> &'static mut Self {
        unsafe { paddr.to_virtual(map).as_ref_mut::<Self>() }
    }

    pub fn get<DM: DirectMap>(
        &mut self,
        vaddr: VirtualAddr,
        kalloc: &KernelAllocator<DM>,
    ) -> Result<&mut PageTableEntry> {
        self.get_level(vaddr, PageTableLevel::Pml4, kalloc)
    }

    pub fn get_if_present<DM: DirectMap>(
        &self,
        vaddr: VirtualAddr,
        kalloc: &KernelAllocator<DM>,
    ) -> Result<Option<PageTableEntry>> {
        self.get_present_level(vaddr, PageTableLevel::Pml4, kalloc.direct_map())
    }

    /// Walk page table without allocating. Returns mutable ref to the PD entry if present.
    fn get_mut_if_present(
        &mut self,
        vaddr: VirtualAddr,
        map: &impl DirectMap,
    ) -> Option<&mut PageTableEntry> {
        self.get_mut_present_level(vaddr, PageTableLevel::Pml4, map)
    }

    fn get_mut_present_level(
        &mut self,
        vaddr: VirtualAddr,
        level: PageTableLevel,
        map: &impl DirectMap,
    ) -> Option<&mut PageTableEntry> {
        if level == PageTableLevel::Pd {
            let entry = &mut self.entries[index_for(level, vaddr)];
            return if entry.is_present() {
                Some(entry)
            } else {
                None
            };
        }

        let entry = &self.entries[index_for(level, vaddr)];
        if !entry.is_present() {
            return None;
        }

        let next = level.next()?;
        let child_addr = entry.addr();
        let child = unsafe { Self::from_paddr_mut(child_addr, map) };
        child.get_mut_present_level(vaddr, next, map)
    }

    fn get_level<DM: DirectMap>(
        &mut self,
        vaddr: VirtualAddr,
        level: PageTableLevel,
        kalloc: &KernelAllocator<DM>,
    ) -> Result<&mut PageTableEntry> {
        if level == PageTableLevel::Pd {
            return Ok(&mut self.entries[index_for(level, vaddr)]);
        }

        let entry = &mut self.entries[index_for(level, vaddr)];

        if !entry.is_present() {
            entry.set_table(kalloc.calloc(PAGE_TABLE_SIZE)?);
        }

        let Some(next) = level.next() else {
            return Err(MemoryError::VirtualToPhysical {
                addr: vaddr.as_usize(),
            });
        };

        let child = unsafe { Self::from_paddr_mut(entry.addr(), kalloc.direct_map()) };
        child.get_level(vaddr, next, kalloc)
    }

    fn get_present_level(
        &self,
        vaddr: VirtualAddr,
        level: PageTableLevel,
        map: &impl DirectMap,
    ) -> Result<Option<PageTableEntry>> {
        let entry = self.entries[index_for(level, vaddr)];

        if !entry.is_present() {
            return Ok(None);
        }

        if level == PageTableLevel::Pd {
            return Ok(Some(entry));
        }

        let Some(next) = level.next() else {
            return Ok(None);
        };

        let child = unsafe { Self::from_paddr_mut(entry.addr(), map) };
        child.get_present_level(vaddr, next, map)
    }

    pub fn free<DM: DirectMap>(&mut self, kalloc: &KernelAllocator<DM>) -> Result<()> {
        self.free_level(PageTableLevel::Pml4, kalloc)
    }

    fn free_level<DM: DirectMap>(
        &mut self,
        level: PageTableLevel,
        kalloc: &KernelAllocator<DM>,
    ) -> Result<()> {
        let end = if level == PageTableLevel::Pml4 {
            USER_PML4_LIMIT
        } else {
            PAGE_TABLE_ENTRIES
        };

        if let Some(next) = level.next() {
            for i in 0..end {
                let entry = self.entries[i];
                if entry.is_present() {
                    let child = unsafe { Self::from_paddr_mut(entry.addr(), kalloc.direct_map()) };
                    child.free_level(next, kalloc)?;
                }
            }
        } else {
            for i in 0..end {
                let entry = self.entries[i];
                if entry.is_present() {
                    kalloc.free(entry.addr())?;
                }
            }
        }

        kalloc.free(to_physical_checked(self.self_vaddr(), kalloc.direct_map())?)?;
        Ok(())
    }

    fn self_vaddr(&self) -> VirtualAddr {
        VirtualAddr::new(self as *const Self as usize)
    }
}

fn index_for(level: PageTableLevel, vaddr: VirtualAddr) -> usize {
    match level {
        PageTableLevel::Pml4 => get_pml4_index(vaddr),
        PageTableLevel::Pdpt => get_pdpt_index(vaddr),
        PageTableLevel::Pd => get_pd_index(vaddr),
    }
}

pub struct RootPageTable<'i, DM: DirectMap> {
    kalloc: &'i KernelAllocator<'i, DM>,
    addr: PhysicalAddr,
}

impl<'i, DM: DirectMap> RootPageTable<'i, DM> {
    pub fn new(
        kernel_page_table: &'i RootPageTable<'i, DM>,
        kalloc: &'i KernelAllocator<'i, DM>,
    ) -> Result<Self> {
        let addr = kalloc.calloc(PAGE_TABLE_SIZE)?;
        let map = kalloc.direct_map();

        unsafe {
            copy_nonoverlapping(
                kernel_page_table
                    .addr
                    .to_virtual(map)
                    .as_ptr::<usize>()
                    .add(USER_PML4_LIMIT),
                addr.to_virtual(map).as_ptr::<usize>().add(USER_PML4_LIMIT),
                PAGE_TABLE_ENTRIES - USER_PML4_LIMIT,
            );
        }

        unsafe { Ok(Self::from_paddr(addr, kalloc)) }
    }

    pub const unsafe fn from_paddr(
        addr: PhysicalAddr,
        kalloc: &'i KernelAllocator<'i, DM>,
    ) -> Self {
        Self { kalloc, addr }
    }

    pub fn addr(&self) -> PhysicalAddr {
        self.addr
    }

    pub fn get(&self, addr: VirtualAddr) -> Result<&mut PageTableEntry> {
        self.get_pml4().get(addr, self.kalloc)
    }

    pub fn get_if_present(&self, addr: VirtualAddr) -> Result<Option<PageTableEntry>> {
        self.get_pml4().get_if_present(addr, self.kalloc)
    }

    /// Map a 2 MB huge page: vaddr -> paddr.
    /// Allocates intermediate PML4E/PDPTE on demand.
    /// Returns error if a mapping already exists at vaddr.
    pub fn map_2mb(&self, vaddr: VirtualAddr, paddr: PhysicalAddr) -> Result<()> {
        let entry = self.get_pml4().get(vaddr, self.kalloc)?;
        if entry.is_present() {
            return Err(MemoryError::AlreadyMapped {
                addr: vaddr.as_usize(),
            });
        }
        entry.set_paddr(paddr);
        Ok(())
    }

    /// Unmap the 2 MB page at vaddr. Returns the physical address that was mapped.
    /// Returns error if not mapped. Issues INVLPG to invalidate the TLB entry.
    pub fn unmap_2mb(&self, vaddr: VirtualAddr) -> Result<PhysicalAddr> {
        let entry = self
            .get_pml4()
            .get_mut_if_present(vaddr, self.kalloc.direct_map())
            .ok_or(MemoryError::NotMapped {
                addr: vaddr.as_usize(),
            })?;
        let paddr = entry.addr();
        entry.0 = 0;
        // SAFETY: Invalidating a TLB entry for a just-unmapped address is always safe.
        #[cfg(not(test))]
        unsafe {
            core::arch::asm!("invlpg [{}]", in(reg) vaddr.as_usize(), options(nostack));
        }
        Ok(paddr)
    }

    fn get_pml4(&self) -> &mut PageTable {
        // SAFETY: The PML4 page table at self.addr is mapped via the direct map.
        // Mutation goes through direct-map raw pointers, not through &self.
        unsafe { PageTable::from_paddr_mut(self.addr, self.kalloc.direct_map()) }
    }
}

impl<DM: DirectMap> Drop for RootPageTable<'_, DM> {
    fn drop(&mut self) {
        self.get_pml4().free(self.kalloc).unwrap();
    }
}

fn to_physical_checked(vaddr: VirtualAddr, map: &impl DirectMap) -> Result<PhysicalAddr> {
    vaddr
        .to_physical(map)
        .ok_or(MemoryError::VirtualToPhysical {
            addr: vaddr.as_usize(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::alloc::{kmalloc::KernelAllocator, palloc::PageAllocator};
    use sumi_abi::arch::layout::KERNEL_STACK;

    // PAGE_SIZE is 2 MB.
    use sumi_abi::arch::layout::PAGE_SIZE;

    /// PAGE_TABLE_ALIGN must be a power of two and at least as large as the
    /// `align(4096)` on `PageTable`, so that virtual addresses produced by
    /// `p2v` satisfy the alignment assertion in `VirtualAddr::as_ref_mut`.
    const PAGE_TABLE_ALIGN: usize = 4096;

    /// Backs physical addresses with a 4096-byte-aligned heap buffer.
    /// Physical address KERNEL_STACK maps to buf[0], so subsequent allocations
    /// at KERNEL_STACK + k*PAGE_TABLE_SIZE are also PAGE_TABLE_ALIGN-aligned.
    struct TestDirectMap {
        phys_base: usize,
        _buf: std::vec::Vec<u8>,
        /// Actual start of aligned region within the raw allocation.
        aligned_ptr: *mut u8,
        len: usize,
    }

    impl TestDirectMap {
        fn new(pages: usize) -> Self {
            let len = pages * PAGE_SIZE;
            // Over-allocate by PAGE_TABLE_ALIGN so we can align the start.
            let raw_len = len + PAGE_TABLE_ALIGN;
            let buf = vec![0u8; raw_len];
            let raw = buf.as_ptr() as usize;
            let aligned = (raw + PAGE_TABLE_ALIGN - 1) & !(PAGE_TABLE_ALIGN - 1);
            let aligned_ptr = aligned as *mut u8;
            Self {
                phys_base: KERNEL_STACK.as_usize(),
                _buf: buf,
                aligned_ptr,
                len,
            }
        }
    }

    // SAFETY: TestDirectMap owns the buffer; aligned_ptr is valid for `len` bytes
    // inside `buf` which is never moved while the struct is alive.
    unsafe impl Send for TestDirectMap {}
    unsafe impl Sync for TestDirectMap {}

    impl sumi_abi::address::DirectMap for TestDirectMap {
        fn p2v(&self, paddr: PhysicalAddr) -> VirtualAddr {
            VirtualAddr::new(self.aligned_ptr as usize + (paddr.as_usize() - self.phys_base))
        }

        fn v2p(&self, vaddr: VirtualAddr) -> Option<PhysicalAddr> {
            let base = self.aligned_ptr as usize;
            if vaddr.as_usize() < base || vaddr.as_usize() >= base + self.len {
                return None;
            }
            Some(PhysicalAddr::new(
                vaddr.as_usize() - base + self.phys_base,
            ))
        }
    }

    /// Creates a TestDirectMap, PageAllocator, and KernelAllocator.
    /// Returns boxed values; the static references into them are valid as long
    /// as the boxes are alive (caller must keep the boxes alive).
    fn make_alloc(
        pages: usize,
    ) -> (
        Box<TestDirectMap>,
        Box<PageAllocator>,
        Box<KernelAllocator<'static, TestDirectMap>>,
    ) {
        let dm = Box::new(TestDirectMap::new(pages));
        let pa = Box::new(PageAllocator::new());
        // SAFETY: The boxes live for the duration of each test; the raw-pointer
        // cast to 'static is safe because the caller keeps the boxes alive.
        let dm_ref: &'static TestDirectMap = unsafe { &*(dm.as_ref() as *const _) };
        let pa_ref: &'static PageAllocator = unsafe { &*(pa.as_ref() as *const _) };
        (dm, pa, Box::new(KernelAllocator::new(dm_ref, pa_ref)))
    }

    /// Allocates a PML4 page through `kalloc` and wraps it in a `RootPageTable`.
    /// The PML4 is kalloc-tracked, so Drop can free it correctly.
    fn make_page_table<'a>(
        kalloc: &'a KernelAllocator<'a, TestDirectMap>,
    ) -> RootPageTable<'a, TestDirectMap> {
        let pml4_addr = kalloc.calloc(PAGE_TABLE_SIZE).expect("alloc PML4");
        // SAFETY: pml4_addr is zeroed memory allocated by kalloc and valid for the
        // lifetime of kalloc. It is tracked in the allocator so Drop can free it.
        unsafe { RootPageTable::from_paddr(pml4_addr, kalloc) }
    }

    /// Allocates a "data page" through kalloc so that the page-table Drop can
    /// free the PD entry without hitting UnknownAllocation.
    fn alloc_data_page<'a>(
        kalloc: &'a KernelAllocator<'a, TestDirectMap>,
    ) -> PhysicalAddr {
        kalloc.calloc(PAGE_TABLE_SIZE).expect("alloc data page")
    }

    // A 2 MB-aligned virtual address well within user space (PML4 index 0).
    const VADDR_A: VirtualAddr = VirtualAddr::new(0x0000_0000_0020_0000); // 2 MB
    const VADDR_B: VirtualAddr = VirtualAddr::new(0x0000_0000_0040_0000); // 4 MB — same PDPT/PD slot column, different PD entry
    const VADDR_C: VirtualAddr = VirtualAddr::new(0x0000_0040_0000_0000); // different PDPT entry

    #[test]
    fn map_2mb_succeeds() {
        let (_dm, _pa, kalloc) = make_alloc(16);
        let pt = make_page_table(&kalloc);
        let pdata = alloc_data_page(&kalloc);

        pt.map_2mb(VADDR_A, pdata).expect("map should succeed");

        let entry = pt.get_if_present(VADDR_A).expect("get_if_present");
        assert!(entry.is_some(), "mapping should be present after map_2mb");
        assert_eq!(
            entry.unwrap().addr(),
            pdata,
            "entry should point to the mapped physical address"
        );
    }

    #[test]
    fn double_map_returns_already_mapped() {
        let (_dm, _pa, kalloc) = make_alloc(16);
        let pt = make_page_table(&kalloc);
        let pdata = alloc_data_page(&kalloc);

        pt.map_2mb(VADDR_A, pdata).expect("first map should succeed");
        let result = pt.map_2mb(VADDR_A, pdata);
        assert!(
            matches!(result, Err(MemoryError::AlreadyMapped { addr }) if addr == VADDR_A.as_usize()),
            "second map to the same vaddr must return AlreadyMapped, got {result:?}"
        );
    }

    #[test]
    fn unmap_2mb_returns_correct_physical_address() {
        let (_dm, _pa, kalloc) = make_alloc(16);
        let pt = make_page_table(&kalloc);
        let pdata = alloc_data_page(&kalloc);

        pt.map_2mb(VADDR_A, pdata).expect("map should succeed");
        let returned = pt.unmap_2mb(VADDR_A).expect("unmap should succeed");
        assert_eq!(
            returned, pdata,
            "unmap_2mb should return the physical address that was mapped"
        );
        // Free the data page ourselves — unmap_2mb does not free it.
        kalloc.free(pdata).expect("free data page after unmap");
    }

    #[test]
    fn unmap_unmapped_address_returns_not_mapped() {
        let (_dm, _pa, kalloc) = make_alloc(16);
        let pt = make_page_table(&kalloc);

        let result = pt.unmap_2mb(VADDR_A);
        assert!(
            matches!(result, Err(MemoryError::NotMapped { addr }) if addr == VADDR_A.as_usize()),
            "unmap of never-mapped address must return NotMapped, got {result:?}"
        );
    }

    #[test]
    fn unmap_after_unmap_returns_not_mapped() {
        let (_dm, _pa, kalloc) = make_alloc(16);
        let pt = make_page_table(&kalloc);
        let pdata = alloc_data_page(&kalloc);

        pt.map_2mb(VADDR_A, pdata).unwrap();
        let first = pt.unmap_2mb(VADDR_A).unwrap();
        kalloc.free(first).unwrap();

        let result = pt.unmap_2mb(VADDR_A);
        assert!(
            matches!(result, Err(MemoryError::NotMapped { .. })),
            "second unmap of the same address must return NotMapped"
        );
    }

    #[test]
    fn map_multiple_distinct_addresses() {
        let (_dm, _pa, kalloc) = make_alloc(32);
        let pt = make_page_table(&kalloc);
        let pdata_a = alloc_data_page(&kalloc);
        let pdata_b = alloc_data_page(&kalloc);
        let pdata_c = alloc_data_page(&kalloc);

        pt.map_2mb(VADDR_A, pdata_a).expect("map A");
        pt.map_2mb(VADDR_B, pdata_b).expect("map B");
        pt.map_2mb(VADDR_C, pdata_c).expect("map C");

        assert_eq!(
            pt.get_if_present(VADDR_A).unwrap().unwrap().addr(),
            pdata_a,
            "VADDR_A should map to pdata_a"
        );
        assert_eq!(
            pt.get_if_present(VADDR_B).unwrap().unwrap().addr(),
            pdata_b,
            "VADDR_B should map to pdata_b"
        );
        assert_eq!(
            pt.get_if_present(VADDR_C).unwrap().unwrap().addr(),
            pdata_c,
            "VADDR_C should map to pdata_c"
        );
    }

    #[test]
    fn map_unmap_remap_succeeds() {
        let (_dm, _pa, kalloc) = make_alloc(16);
        let pt = make_page_table(&kalloc);
        let pdata1 = alloc_data_page(&kalloc);
        let pdata2 = alloc_data_page(&kalloc);

        pt.map_2mb(VADDR_A, pdata1).expect("first map");

        let returned = pt.unmap_2mb(VADDR_A).expect("unmap");
        assert_eq!(returned, pdata1);
        kalloc.free(returned).expect("free first data page");

        pt.map_2mb(VADDR_A, pdata2).expect("remap after unmap should succeed");
        assert_eq!(
            pt.get_if_present(VADDR_A).unwrap().unwrap().addr(),
            pdata2,
            "remap should install the new physical address"
        );
    }

    #[test]
    fn get_if_present_returns_none_for_unmapped() {
        let (_dm, _pa, kalloc) = make_alloc(16);
        let pt = make_page_table(&kalloc);

        let result = pt.get_if_present(VADDR_A).expect("get_if_present");
        assert!(
            result.is_none(),
            "get_if_present on never-mapped address must return None"
        );
    }

    #[test]
    fn independent_addresses_do_not_alias() {
        let (_dm, _pa, kalloc) = make_alloc(32);
        let pt = make_page_table(&kalloc);
        let pdata_a = alloc_data_page(&kalloc);
        let pdata_b = alloc_data_page(&kalloc);

        pt.map_2mb(VADDR_A, pdata_a).expect("map A");
        pt.map_2mb(VADDR_B, pdata_b).expect("map B");

        let returned_a = pt.unmap_2mb(VADDR_A).expect("unmap A");
        kalloc.free(returned_a).expect("free A data page");

        // B must still be intact after A is unmapped.
        assert_eq!(
            pt.get_if_present(VADDR_B).unwrap().unwrap().addr(),
            pdata_b,
            "unmapping VADDR_A must not affect VADDR_B"
        );
        assert!(
            pt.get_if_present(VADDR_A).unwrap().is_none(),
            "VADDR_A must be absent after unmap"
        );
    }
}
