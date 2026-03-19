use crate::address::{PhysicalAddr, VirtualAddr};

pub const MAX_PHYSICAL_ADDR: usize = 0x0000_00FF_FFFF_FFFF;
pub const KERNEL_CODE_VIRT: VirtualAddr = VirtualAddr::new(0xFFFF_FFFF_8000_0000);
pub const KERNEL_CODE_PHYS: PhysicalAddr = PhysicalAddr::new(0x0000_0000_0000_0000);
pub const KERNEL_CODE_SIZE: usize = 1 << 31;
