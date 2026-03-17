use crate::{
    address::{PhysicalAddr, VirtualAddr},
    arch::x86_64::layout::{DIRECT_MAP_OFFSET, MAX_PHYSICAL_ADDR},
};

pub struct DirectMap;

impl crate::address::DirectMap for DirectMap {
    fn p2v(&self, paddr: PhysicalAddr) -> VirtualAddr {
        assert!(paddr.as_usize() < MAX_PHYSICAL_ADDR);
        VirtualAddr::new(paddr.as_usize() + DIRECT_MAP_OFFSET.as_usize())
    }

    fn v2p(&self, vaddr: VirtualAddr) -> Option<PhysicalAddr> {
        if vaddr.as_usize() < DIRECT_MAP_OFFSET.as_usize()
            || vaddr.as_usize() > DIRECT_MAP_OFFSET.as_usize() + MAX_PHYSICAL_ADDR
        {
            None
        } else {
            Some(PhysicalAddr::new(
                vaddr.as_usize() - DIRECT_MAP_OFFSET.as_usize(),
            ))
        }
    }
}

pub trait X64Vaddr {
    fn pml4_index(self) -> usize;
    fn pdpt_index(self) -> usize;
    fn pd_index(self) -> usize;
}

pub const fn get_pml4_index(addr: VirtualAddr) -> usize {
    (addr.as_usize() >> 39) & 0x1FF
}

pub const fn get_pdpt_index(addr: VirtualAddr) -> usize {
    (addr.as_usize() >> 30) & 0x1FF
}

pub const fn get_pd_index(addr: VirtualAddr) -> usize {
    (addr.as_usize() >> 21) & 0x1FF
}

impl X64Vaddr for VirtualAddr {
    fn pml4_index(self) -> usize {
        get_pml4_index(self)
    }

    fn pdpt_index(self) -> usize {
        get_pdpt_index(self)
    }

    fn pd_index(self) -> usize {
        get_pd_index(self)
    }
}
