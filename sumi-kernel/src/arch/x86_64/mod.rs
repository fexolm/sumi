use core::arch::asm;

pub mod debugcon;
pub mod pagetable;
#[cfg(not(test))]
pub mod syscall;
#[cfg(not(test))]
pub mod ap_start;
#[cfg(not(test))]
pub mod smp;
#[cfg(not(test))]
pub mod hypercall;
#[cfg(not(test))]
pub mod switch;
#[cfg(not(test))]
pub mod tss;
#[cfg(not(test))]
pub mod idt;
#[cfg(not(test))]
pub mod lapic;
#[cfg(not(test))]
pub mod interrupt;

pub use self::pagetable::RootPageTable;
pub use sumi_abi::arch::address::DirectMap as KernelDirectMap;

#[inline(always)]
pub fn debugcon_write_byte(byte: u8) {
    unsafe {
        asm!(
            "out dx, al",
            in("dx") 0xE9u16,
            in("al") byte,
            options(nomem, nostack, preserves_flags)
        );
    }
}

#[inline(always)]
pub fn halt() {
    unsafe {
        asm!("hlt", options(nomem, nostack));
    }
}

pub fn halt_forever() -> ! {
    loop {
        halt();
    }
}
