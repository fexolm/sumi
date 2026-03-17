use crate::vm::Hypervisor;
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
use crate::{arch::x86_64::kvm::KvmVm, vm::SumiVm};
use crate::{
    error::{Error, Result},
    vm::VmCreateInfo,
};

#[cfg(target_arch = "x86_64")]
mod x86_64;

// #[allow(unreachable_patterns)]
// pub fn create_vm(info: &VmCreateInfo) -> Result<Box<dyn VirtBackend>> {
//     match info.hypervisor {
//         #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
//         Hypervisor::Kvm => Ok(Box::new(x86_64::kvm::KvmVm::new(info))),
//         _ => Err(Error::MissingHypervisor(info.hypervisor)),
//     }
// }

pub fn run_sumi_vm(info: &VmCreateInfo) -> Result<()> {
    match info.hypervisor {
        #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
        Hypervisor::Kvm => SumiVm::<KvmVm>::new(info)?.run(),
        _ => Err(Error::MissingHypervisor(info.hypervisor)),
    }
}
