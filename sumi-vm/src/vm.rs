use vm_memory::{GuestAddress, GuestMemoryMmap};

use crate::error::Result;
use std::{
    fmt::{self, Display},
    thread,
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
            GuestMemoryMmap::from_ranges(&[(GuestAddress(0), info.mem_size)])?;

        backend.initialize_memory(&mem)?;

        Ok(Self {
            _mem: mem,
            vcpus,
            _backend: backend,
        })
    }

    pub fn run(self) -> Result<()> {
        let threads = self
            .vcpus
            .into_iter()
            .map(|mut cpu| {
                thread::spawn(move || {
                    cpu.init()?;
                    cpu.run()
                })
            })
            .collect::<Vec<_>>();

        for t in threads {
            t.join().unwrap()?;
        }

        Ok(())
    }
}

pub trait VCpu: Send {
    fn init(&mut self) -> Result<()>;
    fn run(&mut self) -> Result<()>;
}
