#![no_std]
#![no_main]

use core::arch::asm;

fn syscall3(nr: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    let ret: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") nr => ret,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    ret
}

fn write(fd: u64, buf: &[u8]) -> i64 {
    syscall3(1, fd, buf.as_ptr() as u64, buf.len() as u64)
}

fn exit_group(code: u64) -> ! {
    syscall3(231, code, 0, 0);
    loop {}
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    write(1, b"Hello from Rust!\n");
    exit_group(0);
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    write(2, b"PANIC\n");
    exit_group(1);
}
