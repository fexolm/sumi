use crate::{
    address::{PhysicalAddr, VirtualAddr},
    arch::x86_64::address::get_pml4_index,
};

pub const MAX_PHYSICAL_ADDR: usize = 0x0000_00FF_FFFF_FFFF;
pub const PAGE_SIZE: usize = 2 << 20;

pub const PAGE_TABLE_ENTRIES: usize = 512;
pub const PAGE_TABLE_SIZE: usize = 8 * PAGE_TABLE_ENTRIES;

pub const KERNEL_CODE_VIRT: VirtualAddr = VirtualAddr::new(0xFFFF_FFFF_8000_0000);

pub const DIRECT_MAP_OFFSET: VirtualAddr = VirtualAddr::new(0xFFFF_8880_0000_0000);

pub const DIRECT_MAP_PML4: PhysicalAddr = PhysicalAddr::new(0x0);

pub const DIRECT_MAP_PML4_OFFSET: usize = get_pml4_index(DIRECT_MAP_OFFSET);
pub const DIRECT_MAP_PML4_ENTRIES_COUNT: usize = (MAX_PHYSICAL_ADDR + 1)
    .div_ceil(PAGE_SIZE * PAGE_TABLE_ENTRIES * PAGE_TABLE_ENTRIES * PAGE_TABLE_ENTRIES); // number of PML4 entries needed to cover the direct map region

pub const DIRECT_MAP_PDPT: PhysicalAddr = DIRECT_MAP_PML4.add(PAGE_TABLE_SIZE);
pub const DIRECT_MAP_PDPT_COUNT: usize =
    (MAX_PHYSICAL_ADDR + 1).div_ceil(PAGE_SIZE * PAGE_TABLE_ENTRIES * PAGE_TABLE_ENTRIES);

pub const DIRECT_MAP_PD: PhysicalAddr =
    DIRECT_MAP_PDPT.add(DIRECT_MAP_PDPT_COUNT * PAGE_TABLE_SIZE);
pub const DIRECT_MAP_PD_COUNT: usize =
    (MAX_PHYSICAL_ADDR + 1).div_ceil(PAGE_SIZE * PAGE_TABLE_ENTRIES);

// pdpd and pd for the kernel code (we need to reserve 2gb of virtual address space for kernel code, for code-model=kernel)
pub const KERNEL_CODE_PDPD: PhysicalAddr = DIRECT_MAP_PD.add(DIRECT_MAP_PD_COUNT * PAGE_TABLE_SIZE);
pub const KERNEL_CODE_PD: PhysicalAddr = KERNEL_CODE_PDPD.add(PAGE_TABLE_SIZE);

const KERNEL_STACK_SIZE: usize = 0x1000 * 8; // 32KB stack
pub const KERNEL_STACK: PhysicalAddr = KERNEL_CODE_PD
    .add(PAGE_TABLE_SIZE + KERNEL_STACK_SIZE)
    .align_up(PAGE_SIZE);

pub const KERNEL_CODE_PHYS: PhysicalAddr = KERNEL_STACK; // stack will grow down from this point, code will grow up
pub const KERNEL_CODE_SIZE: usize = PAGE_SIZE;

pub const PALLOC_FIRST_PAGE: PhysicalAddr = KERNEL_CODE_PHYS.add(KERNEL_CODE_SIZE);
