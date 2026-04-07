use kvm_bindings::{
    KVM_GUESTDBG_ENABLE, KVM_GUESTDBG_SINGLESTEP, KVM_GUESTDBG_USE_SW_BP, KVM_MAX_CPUID_ENTRIES,
    kvm_guest_debug, kvm_userspace_memory_region,
};
use kvm_ioctls::VcpuExit;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use sumi_abi::arch::address::DirectMap;
use sumi_abi::arch::address::{get_pdpt_index, get_pml4_index};
use sumi_abi::arch::layout::{
    DAX_WINDOW_BASE, DAX_WINDOW_SIZE, DIRECT_MAP_PDPT, DIRECT_MAP_PDPT_COUNT, DIRECT_MAP_PML4,
    DIRECT_MAP_PML4_ENTRIES_COUNT, DIRECT_MAP_PML4_OFFSET, HUGE_PAGE_SIZE_1G, KERNEL_CODE_PD,
    KERNEL_CODE_PDPD, KERNEL_STACK, PAGE_SIZE, PAGE_TABLE_ENTRIES, PAGE_TABLE_SIZE,
};
use sumi_abi::layout::{KERNEL_CODE_PHYS, KERNEL_CODE_VIRT};
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

use crate::debug::breakpoints::{BreakpointManager, read_guest_memory, write_guest_memory};
use crate::debug::{DebugCommand, DebugEvent, RegisterFile, StopReason, VCpuDebugReceiver};
use crate::devices::DeviceRegistry;
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
const EFER_SCE: u64 = 1 << 0;
const EFER_LME: u64 = 1 << 8;
const EFER_LMA: u64 = 1 << 10;
const CR0_PE: u64 = 1 << 0;
const CR0_MP: u64 = 1 << 1;
const CR0_EM: u64 = 1 << 2;
const CR0_TS: u64 = 1 << 3;
const CR0_NE: u64 = 1 << 5;
const CR0_PG: u64 = 1 << 31;
const RFLAGS_RESERVED: u64 = 2;

mod cpuid_mask;

// Segment selectors / descriptor types
const CS_SELECTOR: u16 = 0x8;
const SS_SELECTOR: u16 = 0x10;
const CS_TYPE: u8 = 0xB;
const SS_TYPE: u8 = 0x3;

pub const GUEST_BASE: GuestAddress = GuestAddress(0);

pub struct KvmVm {
    vm_fd: kvm_ioctls::VmFd,
    next_vcpu_id: AtomicUsize,
    /// Host pointer to the 128 GB DAX window (anonymous mmap, MAP_NORESERVE).
    /// Written once in initialize_memory, read thereafter.
    dax_host_ptr: AtomicPtr<u8>,
}

impl KvmVm {}

impl VirtBackend for KvmVm {
    type VCpuType = KvmVCpu;

    fn new(_info: &VmCreateInfo) -> Result<Self> {
        let kvm = kvm_ioctls::Kvm::new()?;
        let vm_fd = kvm.create_vm()?;
        Ok(Self {
            vm_fd,
            next_vcpu_id: AtomicUsize::new(0),
            dax_host_ptr: AtomicPtr::new(core::ptr::null_mut()),
        })
    }

    fn dax_host_ptr(&self) -> *mut u8 {
        self.dax_host_ptr.load(Ordering::Acquire)
    }

    fn initialize_memory(&self, mem: &GuestMemoryMmap<()>) -> Result<()> {
        // PML4 entries — each points to a PDPT table
        for i in 0..DIRECT_MAP_PML4_ENTRIES_COUNT {
            let entry_val = (DIRECT_MAP_PDPT.as_u64() + i as u64 * PAGE_TABLE_SIZE as u64)
                | PTE_PRESENT
                | PTE_RW;
            let entry_addr =
                GuestAddress(DIRECT_MAP_PML4.as_u64() + ((DIRECT_MAP_PML4_OFFSET + i) * 8) as u64);
            mem.write_slice(&entry_val.to_le_bytes(), entry_addr)?;
        }

        // PDPT entries — 1GB huge pages (PTE_PS at PDPT level), no PD tables needed
        for i in 0..DIRECT_MAP_PDPT_COUNT * PAGE_TABLE_ENTRIES {
            let phys = i as u64 * HUGE_PAGE_SIZE_1G as u64;
            let entry_val = phys | PTE_PRESENT | PTE_RW | PTE_PS;
            let entry_addr = GuestAddress(DIRECT_MAP_PDPT.as_u64() + (i * 8) as u64);
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

        // Register the guest memory region with KVM (slot 0).
        let guest_memory_size = mem.last_addr().0 + 1;

        unsafe {
            self.vm_fd
                .set_user_memory_region(kvm_userspace_memory_region {
                    slot: 0,
                    guest_phys_addr: GUEST_BASE.0,
                    memory_size: guest_memory_size,
                    userspace_addr: mem.get_host_address(GUEST_BASE).unwrap() as u64,
                    flags: 0,
                })?;
        }

        // Allocate the 128 GB DAX window on the host and register as KVM memslot 1.
        // MAP_NORESERVE so we don't commit swap for the full 128 GB upfront.
        // SAFETY: mmap with MAP_ANONYMOUS|MAP_PRIVATE|MAP_NORESERVE and prot RW
        // returns a valid pointer or MAP_FAILED (-1).
        let dax_ptr = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                DAX_WINDOW_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANONYMOUS | libc::MAP_PRIVATE | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if dax_ptr == libc::MAP_FAILED {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        let dax_ptr = dax_ptr as *mut u8;

        // SAFETY: dax_ptr is a valid host mapping of DAX_WINDOW_SIZE bytes.
        unsafe {
            self.vm_fd
                .set_user_memory_region(kvm_userspace_memory_region {
                    slot: 1,
                    guest_phys_addr: DAX_WINDOW_BASE.as_u64(),
                    memory_size: DAX_WINDOW_SIZE as u64,
                    userspace_addr: dax_ptr as u64,
                    flags: 0,
                })?;
        }

        self.dax_host_ptr.store(dax_ptr, Ordering::Release);

        Ok(())
    }

    fn create_vcpu(
        &self,
        devices: Arc<Mutex<DeviceRegistry>>,
        mem: Arc<GuestMemoryMmap<()>>,
    ) -> Result<Self::VCpuType> {
        let id = self.next_vcpu_id.fetch_add(1, Ordering::SeqCst);
        let fd = self.vm_fd.create_vcpu(id as u64)?;

        Ok(KvmVCpu::new(fd, devices, mem))
    }
}

pub struct KvmVCpu {
    fd: kvm_ioctls::VcpuFd,
    devices: Arc<Mutex<DeviceRegistry>>,
    mem: Arc<GuestMemoryMmap<()>>,
}

impl KvmVCpu {
    pub fn new(
        fd: kvm_ioctls::VcpuFd,
        devices: Arc<Mutex<DeviceRegistry>>,
        mem: Arc<GuestMemoryMmap<()>>,
    ) -> Self {
        Self { fd, devices, mem }
    }
}

impl KvmVCpu {
    /// Enable guest debugging with software breakpoints and optional single-step.
    fn set_debug_mode(&self, single_step: bool) -> Result<()> {
        let mut control = KVM_GUESTDBG_ENABLE | KVM_GUESTDBG_USE_SW_BP;
        if single_step {
            control |= KVM_GUESTDBG_SINGLESTEP;
        }
        let debug = kvm_guest_debug {
            control,
            pad: 0,
            arch: Default::default(),
        };
        self.fd.set_guest_debug(&debug)?;
        Ok(())
    }

    /// Read the current register state into our RegisterFile.
    fn read_registers(&self) -> Result<RegisterFile> {
        let regs = self.fd.get_regs()?;
        let sregs = self.fd.get_sregs()?;
        Ok(RegisterFile {
            rax: regs.rax,
            rbx: regs.rbx,
            rcx: regs.rcx,
            rdx: regs.rdx,
            rsi: regs.rsi,
            rdi: regs.rdi,
            rbp: regs.rbp,
            rsp: regs.rsp,
            r8: regs.r8,
            r9: regs.r9,
            r10: regs.r10,
            r11: regs.r11,
            r12: regs.r12,
            r13: regs.r13,
            r14: regs.r14,
            r15: regs.r15,
            rip: regs.rip,
            eflags: regs.rflags as u32,
            cs: sregs.cs.selector as u32,
            ss: sregs.ss.selector as u32,
            ds: sregs.ds.selector as u32,
            es: sregs.es.selector as u32,
            fs: sregs.fs.selector as u32,
            gs: sregs.gs.selector as u32,
        })
    }

    fn get_cr3(&self) -> Result<u64> {
        Ok(self.fd.get_sregs()?.cr3)
    }
}

impl VCpu for KvmVCpu {
    fn tsc_khz(&self) -> u32 {
        self.fd.get_tsc_khz().unwrap_or(0)
    }

    fn init(&mut self, entry_point: u64) -> Result<()> {
        // Mask AVX/AVX2/AVX-512/FMA from the guest CPUID before the first
        // KVM_RUN. We do not enable CR4.OSXSAVE or set XCR0, so the guest
        // cannot legally execute AVX instructions. Masking the CPUID bits
        // keeps glibc IFUNC resolvers on the SSE2 baseline. See
        // docs/glibc-support-design.md §5.
        // TODO: Hoist CPUID setup into KvmVm::new and pass the masked CpuId through
        // to each vCPU. Currently we re-open /dev/kvm here, which is a layering
        // inversion. See round 2 review issue W1.
        let kvm = kvm_ioctls::Kvm::new()?;
        let mut cpuid = kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)?;
        cpuid_mask::apply(cpuid.as_mut_slice());
        self.fd.set_cpuid2(&cpuid)?;

        // General purpose registers:
        // - RIP: instruction pointer where the guest will start executing
        // - RSP: stack pointer inside guest memory
        // - RFLAGS: set the reserved bit required by x86
        let mut regs = self.fd.get_regs()?;
        // Start executing at the ELF entry point supplied by the kernel image.
        regs.rip = entry_point;
        // _start is entered without a CALL frame; keep SysV ABI expectation
        // (RSP % 16 == 8 on function entry) so local variables that require
        // 16-byte alignment remain aligned after prologue.
        regs.rsp = KERNEL_STACK.to_virtual(&DirectMap).as_u64() - 8;
        regs.rflags = RFLAGS_RESERVED; // required reserved bit
        self.fd.set_regs(&regs)?;

        let mut sregs = self.fd.get_sregs()?;
        sregs.cr3 = DIRECT_MAP_PML4.as_u64(); // CR3 = physical address of the PML4 (page-table root)

        // CR4.OSXSAVE is intentionally NOT set. The CPUID mask in
        // apply_cpuid_avx_mask removes all VEX/EVEX feature bits so glibc
        // IFUNC resolvers stay on the SSE2 baseline. If you set OSXSAVE
        // here, you also need to set XCR0 via xsetbv AND remove the CPUID
        // mask, or you will create a half-broken vCPU. See
        // docs/glibc-support-design.md §5.
        // CR4.PAE must be set to enable physical-address-extension paging required
        // by 64-bit mode page tables.
        sregs.cr4 |= CR4_PAE | CR4_OSFXSR | CR4_OSXMMEXCPT;

        // EFER.LME enables Long Mode; EFER.LMA indicates Long Mode Active.
        // EFER.SCE enables the SYSCALL/SYSRET instructions.
        sregs.efer = EFER_LME | EFER_LMA | EFER_SCE;

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
                VcpuExit::IoOut(0xE9, data) => {
                    use std::io::Write;
                    let stdout = std::io::stdout();
                    let mut lock = stdout.lock();
                    let _ = lock.write_all(data);
                    let _ = lock.flush();
                }
                VcpuExit::MmioRead(addr, data) => {
                    let devs = self.devices.lock().unwrap();
                    devs.handle_mmio_read(addr, data);
                }
                VcpuExit::MmioWrite(addr, data) => {
                    let mut devs = self.devices.lock().unwrap();
                    devs.handle_mmio_write(addr, data, &self.mem);
                }
                VcpuExit::Hlt => return Ok(()),
                VcpuExit::Shutdown => {
                    eprintln!("[vm] SHUTDOWN (triple fault)");
                    let regs = self.fd.get_regs()?;
                    eprintln!("[vm]   RIP={:#018x} RSP={:#018x}", regs.rip, regs.rsp);
                    eprintln!("[vm]   RAX={:#018x} RDI={:#018x}", regs.rax, regs.rdi);
                    return Ok(());
                }
                other => return Err(Error::UnexpectedExit(format!("{:?}", other))),
            }
        }
    }

    fn run_debug(&mut self, dbg: VCpuDebugReceiver) -> Result<()> {
        let mut breakpoints = BreakpointManager::new();
        // Address where we need to single-step past a breakpoint then re-insert
        let mut stepping_past_bp: Option<u64> = false.then_some(0);

        // Enable debug mode (software breakpoints intercepted by KVM)
        self.set_debug_mode(false)?;

        // Start paused — wait for GDB to send Continue
        let mut paused = true;

        loop {
            if paused {
                // Block until GDB sends a command
                let cmd = match dbg.cmd_rx.recv() {
                    Ok(cmd) => cmd,
                    Err(_) => return Ok(()), // channel closed
                };

                match cmd {
                    DebugCommand::Continue => {
                        if stepping_past_bp.is_some() {
                            // Need to single-step one instruction past the breakpoint
                            self.set_debug_mode(true)?;
                        } else {
                            self.set_debug_mode(false)?;
                        }
                        paused = false;
                    }
                    DebugCommand::SingleStep => {
                        // If stepping past a bp, we already restored the byte
                        self.set_debug_mode(true)?;
                        // Clear stepping_past_bp so the step reports to GDB
                        stepping_past_bp = None;
                        paused = false;
                    }
                    DebugCommand::ReadRegisters => {
                        let regs = self.read_registers()?;
                        dbg.event_tx.send(DebugEvent::Registers(regs)).ok();
                        continue;
                    }
                    DebugCommand::ReadMemory { addr, len } => {
                        let cr3 = self.get_cr3()?;
                        match read_guest_memory(&self.mem, cr3, addr, len) {
                            Ok(data) => dbg.event_tx.send(DebugEvent::Memory(data)).ok(),
                            Err(e) => dbg.event_tx.send(DebugEvent::Error(e.to_string())).ok(),
                        };
                        continue;
                    }
                    DebugCommand::WriteMemory { addr, data } => {
                        let cr3 = self.get_cr3()?;
                        match write_guest_memory(&self.mem, cr3, addr, &data) {
                            Ok(()) => dbg.event_tx.send(DebugEvent::Ok).ok(),
                            Err(e) => dbg.event_tx.send(DebugEvent::Error(e.to_string())).ok(),
                        };
                        continue;
                    }
                    DebugCommand::InsertSwBreakpoint(addr) => {
                        let cr3 = self.get_cr3()?;
                        match breakpoints.insert_sw(addr, &self.mem, cr3) {
                            Ok(()) => dbg.event_tx.send(DebugEvent::Ok).ok(),
                            Err(e) => dbg.event_tx.send(DebugEvent::Error(e.to_string())).ok(),
                        };
                        continue;
                    }
                    DebugCommand::RemoveSwBreakpoint(addr) => {
                        let cr3 = self.get_cr3()?;
                        match breakpoints.remove_sw(addr, &self.mem, cr3) {
                            Ok(()) => dbg.event_tx.send(DebugEvent::Ok).ok(),
                            Err(e) => dbg.event_tx.send(DebugEvent::Error(e.to_string())).ok(),
                        };
                        continue;
                    }
                    DebugCommand::Detach => {
                        // Remove all breakpoints and resume
                        let cr3 = self.get_cr3()?;
                        breakpoints.remove_all(&self.mem, cr3);
                        // Disable debug mode
                        let debug = kvm_guest_debug {
                            control: 0,
                            pad: 0,
                            arch: Default::default(),
                        };
                        self.fd.set_guest_debug(&debug)?;
                        // Run normally until exit
                        return self.run();
                    }
                    DebugCommand::Kill => {
                        return Ok(());
                    }
                }
                continue;
            }

            // Not paused — run the vCPU
            match self.fd.run()? {
                VcpuExit::IoOut(0xE9, data) => {
                    use std::io::Write;
                    let stdout = std::io::stdout();
                    let mut lock = stdout.lock();
                    let _ = lock.write_all(data);
                    let _ = lock.flush();
                }
                VcpuExit::MmioRead(addr, data) => {
                    let devs = self.devices.lock().unwrap();
                    devs.handle_mmio_read(addr, data);
                }
                VcpuExit::MmioWrite(addr, data) => {
                    let mut devs = self.devices.lock().unwrap();
                    devs.handle_mmio_write(addr, data, &self.mem);
                }
                VcpuExit::Debug(debug_exit) => {
                    // Check if we were single-stepping past a breakpoint
                    if let Some(bp_addr) = stepping_past_bp.take() {
                        // Re-insert the breakpoint we temporarily removed
                        let cr3 = self.get_cr3()?;
                        let _ = breakpoints.reinstate_sw(bp_addr, &self.mem, cr3);
                        // Continue running (this single-step was internal, not user-requested)
                        self.set_debug_mode(false)?;
                        continue;
                    }

                    let pc = debug_exit.pc;
                    paused = true;

                    if debug_exit.exception == 3 && breakpoints.has_sw(pc) {
                        // Software breakpoint hit.
                        // RIP points to the int3 byte. We need to set up
                        // "step past breakpoint" for when Continue is issued.
                        // Temporarily restore original byte and single-step.
                        let cr3 = self.get_cr3()?;
                        let _ = breakpoints.suspend_sw(pc, &self.mem, cr3);
                        stepping_past_bp = Some(pc);
                        dbg.event_tx
                            .send(DebugEvent::Stopped(StopReason::Breakpoint))
                            .ok();
                    } else if debug_exit.exception == 1 {
                        // Single-step completed (user-requested)
                        dbg.event_tx
                            .send(DebugEvent::Stopped(StopReason::SingleStep))
                            .ok();
                    } else {
                        // Other debug exception
                        dbg.event_tx
                            .send(DebugEvent::Stopped(StopReason::Signal(5)))
                            .ok();
                    }
                }
                VcpuExit::Hlt => {
                    dbg.event_tx
                        .send(DebugEvent::Stopped(StopReason::Exited))
                        .ok();
                    return Ok(());
                }
                VcpuExit::Shutdown => {
                    eprintln!("[vm] SHUTDOWN (triple fault)");
                    let regs = self.fd.get_regs()?;
                    eprintln!("[vm]   RIP={:#018x} RSP={:#018x}", regs.rip, regs.rsp);
                    eprintln!("[vm]   RAX={:#018x} RDI={:#018x}", regs.rax, regs.rdi);
                    dbg.event_tx
                        .send(DebugEvent::Stopped(StopReason::Signal(11)))
                        .ok();
                    paused = true;
                }
                VcpuExit::Intr => {
                    // Signal interrupted KVM_RUN — check for pending commands
                    match dbg.cmd_rx.try_recv() {
                        Ok(DebugCommand::Kill) => return Ok(()),
                        _ => {} // re-enter KVM_RUN
                    }
                }
                other => return Err(Error::UnexpectedExit(format!("{:?}", other))),
            }
        }
    }
}
