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

/// `out dx, al` to the QEMU/KVM debug-console port. Real port I/O in
/// production; `out` traps (#GP) at CPL>0, so the host stand-in routes
/// `kprintln!` output (now reachable from `cargo test` via `sys_exit`
/// and friends, F14) to stderr instead.
#[cfg(not(test))]
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

#[cfg(test)]
#[inline(always)]
pub fn debugcon_write_byte(byte: u8) {
    use std::io::Write;
    let _ = std::io::stderr().write_all(&[byte]);
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
