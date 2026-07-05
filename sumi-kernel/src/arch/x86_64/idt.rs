//! Static 256-entry IDT.
//!
//! Each entry is a 16-byte interrupt-gate descriptor. We populate only the
//! vectors we handle; the rest are left as zero (not-present), which causes
//! a double fault if an unexpected vector fires.
//!
//! Vector assignments:
//!   0x06 (#UD)  — undefined instruction
//!   0x08 (#DF)  — double fault (IST=1 — stack may be corrupt)
//!   0x0D (#GP)  — general protection fault
//!   0x0E (#PF)  — page fault
//!   0x40        — LAPIC timer ISR (IST=1, preemption point)
//!   0x41        — IPI / TLB-flush ISR (IST=1)
//!
//! The BSP calls `init_and_load()` once to fill the table and load IDTR.
//! APs call `load()` to point their own IDTR at the same static table.

use core::arch::asm;

// Each IDT entry is 16 bytes: two u64 halves.
#[repr(C)]
#[derive(Clone, Copy)]
struct IdtEntry {
    low: u64,
    high: u64,
}

impl IdtEntry {
    const fn absent() -> Self {
        Self { low: 0, high: 0 }
    }

    /// Build an interrupt-gate descriptor.
    ///
    /// - `handler`: address of the ISR trampoline.
    /// - `ist`:     IST index (1–7) or 0 for none.
    /// - `dpl`:     descriptor privilege level (0 = kernel only).
    ///
    /// Gate type 0xE = 64-bit interrupt gate (clears IF on entry).
    /// Selector 0x08 = kernel code segment.
    const fn interrupt_gate(handler: u64, ist: u8, dpl: u8) -> Self {
        let offset_low = handler & 0xFFFF;
        let offset_mid = (handler >> 16) & 0xFFFF;
        let offset_high = handler >> 32;

        // Low 64 bits:
        //  [15:0]  offset[15:0]
        //  [31:16] segment selector = 0x08
        //  [34:32] IST index
        //  [39:35] reserved = 0
        //  [43:40] gate type = 0xE (interrupt gate)
        //  [44]    S = 0 (system)
        //  [46:45] DPL
        //  [47]    P = 1 (present)
        //  [63:48] offset[31:16]
        let low = offset_low
            | (0x08u64 << 16)
            | ((ist as u64 & 0x7) << 32)
            | (0x0Eu64 << 40)
            | ((dpl as u64 & 0x3) << 45)
            | (1u64 << 47)
            | (offset_mid << 48);

        // High 64 bits: offset[63:32] in [31:0], rest reserved.
        let high = offset_high;

        Self { low, high }
    }
}

/// IDTR pseudo-descriptor.
#[repr(C, packed)]
struct Idtr {
    limit: u16,
    base: u64,
}

/// 256-entry IDT. Static so it lives for the kernel's lifetime and both
/// the BSP and APs can share the same table (only the IDTR base pointer
/// is CPU-local; the table itself is global read-only after init).
static mut IDT: [IdtEntry; 256] = [const { IdtEntry::absent() }; 256];

// Declare the naked ISR trampolines defined in `interrupt.rs`.
unsafe extern "C" {
    fn isr_ud();
    fn isr_df();
    fn isr_gp();
    fn isr_pf();
    fn isr_timer();
    fn isr_ipi();
}

/// Populate the IDT and load it on the BSP. Must be called after `tss::init_and_load`.
pub fn init_and_load() {
    // SAFETY: Called exactly once from the BSP before APs start or
    // before KERNEL_READY is published. No other CPU touches IDT[] here.
    unsafe {
        IDT[0x06] = IdtEntry::interrupt_gate(isr_ud as *const () as u64, 0, 0);
        IDT[0x08] = IdtEntry::interrupt_gate(isr_df as *const () as u64, 1, 0);
        IDT[0x0D] = IdtEntry::interrupt_gate(isr_gp as *const () as u64, 0, 0);
        IDT[0x0E] = IdtEntry::interrupt_gate(isr_pf as *const () as u64, 0, 0);
        IDT[0x40] = IdtEntry::interrupt_gate(isr_timer as *const () as u64, 1, 0);
        IDT[0x41] = IdtEntry::interrupt_gate(isr_ipi as *const () as u64, 1, 0);
    }
    load();
}

/// Load the IDT on the calling CPU (for APs that share the BSP's table).
pub fn load() {
    let idtr = Idtr {
        limit: (256 * 16 - 1) as u16,
        // SAFETY: IDT is a 'static array; its address is valid for the
        // kernel's lifetime.
        base: core::ptr::addr_of!(IDT) as u64,
    };
    // SAFETY: idtr points at a valid, 'static 256-entry IDT. Ring-0 lidt.
    unsafe {
        asm!(
            "lidt [{idtr}]",
            idtr = in(reg) &idtr as *const Idtr,
            options(nostack, preserves_flags),
        );
    }
}
