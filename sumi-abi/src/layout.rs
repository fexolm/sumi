use crate::address::{PhysicalAddr, VirtualAddr};

pub const MAX_PHYSICAL_ADDR: usize = 0x0000_7FFF_FFFF_FFFF;
/// Maximum guest RAM the page allocator tracks (2 TB).
/// Separate from MAX_PHYSICAL_ADDR which is the direct-map address space (128 TB).
pub const MAX_GUEST_MEMORY: usize = 0x0000_0200_0000_0000;
pub const KERNEL_CODE_VIRT: VirtualAddr = VirtualAddr::new(0xFFFF_FFFF_8000_0000);
pub const KERNEL_CODE_PHYS: PhysicalAddr = PhysicalAddr::new(0x0000_0000_0000_0000);
pub const KERNEL_CODE_SIZE: usize = 1 << 31;
