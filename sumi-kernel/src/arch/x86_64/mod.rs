use core::arch::asm;

pub mod pagetable;

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
