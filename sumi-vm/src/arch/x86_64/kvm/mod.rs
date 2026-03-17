use vm_memory::GuestMemoryMmap;

use crate::{
    error::Result,
    vm::{VCpu, VirtBackend, VmCreateInfo},
};

pub struct KvmVm {}

impl KvmVm {}

impl VirtBackend for KvmVm {
    type VCpuType = KvmVCpu;

    fn new(info: &VmCreateInfo) -> Result<Self> {
        todo!();
    }

    fn initialize_memory(&self, mem: &GuestMemoryMmap<()>) -> Result<()> {
        // initialize kernel memory
        todo!();
    }

    fn create_vcpu(&self) -> Result<Self::VCpuType> {
        todo!()
    }
}

pub struct KvmVCpu {}

impl KvmVCpu {
    pub fn new() -> Self {
        Self {}
    }
}

impl VCpu for KvmVCpu {
    fn init(&mut self) -> Result<()> {
        // setup x64 mode
        todo!()
    }
    fn run(&mut self) -> Result<()> {
        // run loop
        todo!()
    }
}
