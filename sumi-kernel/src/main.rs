#![no_std]
#![no_main]

use core::arch::asm;
#[cfg(not(test))]
use core::panic::PanicInfo;

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    unsafe {
        asm!(
            "mov eax, 0x41",
            "mov dx, 0xE9",
            "out dx, al",
            "hlt",
            options(noreturn, nomem, nostack)
        );
    }
}

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    loop {
        unsafe {
            asm!("hlt", options(nomem, nostack));
        }
    }
}
