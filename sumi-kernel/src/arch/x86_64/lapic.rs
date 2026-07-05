//! Local APIC access via direct-map MMIO.
//!
//! The LAPIC is at physical `0xFEE0_0000` (x2APIC-compatible xAPIC mode).
//! KVM exposes it in-kernel when `KVM_CREATE_IRQCHIP` has been called, so
//! guest writes to this MMIO range are intercepted and handled by KVM.
//!
//! Each vCPU must call `init()` to enable its own LAPIC and start the
//! periodic timer (vector 0x40, ~1 ms at KVM's 1 GHz APIC bus).

use crate::KERNEL_DIRECT_MAP;
use sumi_abi::address::PhysicalAddr;
use sumi_abi::arch::layout::LAPIC_BASE_PHYS;

// LAPIC register offsets (byte offsets from LAPIC_BASE).
const LAPIC_SVR: u32 = 0x0F0; // Spurious Vector Register
const LAPIC_EOI: u32 = 0x0B0; // End of Interrupt (write-only)
const LAPIC_LVT_TIMER: u32 = 0x320; // LVT Timer register
const LAPIC_TIMER_ICR: u32 = 0x380; // Timer Initial Count Register
const LAPIC_TIMER_DCR: u32 = 0x3E0; // Timer Divide Configuration Register

/// Write a 32-bit value to a LAPIC MMIO register.
///
/// SAFETY: The caller must ensure the LAPIC is mapped (KVM_CREATE_IRQCHIP
/// was called) and `reg` is a valid LAPIC register offset.
#[inline]
fn write(reg: u32, val: u32) {
    let paddr = PhysicalAddr::new(LAPIC_BASE_PHYS.as_usize() + reg as usize);
    let vaddr = paddr.to_virtual(&KERNEL_DIRECT_MAP);
    // SAFETY: KVM_CREATE_IRQCHIP maps the LAPIC MMIO range; the direct
    // map covers the full physical address space. Volatile write ensures
    // the compiler does not elide or reorder the MMIO access.
    unsafe {
        core::ptr::write_volatile(vaddr.as_ptr::<u32>(), val);
    }
}

/// Initialise the LAPIC on the calling CPU and start the periodic timer.
///
/// Must be called after `tss::init_and_load` and `idt::init_and_load` /
/// `idt::load`, so the timer vector is already handled before the first
/// tick fires.
pub fn init() {
    // Enable the LAPIC via the Spurious Vector Register.
    // Bit 8 = APIC Software Enable; spurious vector = 0xFF.
    write(LAPIC_SVR, 0x1FF);

    // Timer divide: divide-by-1 (bits 3:0 = 0b1011, bit 2 is an
    // additional divide-by-2 bit; 0b1011 = divide by 1).
    write(LAPIC_TIMER_DCR, 0x0B);

    // LVT Timer: periodic mode (bit 17 = 1) + vector 0x40.
    write(LAPIC_LVT_TIMER, (1 << 17) | 0x40);

    // Initial count: ~1 ms at KVM's 1 GHz APIC bus frequency.
    // The APIC timer decrements at bus_freq / divisor. With divide-by-1
    // and 1 GHz bus, each tick is 1 ns. 1_000_000 ticks = 1 ms.
    write(LAPIC_TIMER_ICR, 1_000_000);
}

/// Send an End-of-Interrupt signal. Must be called from every ISR that
/// receives a LAPIC interrupt (timer, IPI) before returning with `iretq`.
#[inline(always)]
pub fn eoi() {
    // Writing 0 to EOI register acknowledges the interrupt.
    write(LAPIC_EOI, 0);
}
