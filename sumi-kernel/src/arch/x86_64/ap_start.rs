//! AP entry stub.
//!
//! Each AP host thread in sumi-vm starts `KVM_RUN` with:
//!   - `RIP` = address of `ap_start_asm` (from ELF symbol table)
//!   - `RDI` = cpu_id (1..N)
//!   - `RSP` = garbage (overwritten below)
//!   - `CR3`, sregs, EFER = identical to the BSP (see sumi-vm set_sregs).
//!
//! The stub's only job is to set `RSP` to the top of this AP's boot
//! stack (`AP_BOOT_STACKS[cpu_id]`) and tail-call the Rust entry
//! point `ap_main_rust`. Everything else — MSR programming, the spin
//! on `KERNEL_READY`, the idle loop — lives in Rust.
//!
//! Stack alignment: RSP must be 0 (mod 16) immediately before the `call`;
//! `call` pushes 8 so the callee sees `(rsp+8) % 16 == 0` as required
//! by the SysV AMD64 ABI.
//!
//! `AP_BOOT_STACKS` is indexed by `cpu_id` including slot 0 (unused,
//! the BSP has its own stack); this keeps the address computation a
//! single `cpu_id * AP_BOOT_STACK_SIZE` with no -1.

use crate::sched::percpu::AP_BOOT_STACK_SIZE;

// Build-time sanity: the asm below hard-codes the stack size via
// `.set AP_BOOT_STACK_SIZE, 0x4000`. If AP_BOOT_STACK_SIZE ever
// changes, the assert below forces a review of the asm (there is no
// way to `in(const)` a constant into `global_asm!` directly — the
// value is spelled literally in the `.set` directive).
const _: () = {
    assert!(
        AP_BOOT_STACK_SIZE == 16 * 1024,
        "ap_start.rs global_asm has `.set AP_BOOT_STACK_SIZE, 0x4000` hard-coded; \
         update both sides together.",
    );
};

core::arch::global_asm!(
    ".set AP_BOOT_STACK_SIZE, 0x4000", // 16 KiB — must match AP_BOOT_STACK_SIZE
    ".global ap_start_asm",
    "ap_start_asm:",
    // rdi = cpu_id (set by sumi-vm KvmVCpu::init_ap before KVM_RUN).
    //
    // Compute: rsp = &AP_BOOT_STACKS + (cpu_id + 1) * AP_BOOT_STACK_SIZE
    // i.e. the one-past-end of this AP's boot stack slot.
    "lea rax, [rip + AP_BOOT_STACKS]",
    "mov rcx, rdi",
    "inc rcx",
    "imul rcx, rcx, 0x4000",
    "add rax, rcx",
    "mov rsp, rax",
    // rsp == one-past-end of this AP's boot stack slot.
    // `AP_BOOT_STACKS` base is 16-aligned (struct align(16)) and
    // `AP_BOOT_STACK_SIZE = 0x4000` is a multiple of 16, so rsp is
    // already 16-aligned. SysV ABI requires `rsp % 16 == 0` immediately
    // before the `call`; the implicit push by `call` makes the callee
    // observe `(rsp+8) % 16 == 0` as required. No padding needed.
    "call ap_main_rust",
    "ud2",
);

// Declared so Rust knows `ap_start_asm` is an external symbol for any
// code that wants to take its address (tests, potential GDB helpers).
// Not called directly from Rust.
unsafe extern "C" {
    pub fn ap_start_asm();
}

// Ensure the linker keeps `ap_start_asm` even with LTO. A `#[used]`
// static that holds a function pointer referencing the symbol forces
// the linker to retain it.
#[used]
static _AP_START_ASM_REF: unsafe extern "C" fn() = ap_start_asm;
