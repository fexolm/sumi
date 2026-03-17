use sumi_vm::{Hypervisor, VmCreateInfo, run_sumi_vm};

pub fn main() {
    let info = VmCreateInfo {
        vcpu_count: 1,
        hypervisor: Hypervisor::Kvm,
        mem_size: 2 << 30,
    };
    run_sumi_vm(&info).unwrap()
}
