use core::ptr;
use sumi_abi::address::VirtualAddr;

/// Read a 32-bit MMIO register.
///
/// # Safety
/// `base` must be the virtual address of a valid MMIO device region.
#[inline(always)]
pub unsafe fn read32(base: VirtualAddr, offset: usize) -> u32 {
    // SAFETY: Caller guarantees base+offset is a valid MMIO register.
    unsafe { ptr::read_volatile(base.add(offset).as_ptr::<u32>()) }
}

/// Write a 32-bit MMIO register.
///
/// # Safety
/// `base` must be the virtual address of a valid MMIO device region.
#[inline(always)]
pub unsafe fn write32(base: VirtualAddr, offset: usize, value: u32) {
    // SAFETY: Caller guarantees base+offset is a valid MMIO register.
    unsafe { ptr::write_volatile(base.add(offset).as_ptr::<u32>(), value) }
}
