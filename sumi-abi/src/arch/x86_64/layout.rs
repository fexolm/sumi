use crate::{
    address::{PhysicalAddr, VirtualAddr},
    arch::x86_64::address::get_pml4_index,
    layout::{KERNEL_CODE_PHYS, KERNEL_CODE_SIZE, MAX_PHYSICAL_ADDR},
};

pub const PAGE_SIZE: usize = 2 << 20;
pub const HUGE_PAGE_SIZE_1G: usize = 1 << 30;

pub const PAGE_TABLE_ENTRIES: usize = 512;
pub const PAGE_TABLE_SIZE: usize = 8 * PAGE_TABLE_ENTRIES;

pub const DIRECT_MAP_OFFSET: VirtualAddr = VirtualAddr::new(0xFFFF_8880_0000_0000);

pub const DIRECT_MAP_PML4: PhysicalAddr = KERNEL_CODE_PHYS.add(KERNEL_CODE_SIZE);

pub const DIRECT_MAP_PML4_OFFSET: usize = get_pml4_index(DIRECT_MAP_OFFSET);
// Each PML4 entry covers 512 PDPT entries * 1GB = 512GB
pub const DIRECT_MAP_PML4_ENTRIES_COUNT: usize =
    (MAX_PHYSICAL_ADDR + 1).div_ceil(HUGE_PAGE_SIZE_1G * PAGE_TABLE_ENTRIES);

// Direct map uses 1GB huge pages at PDPT level (no PD tables needed).
// One PDPT table per PML4 entry.
pub const DIRECT_MAP_PDPT: PhysicalAddr = DIRECT_MAP_PML4.add(PAGE_TABLE_SIZE);
pub const DIRECT_MAP_PDPT_COUNT: usize = DIRECT_MAP_PML4_ENTRIES_COUNT;

// pdpd and pd for the kernel code (we need to reserve 2gb of virtual address space for kernel code, for code-model=kernel)
pub const KERNEL_CODE_PDPD: PhysicalAddr =
    DIRECT_MAP_PDPT.add(DIRECT_MAP_PDPT_COUNT * PAGE_TABLE_SIZE);
pub const KERNEL_CODE_PD: PhysicalAddr = KERNEL_CODE_PDPD.add(PAGE_TABLE_SIZE);

const KERNEL_STACK_SIZE: usize = 0x1000 * 8; // 32KB stack
pub const KERNEL_STACK: PhysicalAddr = KERNEL_CODE_PD
    .add(PAGE_TABLE_SIZE + KERNEL_STACK_SIZE)
    .align_up(PAGE_SIZE);
