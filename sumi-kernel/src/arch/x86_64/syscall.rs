use core::arch::asm;

const MSR_STAR:   u32 = 0xC000_0081;
const MSR_LSTAR:  u32 = 0xC000_0082;
const MSR_SFMASK: u32 = 0xC000_0084;

// STAR[47:32] = 0x0008 => CS=0x0008, SS=0x0010 on syscall entry
const STAR_VALUE: u64 = 0x0008u64 << 32;
// Clear IF (bit 9) and DF (bit 10)
const SFMASK_VALUE: u64 = 0x600;

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

/// Initialize MSRs to enable syscall handling. Call once at boot before running any user code.
pub fn init() {
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
    // Alignment padding: 16 pushes so far = 128 bytes. After `call` (8 bytes),
    // total = 136. SysV requires (RSP+8) % 16 == 0, so we need RSP % 16 == 8
    // before the call. 17 pushes = 136 bytes; if entry RSP % 16 == 8 (typical
    // for code reached via `call`), RSP after 17 pushes = (entry-8) % 16 == 0,
    // then after `call` RSP % 16 == 8. Correct. Use sub instead of push 0
    // to avoid a memory write.
    "sub rsp, 8",
    // rdi = &SyscallArgs: 9 qwords above current RSP
    // (padding=8, r11=8, rcx=8, rbx=8, rbp=8, r12=8, r13=8, r14=8, r15=8 = 72 bytes)
    "lea rdi, [rsp + 72]",
    "call syscall_dispatch",
    // rax = SyscallResult
    "add rsp, 8",   // remove padding
    "pop r11",      // restore RFLAGS
    "pop rcx",      // restore return RIP
    "pop rbx",
    "pop rbp",
    "pop r12",
    "pop r13",
    "pop r14",
    "pop r15",
    "add rsp, 56",  // discard SyscallArgs (7 * 8 bytes)
    // Restore RFLAGS from r11 and return to caller
    "push r11",
    "popfq",
    "jmp rcx",
);
