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

    /// True if this entry is a 2 MB huge-page leaf (as opposed to a pointer to
    /// a lower-level table, or an empty slot). A hidden-but-real mapping made by
    /// `clear_present` keeps this bit set even with PRESENT cleared.
    pub fn is_huge_page(&self) -> bool {
        (self.0 & HUGE_PAGE) != 0
    }

    pub fn addr(&self) -> PhysicalAddr {
        PhysicalAddr::new(self.0 & ADDR_MASK)
    }

    /// Read the raw entry value (all bits).
    pub fn raw(&self) -> u64 {
        self.0 as u64
    }

    /// Clear the PRESENT bit while preserving all other bits (physical address, flags).
    /// Used to implement PROT_NONE: the entry stays in the page table so
    /// `restore_present` can re-enable the mapping without re-walking.
    pub fn clear_present(&mut self) {
        self.0 &= !PRESENT;
    }

    /// Set the PRESENT bit. Used to restore a previously cleared mapping.
    pub fn restore_present(&mut self) {
        self.0 |= PRESENT;
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

    /// Walk page table without allocating. Returns mutable ref to the PD entry regardless
    /// of whether the PRESENT bit is set. Used to restore a previously cleared mapping.
    /// Returns None only when the PML4/PDPT entry itself is absent (page was never mapped).
    fn get_mut_unconditional(
        &mut self,
        vaddr: VirtualAddr,
        map: &impl DirectMap,
    ) -> Option<&mut PageTableEntry> {
        self.get_mut_unconditional_level(vaddr, PageTableLevel::Pml4, map)
    }

    fn get_mut_unconditional_level(
        &mut self,
        vaddr: VirtualAddr,
        level: PageTableLevel,
        map: &impl DirectMap,
    ) -> Option<&mut PageTableEntry> {
        if level == PageTableLevel::Pd {
            // Return the PD entry regardless of PRESENT bit.
            return Some(&mut self.entries[index_for(level, vaddr)]);
        }

        // For PML4 and PDPT we still need the intermediate entries to exist.
        let entry = &self.entries[index_for(level, vaddr)];
        if !entry.is_present() {
            return None;
        }

        let next = level.next()?;
        let child_addr = entry.addr();
        let child = unsafe { Self::from_paddr_mut(child_addr, map) };
        child.get_mut_unconditional_level(vaddr, next, map)
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
                    kalloc.free(entry.addr());
                }
            }
        }

        kalloc.free(to_physical_checked(self.self_vaddr(), kalloc.direct_map())?);
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

    /// # Safety
    /// `addr` must point to a valid, initialized PML4 page table.
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

    /// Clear the PRESENT bit of the 2MB page at `vaddr` without removing the entry.
    /// The physical address and other flags are preserved so `restore_present_2mb` can
    /// re-enable the mapping. Returns error if the page was never mapped.
    pub fn clear_present_2mb(&self, vaddr: VirtualAddr) -> Result<()> {
        let entry = self
            .get_pml4()
            .get_mut_if_present(vaddr, self.kalloc.direct_map())
            .ok_or(MemoryError::NotMapped {
                addr: vaddr.as_usize(),
            })?;
        entry.clear_present();
        Ok(())
    }

    /// Restore the PRESENT bit of the 2MB page at `vaddr` that was previously cleared
    /// by `clear_present_2mb`. Returns error if the intermediate tables are absent
    /// (i.e. the page was never mapped at all, not merely hidden).
    pub fn restore_present_2mb(&self, vaddr: VirtualAddr) -> Result<()> {
        let entry = self
            .get_pml4()
            .get_mut_unconditional(vaddr, self.kalloc.direct_map())
            .ok_or(MemoryError::NotMapped {
                addr: vaddr.as_usize(),
            })?;
        // `get_mut_unconditional` returns the PD slot even when it is empty
        // (the intermediate tables exist but this 2 MB range was never mapped —
        // e.g. a guard page or a hole in a partially-mapped VMA). Blindly
        // setting PRESENT on a zero slot would create a present non-huge PDE
        // pointing at physical address 0, which the CPU would walk as a page
        // table. Only restore a slot that is a real (hidden) huge-page leaf.
        if !entry.is_huge_page() {
            return Err(MemoryError::NotMapped {
                addr: vaddr.as_usize(),
            });
        }
        entry.restore_present();
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

    #[allow(clippy::mut_from_ref)] // Mutation goes through direct-map raw pointers, not &self
    fn get_pml4(&self) -> &mut PageTable {
        // SAFETY: The PML4 page table at self.addr is mapped via the direct map.
        // Mutation goes through direct-map raw pointers, not through &self.
        unsafe { PageTable::from_paddr_mut(self.addr, self.kalloc.direct_map()) }
    }
}

impl<DM: DirectMap> Drop for RootPageTable<'_, DM> {
    fn drop(&mut self) {
        // PageTable::free can only fail if v2p fails, which is a bug.
        self.get_pml4().free(self.kalloc).expect("page table free");
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
#[path = "pagetable_test.rs"]
mod pagetable_test;
