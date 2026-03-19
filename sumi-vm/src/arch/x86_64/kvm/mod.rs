use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::VcpuExit;
use std::sync::atomic::{AtomicUsize, Ordering};
use sumi_abi::arch::address::DirectMap;
use sumi_abi::arch::address::{get_pdpt_index, get_pml4_index};
use sumi_abi::arch::layout::{
    DIRECT_MAP_PD, DIRECT_MAP_PD_COUNT, DIRECT_MAP_PDPT, DIRECT_MAP_PDPT_COUNT, DIRECT_MAP_PML4,
    DIRECT_MAP_PML4_ENTRIES_COUNT, DIRECT_MAP_PML4_OFFSET, KERNEL_CODE_PD, KERNEL_CODE_PDPD,
    KERNEL_CODE_PHYS, KERNEL_CODE_VIRT, KERNEL_STACK, PAGE_SIZE, PAGE_TABLE_ENTRIES,
    PAGE_TABLE_SIZE,
};
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

use crate::{
    error::Result,
    vm::{VCpu, VirtBackend, VmCreateInfo},
};

use crate::error::Error;

// Page-table / PTE flag bits
const PTE_PRESENT: u64 = 0x1;
const PTE_RW: u64 = 0x2;
const PTE_PS: u64 = 0x80;

// Control-register / system constants
const CR4_PAE: u64 = 1 << 5;
const CR4_OSFXSR: u64 = 1 << 9;
const CR4_OSXMMEXCPT: u64 = 1 << 10;
const EFER_LME: u64 = 1 << 8;
const EFER_LMA: u64 = 1 << 10;
const CR0_PE: u64 = 1 << 0;
const CR0_MP: u64 = 1 << 1;
const CR0_EM: u64 = 1 << 2;
const CR0_TS: u64 = 1 << 3;
const CR0_NE: u64 = 1 << 5;
const CR0_PG: u64 = 1 << 31;
const RFLAGS_RESERVED: u64 = 2;

// Segment selectors / descriptor types
const CS_SELECTOR: u16 = 0x8;
const SS_SELECTOR: u16 = 0x10;
const CS_TYPE: u8 = 0xB;
const SS_TYPE: u8 = 0x3;

// Port used by the guest test payload to output debug messages.
const DEBUG_PORT: u16 = 0xE9;
const GUEST_TEST_PAYLOAD: [u8; 11] = [
    0xB8, 0x41, 0x00, 0x00, 0x00, // mov eax, 0x41
    0x66, 0xBA, 0xE9, 0x00,       // mov dx, 0xE9
    0xEE,                         // out dx, al
    0xF4,                         // hlt
];

pub const GUEST_BASE: GuestAddress = GuestAddress(0);

pub struct KvmVm {
    vm_fd: kvm_ioctls::VmFd,
    max_vcpus: usize,
    mem_size: usize,
    next_vcpu_id: AtomicUsize,
}

impl KvmVm {}

impl VirtBackend for KvmVm {
    type VCpuType = KvmVCpu;

    fn new(info: &VmCreateInfo) -> Result<Self> {
        let kvm = kvm_ioctls::Kvm::new()?;
        let vm_fd = kvm.create_vm()?;
        Ok(Self {
            vm_fd: vm_fd,
            max_vcpus: kvm.get_nr_vcpus() as usize,
            mem_size: info.mem_size,
            next_vcpu_id: AtomicUsize::new(0),
        })
    }

    fn initialize_memory(&self, mem: &GuestMemoryMmap<()>) -> Result<()> {
        for i in 0..DIRECT_MAP_PML4_ENTRIES_COUNT {
            let entry_val = (DIRECT_MAP_PDPT.as_u64() + i as u64 * PAGE_TABLE_SIZE as u64)
                | PTE_PRESENT
                | PTE_RW;
            let entry_addr =
                GuestAddress(DIRECT_MAP_PML4.as_u64() + ((DIRECT_MAP_PML4_OFFSET + i) * 8) as u64);
            mem.write_slice(&entry_val.to_le_bytes(), entry_addr)?;
        }

        for i in 0..DIRECT_MAP_PDPT_COUNT * PAGE_TABLE_ENTRIES {
            let pd_phys = DIRECT_MAP_PD.as_u64() + i as u64 * PAGE_TABLE_SIZE as u64;
            let entry_val = pd_phys | PTE_PRESENT | PTE_RW;
            let entry_addr = GuestAddress(DIRECT_MAP_PDPT.as_u64() + (i * 8) as u64);
            mem.write_slice(&entry_val.to_le_bytes(), entry_addr)?;
        }

        for i in 0..DIRECT_MAP_PD_COUNT * PAGE_TABLE_ENTRIES {
            let phys = i as u64 * PAGE_SIZE as u64;
            let entry_val = phys | PTE_PRESENT | PTE_RW | PTE_PS;
            let entry_addr = GuestAddress(DIRECT_MAP_PD.as_u64() + (i * 8) as u64);
            mem.write_slice(&entry_val.to_le_bytes(), entry_addr)?;
        }

        // map kernel code region
        let kernel_pml4_val = KERNEL_CODE_PDPD.as_u64() | PTE_PRESENT | PTE_RW;
        let kernel_pml4_addr =
            GuestAddress(DIRECT_MAP_PML4.as_u64() + (get_pml4_index(KERNEL_CODE_VIRT) * 8) as u64);
        mem.write_slice(&kernel_pml4_val.to_le_bytes(), kernel_pml4_addr)?;

        for i in 0..2 {
            let pd_phys = KERNEL_CODE_PD.as_u64() + (i as u64 * PAGE_TABLE_SIZE as u64);
            let entry_val = pd_phys | PTE_PRESENT | PTE_RW;
            let entry_addr = GuestAddress(
                KERNEL_CODE_PDPD.as_u64() + ((get_pdpt_index(KERNEL_CODE_VIRT) + i) * 8) as u64,
            );
            mem.write_slice(&entry_val.to_le_bytes(), entry_addr)?;
        }

        for i in 0..PAGE_TABLE_ENTRIES {
            let phys = KERNEL_CODE_PHYS.add(i * PAGE_SIZE).as_u64();
            let entry_val = phys | PTE_PRESENT | PTE_RW | PTE_PS;
            let entry_addr = GuestAddress(KERNEL_CODE_PD.as_u64() + (i * 8) as u64);
            mem.write_slice(&entry_val.to_le_bytes(), entry_addr)?;
        }

        // Write the guest test payload (a simple program that outputs 'A' to the debug port and halts) into guest memory at the physical address where the kernel code is mapped.
        mem.write_slice(&GUEST_TEST_PAYLOAD, GuestAddress(KERNEL_CODE_PHYS.as_u64()))?;

        // Register the guest memory region with KVM.
        unsafe {
            self.vm_fd
                .set_user_memory_region(kvm_userspace_memory_region {
                    slot: 0,
                    guest_phys_addr: GUEST_BASE.0,
                    memory_size: self.mem_size as u64,
                    userspace_addr: mem.get_host_address(GUEST_BASE).unwrap() as u64,
                    flags: 0,
                })?;
        }

        Ok(())
    }

    fn create_vcpu(&self) -> Result<Self::VCpuType> {
        let id = self.next_vcpu_id.fetch_add(1, Ordering::SeqCst);
        if id >= self.max_vcpus {
            return Err(Error::InvalidVmConfig(format!(
                "vcpu_count exceeds KVM's max of {}",
                self.max_vcpus
            )));
        }
        let fd = self.vm_fd.create_vcpu(id as u64)?;

        Ok(KvmVCpu::new(fd))
    }
}

pub struct KvmVCpu {
    fd: kvm_ioctls::VcpuFd,
}

impl KvmVCpu {
    pub fn new(fd: kvm_ioctls::VcpuFd) -> Self {
        Self { fd }
    }
}

impl VCpu for KvmVCpu {
    fn init(&mut self) -> Result<()> {
        // General purpose registers:
        // - RIP: instruction pointer where the guest will start executing
        // - RSP: stack pointer inside guest memory
        // - RFLAGS: set the reserved bit required by x86
        let mut regs = self.fd.get_regs()?;
        // Start executing at the virtual address where we loaded the guest test payload.
        regs.rip = KERNEL_CODE_VIRT.as_u64();
        // _start is entered without a CALL frame; keep SysV ABI expectation
        // (RSP % 16 == 8 on function entry) so local variables that require
        // 16-byte alignment remain aligned after prologue.
        regs.rsp = KERNEL_STACK.to_virtual(&DirectMap).as_u64() - 8;
        regs.rflags = RFLAGS_RESERVED; // required reserved bit
        self.fd.set_regs(&regs)?;

        let mut sregs = self.fd.get_sregs()?;
        sregs.cr3 = DIRECT_MAP_PML4.as_u64(); // CR3 = physical address of the PML4 (page-table root)

        // CR4.PAE must be set to enable physical-address-extension paging required
        // by 64-bit mode page tables.
        sregs.cr4 |= CR4_PAE | CR4_OSFXSR | CR4_OSXMMEXCPT;

        // EFER.LME enables Long Mode; EFER.LMA indicates Long Mode Active.
        sregs.efer = EFER_LME | EFER_LMA;

        // Code segment descriptor: set as a 64-bit code segment.
        sregs.cs.l = 1; // L bit = 1 => 64-bit code segment
        sregs.cs.db = 0; // DB = 0 => default operand size is 32-bit (unused in 64-bit)
        sregs.cs.s = 1; // S = 1 => code/data descriptor (not system)
        sregs.cs.type_ = CS_TYPE; // executable, read, accessed
        sregs.cs.present = 1;
        sregs.cs.dpl = 0; // ring 0
        sregs.cs.selector = CS_SELECTOR;

        // Stack/data segment for the guest (selector points into the GDT).
        sregs.ss.s = 1;
        sregs.ss.type_ = SS_TYPE;
        sregs.ss.present = 1;
        sregs.ss.selector = SS_SELECTOR;

        // KVM allows zero-sized GDT/IDT here because we supply selectors directly.
        sregs.gdt.limit = 0;
        sregs.idt.limit = 0;

        // CR0: enable protected mode (PE) and paging (PG). Also enable NE (numeric
        // error) so x87 exceptions behave as expected.
        sregs.cr0 |= CR0_PG | CR0_PE | CR0_MP; // paging + protected mode + monitor coprocessor
        sregs.cr0 |= CR0_NE; // numeric error
        sregs.cr0 &= !CR0_EM; // enable x87/SSE instructions
        sregs.cr0 &= !CR0_TS; // allow immediate FPU/SSE use

        self.fd.set_sregs(&sregs)?;
        Ok(())
    }

    fn run(&mut self) -> Result<()> {
        loop {
            match self.fd.run()? {
                VcpuExit::IoOut(port, data) if port == DEBUG_PORT => {
                    println!("IoOut: {}", String::from_utf8_lossy(data));
                }
                VcpuExit::Hlt | VcpuExit::Shutdown => return Ok(()),
                other => return Err(Error::UnexpectedExit(format!("{:?}", other))),
            }
        }
    }
}
