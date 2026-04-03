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

    pub fn get(&mut self, addr: VirtualAddr) -> Result<&mut PageTableEntry> {
        self.get_pml4().get(addr, self.kalloc)
    }

    pub fn get_if_present(&self, addr: VirtualAddr) -> Result<Option<PageTableEntry>> {
        self.get_pml4().get_if_present(addr, self.kalloc)
    }

    fn get_pml4(&self) -> &mut PageTable {
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
