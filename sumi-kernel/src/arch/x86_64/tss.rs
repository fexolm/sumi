//! Per-CPU TSS and GDT for interrupt stack switching.
//!
//! Each CPU needs its own GDT because the TSS descriptor encodes the
//! per-CPU TSS base address. Each TSS's IST1 slot points at a per-CPU
//! interrupt stack drawn from `SYSCALL_STACKS[cpu_id]` — a 64 KiB buffer
//! that is unused since syscall entry switched to per-thread kernel stacks.
//!
//! GDT layout (5 entries):
//!   [0] 0x00  null descriptor
//!   [1] 0x08  64-bit code segment (selector 0x08)
//!   [2] 0x10  data/stack segment  (selector 0x10)
//!   [3] 0x18  TSS descriptor low  (selector 0x18, 2 slots)
//!   [4] 0x20  TSS descriptor high
//!
//! Call `init_and_load(cpu_id)` on each CPU before loading the IDT.

use core::arch::asm;

use crate::sched::percpu::{MAX_VCPUS, SYSCALL_STACK_SIZE, SYSCALL_STACKS};

/// x86-64 TSS. Only RSP0 and IST1..7 fields are meaningful here;
/// sumi stays in ring 0 throughout so the ring-transition RSP fields
/// are unused. IOPB offset is set to `size_of::<Tss>()` (no IOPB).
#[repr(C, packed)]
pub struct Tss {
    _reserved0: u32,
    rsp0: u64,
    rsp1: u64,
    rsp2: u64,
    _reserved1: u64,
    /// IST1..IST7 stacks. We populate IST1 (index 0) for timer/IPI ISRs.
    ist: [u64; 7],
    _reserved2: u64,
    _reserved3: u16,
    iopb_offset: u16,
}

const _: () = assert!(core::mem::size_of::<Tss>() == 104);

impl Tss {
    pub const fn zeroed() -> Self {
        Self {
            _reserved0: 0,
            rsp0: 0,
            rsp1: 0,
            rsp2: 0,
            _reserved1: 0,
            ist: [0; 7],
            _reserved2: 0,
            _reserved3: 0,
            iopb_offset: core::mem::size_of::<Tss>() as u16,
        }
    }
}

/// 5-slot GDT per CPU (40 bytes total).
#[repr(C, align(8))]
struct Gdt([u64; 5]);

/// GDTR pseudo-descriptor loaded by `lgdt`. Must be packed so the CPU
/// sees the 2-byte limit immediately followed by the 8-byte base.
#[repr(C, packed)]
struct Gdtr {
    limit: u16,
    base: u64,
}

/// Per-CPU GDT storage.
static mut GDT: [Gdt; MAX_VCPUS] = [const { Gdt([0; 5]) }; MAX_VCPUS];
/// Per-CPU TSS storage.
static mut TSS: [Tss; MAX_VCPUS] = [const { Tss::zeroed() }; MAX_VCPUS];

/// Initialise `TSS[cpu_id]` and `GDT[cpu_id]`, then execute `lgdt` and
/// `ltr 0x18`. Must be called on the owning CPU before the IDT is loaded.
pub fn init_and_load(cpu_id: u32) {
    let idx = cpu_id as usize;
    debug_assert!(idx < MAX_VCPUS);

    // SAFETY: Each CPU writes its own distinct array slot by cpu_id.
    // BSP is serialized; each AP writes exactly its own slot once,
    // before any other CPU can read it (they spin on KERNEL_READY first).
    unsafe {
        // Use the top of SYSCALL_STACKS[cpu_id] as the IST1 interrupt
        // stack. This 64 KiB region is unused since syscall entry retired
        // the per-CPU syscall stack mechanism. The timer/IPI ISRs use
        // IST1 so they always have a valid stack even if the interrupted
        // thread's kernel stack is nearly full.
        let stack_base = core::ptr::addr_of!(SYSCALL_STACKS[idx]) as u64;
        let stack_top = stack_base + SYSCALL_STACK_SIZE as u64;

        // Populate TSS: set IST1 (array index 0) and IOPB offset.
        let tss_ptr = core::ptr::addr_of_mut!(TSS[idx]);
        (*tss_ptr).ist[0] = stack_top;
        (*tss_ptr).iopb_offset = core::mem::size_of::<Tss>() as u16;

        // Build the 16-byte TSS descriptor.
        // See Intel SDM Vol.3A §7.2.2 for the encoding.
        let base = tss_ptr as u64;
        let limit = (core::mem::size_of::<Tss>() - 1) as u64;

        // Low 64 bits:
        //   [15:0]   limit[15:0]
        //   [39:16]  base[23:0]
        //   [43:40]  type = 0b1001 (64-bit TSS Available)
        //   [44]     S    = 0      (system descriptor)
        //   [46:45]  DPL  = 0
        //   [47]     P    = 1      (present)
        //   [51:48]  limit[19:16]
        //   [55:52]  0
        //   [63:56]  base[31:24]
        let tss_low: u64 = (limit & 0xFFFF)
            | ((base & 0xFF_FFFF) << 16)
            | (0x89u64 << 40)                     // P=1, DPL=0, type=9
            | (((limit >> 16) & 0xF) << 48)
            | (((base >> 24) & 0xFF) << 56);

        // High 64 bits: base[63:32] in [31:0], rest must be zero.
        let tss_high: u64 = (base >> 32) & 0xFFFF_FFFF;

        // Populate GDT.
        let gdt_ptr = core::ptr::addr_of_mut!(GDT[idx]);
        (*gdt_ptr).0[0] = 0; // null
        (*gdt_ptr).0[1] = 0x00AF_9A00_0000_FFFF; // code64
        (*gdt_ptr).0[2] = 0x00CF_9200_0000_FFFF; // data
        (*gdt_ptr).0[3] = tss_low;
        (*gdt_ptr).0[4] = tss_high;

        let gdtr = Gdtr {
            limit: (5 * 8 - 1) as u16,
            base: gdt_ptr as u64,
        };

        // SAFETY: gdtr points at a valid 5-entry GDT with a correct TSS
        // descriptor at slots 3/4. The TSS has IST1 pointing at an
        // allocated, aligned 64 KiB stack buffer. All of this is in
        // 'static storage.
        asm!(
            "lgdt [{gdtr}]",
            "ltr {sel:x}",
            gdtr = in(reg) &gdtr as *const Gdtr,
            sel  = in(reg) 0x18u16,
            options(nostack, preserves_flags),
        );
    }
}
