use core::arch::asm;

const MSR_STAR:   u32 = 0xC000_0081;
const MSR_LSTAR:  u32 = 0xC000_0082;
const MSR_SFMASK: u32 = 0xC000_0084;

// STAR[47:32] = 0x0008 => CS=0x0008, SS=0x0010 on syscall entry
const STAR_VALUE: u64 = 0x0008u64 << 32;
// Clear IF (bit 9) and DF (bit 10)
const SFMASK_VALUE: u64 = 0x600;

/// Dedicated kernel stack for syscall handling (64 KB, 16-byte aligned).
/// Using a kernel stack prevents corruption of user stack frames by DMA
/// operations and deep call chains (kprintln, FUSE I/O, etc.).
const SYSCALL_STACK_SIZE: usize = 64 * 1024;

#[repr(C, align(16))]
struct SyscallStack([u8; SYSCALL_STACK_SIZE]);

#[unsafe(no_mangle)]
static mut SYSCALL_STACK: SyscallStack = SyscallStack([0; SYSCALL_STACK_SIZE]);

/// Top of the syscall stack. Written once by init(), read by syscall_entry asm.
#[unsafe(no_mangle)]
static mut SYSCALL_STACK_TOP: u64 = 0;

/// Scratch space to save user RSP during stack switch (single-CPU only).
#[unsafe(no_mangle)]
static mut SAVED_USER_RSP: u64 = 0;

pub(crate) unsafe fn rdmsr(msr: u32) -> u64 {
    let eax: u32;
    let edx: u32;
    // SAFETY: Caller ensures the MSR address is valid and we are in ring 0.
    unsafe {
        asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") eax,
            out("edx") edx,
            options(nomem, nostack, preserves_flags),
        );
    }
    ((edx as u64) << 32) | (eax as u64)
}

pub(crate) unsafe fn wrmsr(msr: u32, value: u64) {
    let eax = value as u32;
    let edx = (value >> 32) as u32;
    // SAFETY: Caller ensures the MSR address and value are valid and we are in ring 0.
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") eax,
            in("edx") edx,
            options(nomem, nostack, preserves_flags),
        );
    }
}

unsafe extern "C" {
    fn syscall_entry();
}

/// Initialize MSRs and syscall stack. Call once at boot before running any user code.
pub fn init() {
    // SAFETY: Single-threaded boot, setting up the syscall stack pointer.
    // We use raw pointer arithmetic to avoid creating a shared reference to the mutable static.
    unsafe {
        SYSCALL_STACK_TOP =
            core::ptr::addr_of!(SYSCALL_STACK) as u64 + SYSCALL_STACK_SIZE as u64;
    }

    // SAFETY: We are in ring 0 during kernel boot. All MSR addresses and values
    // are correct for 64-bit long mode.
    unsafe {
        wrmsr(MSR_LSTAR, syscall_entry as *const () as u64);
        wrmsr(MSR_STAR, STAR_VALUE);
        wrmsr(MSR_SFMASK, SFMASK_VALUE);
    }
}

core::arch::global_asm!(
    ".global syscall_entry",
    "syscall_entry:",
    // At entry: RCX = return RIP, R11 = saved RFLAGS, RSP = user stack.
    // Switch to the kernel syscall stack to protect user stack frames from
    // corruption by deep call chains and DMA buffer allocations.
    //
    // Save user RSP to scratch location, load kernel stack top.
    "mov [rip + SAVED_USER_RSP], rsp",
    "mov rsp, [rip + SYSCALL_STACK_TOP]",
    //
    // Push saved user RSP (so we can restore it on return)
    "push qword ptr [rip + SAVED_USER_RSP]",
    // Push SyscallArgs: push arg5 first so nr (rax) ends up at lowest address
    "push r9",
    "push r8",
    "push r10",
    "push rdx",
    "push rsi",
    "push rdi",
    "push rax",
    // Save callee-saved registers (SysV ABI)
    "push r15",
    "push r14",
    "push r13",
    "push r12",
    "push rbp",
    "push rbx",
    // Save syscall-clobbered return state
    "push rcx",    // return RIP (saved by syscall instruction into rcx)
    "push r11",    // saved RFLAGS (saved by syscall instruction into r11)
    // Alignment: 17 pushes = 136 bytes from SYSCALL_STACK_TOP (16-aligned).
    // RSP = TOP - 136. 136 mod 16 = 8. So RSP mod 16 = 8.
    // After `call` pushes 8 more → RSP mod 16 = 0. SysV wants RSP+8 ≡ 0 (mod 16)
    // at function entry, i.e. RSP ≡ 8 (mod 16). Currently RSP ≡ 8. Need padding.
    // Actually: 17 pushes from 16-aligned top → RSP = top - 136. 136 = 8*17.
    // RSP mod 16 = (top mod 16) - (136 mod 16) = 0 - 8 = -8 ≡ 8 (mod 16).
    // Before call we need RSP mod 16 == 0 (so after call RSP mod 16 == 8).
    "sub rsp, 8",
    // rdi = &SyscallArgs: 9 qwords above current RSP
    // (padding=8, r11=8, rcx=8, rbx=8, rbp=8, r12=8, r13=8, r14=8, r15=8 = 72 bytes)
    "lea rdi, [rsp + 72]",
    "call syscall_dispatch",
    // rax = SyscallResult (preserve it — don't pop into rax below)
    "add rsp, 8",   // remove padding
    "pop r11",      // restore RFLAGS
    "pop rcx",      // restore return RIP
    "pop rbx",
    "pop rbp",
    "pop r12",
    "pop r13",
    "pop r14",
    "pop r15",
    // Restore syscall input registers: rax(nr), rdi, rsi, rdx, r10, r8, r9
    // musl's inline asm does NOT list these as clobbers, so the compiler
    // assumes they are preserved across the syscall instruction.
    // Skip rax (nr) — we return the result in rax instead.
    "add rsp, 8",   // skip saved rax (nr)
    "pop rdi",      // restore arg0
    "pop rsi",      // restore arg1
    "pop rdx",      // restore arg2
    "pop r10",      // restore arg3
    "pop r8",       // restore arg4
    "pop r9",       // restore arg5
    // Restore RFLAGS while still on the kernel stack (preserves user red zone)
    "push r11",
    "popfq",
    // Switch back to user stack and return
    "pop rsp",      // restore user RSP (from kernel stack)
    "jmp rcx",
);
