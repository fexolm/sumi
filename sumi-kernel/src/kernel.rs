use crate::arch::RootPageTable;
use crate::memory::alloc::{kmalloc::KernelAllocator, palloc::PageAllocator};
use sumi_abi::address::DirectMap;

pub trait Kernel<'i> {
    type DM: DirectMap;

    fn palloc(&self) -> &'i PageAllocator;
    fn kalloc(&self) -> &'i KernelAllocator<'i, Self::DM>;
    fn page_table(&self) -> &'i RootPageTable<'i, Self::DM>;
}

pub struct KernelState<'i, DM: DirectMap> {
    pub palloc: &'i PageAllocator,
    pub kalloc: &'i KernelAllocator<'i, DM>,
    pub page_table: &'i RootPageTable<'i, DM>,
}

impl<'i, DM: DirectMap> KernelState<'i, DM> {
    pub const fn new(
        palloc: &'i PageAllocator,
        kalloc: &'i KernelAllocator<'i, DM>,
        page_table: &'i RootPageTable<'i, DM>,
    ) -> Self {
        Self {
            palloc,
            kalloc,
            page_table,
        }
    }
}

impl<'i, DM: DirectMap> Kernel<'i> for KernelState<'i, DM> {
    type DM = DM;

    fn palloc(&self) -> &'i PageAllocator {
        self.palloc
    }

    fn kalloc(&self) -> &'i KernelAllocator<'i, Self::DM> {
        self.kalloc
    }

    fn page_table(&self) -> &'i RootPageTable<'i, Self::DM> {
        self.page_table
    }
}
