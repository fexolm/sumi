use goblin::elf::Elf;
use goblin::elf::program_header::PT_LOAD;
use sumi_abi::layout::{KERNEL_CODE_PHYS, KERNEL_CODE_SIZE, KERNEL_CODE_VIRT};
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

use crate::error::{Error, Result};
use std::{
    fmt::{self, Display}, path::PathBuf, thread
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Hypervisor {
    Kvm,
}

impl Display for Hypervisor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Hypervisor::Kvm => write!(f, "KVM"),
        }
    }
}

pub struct VmCreateInfo {
    pub vcpu_count: usize,
    pub hypervisor: Hypervisor,
    pub mem_size: usize,
    pub kernel_path: PathBuf,
}

pub trait VirtBackend: Sized {
    type VCpuType: VCpu;

    fn new(info: &VmCreateInfo) -> Result<Self>;

    fn initialize_memory(&self, mem: &GuestMemoryMmap<()>) -> Result<()>;

    fn create_vcpu(&self) -> Result<Self::VCpuType>;
}

pub struct SumiVm<Backend: VirtBackend + 'static> {
    _mem: GuestMemoryMmap<()>,
    _backend: Backend,
    kernel_entry: u64,
    vcpus: Vec<Backend::VCpuType>,
}

impl<Backend: VirtBackend + 'static> SumiVm<Backend> {
    pub fn new(info: &VmCreateInfo) -> Result<Self> {
        let backend = Backend::new(info)?;

        let mut vcpus = Vec::new();

        for _ in 0..info.vcpu_count {
            vcpus.push(backend.create_vcpu()?);
        }

        let mem: GuestMemoryMmap<()> =
            GuestMemoryMmap::from_ranges(&[(GuestAddress(0), info.mem_size + KERNEL_CODE_SIZE)])?;

        backend.initialize_memory(&mem)?;

        let kernel_entry = Self::load_elf(&mem, &info.kernel_path)?;

        Ok(Self {
            _mem: mem,
            vcpus,
            _backend: backend,
            kernel_entry,
        })
    }

    pub fn run(self) -> Result<()> {
        let threads = self
            .vcpus
            .into_iter()
            .map(|mut cpu| {
                let kernel_entry = self.kernel_entry;
                thread::spawn(move || {
                    cpu.init(kernel_entry)?;
                    cpu.run()
                })
            })
            .collect::<Vec<_>>();

        for t in threads {
            t.join().unwrap()?;
        }

        Ok(())
    }

    fn load_elf(mem: &GuestMemoryMmap<()>, kernel_path: &PathBuf) -> Result<u64> {
        let data = std::fs::read(kernel_path)?;
        let elf = Elf::parse(&data)?;
        let guest_memory_end = mem.last_addr().0;
        let mut first_load_vaddr = None;
        let mut first_load_paddr = None;

        for ph in &elf.program_headers {
            if ph.p_type != PT_LOAD {
                continue;
            }

            first_load_vaddr.get_or_insert(ph.p_vaddr);
            first_load_paddr.get_or_insert(ph.p_paddr);

            let file_offset = ph.p_offset as usize;
            let filesz = ph.p_filesz as usize;
            let memsz = ph.p_memsz as usize;

            if ph.p_vaddr < KERNEL_CODE_VIRT.as_u64() {
                return Err(Error::Parsing(goblin::error::Error::Malformed(format!(
                    "Program header with p_vaddr {:#x} is below kernel base {:#x}",
                    ph.p_vaddr,
                    KERNEL_CODE_VIRT.as_u64()
                ))));
            }

            if ph.p_paddr < KERNEL_CODE_PHYS.as_u64() {
                return Err(Error::Parsing(goblin::error::Error::Malformed(format!(
                    "Program header with p_paddr {:#x} is below kernel base {:#x}",
                    ph.p_paddr,
                    KERNEL_CODE_PHYS.as_u64()
                ))));
            }

            if filesz > memsz {
                return Err(Error::Parsing(goblin::error::Error::Malformed(format!(
                    "Program header with p_paddr {:#x} has filesz {:#x} larger than memsz {:#x}",
                    ph.p_paddr, filesz, memsz
                ))));
            }

            let file_end = file_offset.checked_add(filesz).ok_or_else(|| {
                Error::Parsing(goblin::error::Error::Malformed(format!(
                    "Program header with file offset {:#x} and filesz {:#x} overflows",
                    ph.p_offset, filesz
                )))
            })?;
            if file_end > data.len() {
                return Err(Error::Parsing(goblin::error::Error::Malformed(format!(
                    "Program header with file offset {:#x} and filesz {:#x} is out of file bounds",
                    ph.p_offset, filesz
                ))));
            }

            let phys_end = ph.p_paddr.checked_add(memsz as u64).ok_or_else(|| {
                Error::Parsing(goblin::error::Error::Malformed(format!(
                    "Program header with p_paddr {:#x} and memsz {:#x} overflows",
                    ph.p_paddr, memsz
                )))
            })?;
            if phys_end == 0 || phys_end - 1 > guest_memory_end || phys_end > KERNEL_CODE_SIZE as u64 {
                return Err(Error::Parsing(goblin::error::Error::Malformed(format!(
                    "Program header with p_paddr {:#x} and memsz {:#x} is out of guest memory bounds",
                    ph.p_paddr, memsz
                ))));
            }

            // copy the initialized data from the file
            mem.write_slice(
                &data[file_offset..file_end],
                GuestAddress(ph.p_paddr),
            )?;

            // zero the remainder of the segment if any
            if memsz > filesz {
                let zero_addr = GuestAddress(ph.p_paddr + filesz as u64);
                let zero_buf = vec![0u8; memsz - filesz];
                mem.write_slice(&zero_buf, zero_addr)?;
            }
        }

        let first_load_vaddr = first_load_vaddr.ok_or_else(|| {
            Error::Parsing(goblin::error::Error::Malformed(
                "ELF does not contain any PT_LOAD program headers".into(),
            ))
        })?;
        if first_load_vaddr != KERNEL_CODE_VIRT.as_u64() {
            return Err(Error::Parsing(goblin::error::Error::Malformed(format!(
                "First PT_LOAD p_vaddr {:#x} does not match kernel base {:#x}",
                first_load_vaddr,
                KERNEL_CODE_VIRT.as_u64()
            ))));
        }

        let first_load_paddr = first_load_paddr.ok_or_else(|| {
            Error::Parsing(goblin::error::Error::Malformed(
                "ELF does not contain any PT_LOAD program headers".into(),
            ))
        })?;
        if first_load_paddr != KERNEL_CODE_PHYS.as_u64() {
            return Err(Error::Parsing(goblin::error::Error::Malformed(format!(
                "First PT_LOAD p_paddr {:#x} does not match kernel base {:#x}",
                first_load_paddr,
                KERNEL_CODE_PHYS.as_u64()
            ))));
        }

        Ok(elf.entry)
    }
}

pub trait VCpu: Send {
    fn init(&mut self, entry_point: u64) -> Result<()>;
    fn run(&mut self) -> Result<()>;
}
