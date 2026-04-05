use std::path::PathBuf;

use clap::Args;
use sumi_vm::{Hypervisor, VmCreateInfo, run_sumi_vm};

#[derive(Debug, Args)]
pub struct RunCommand {
    /// Path to the program binary that will be loaded into the VM.
    #[arg(value_name = "PROGRAM")]
    program: PathBuf,

    /// Host directory to share with the guest as its root filesystem.
    #[arg(long = "share", value_name = "DIR")]
    share_dir: Option<PathBuf>,

    /// Path to the user program, relative to the share root.
    #[arg(long = "run", value_name = "PATH")]
    run_path: Option<String>,
}

impl RunCommand {
    pub fn execute(self) -> Result<(), sumi_vm::error::Error> {
        let info = VmCreateInfo {
            vcpu_count: 1,
            hypervisor: Hypervisor::Kvm,
            mem_size: 2 << 30,
            kernel_path: self.program,
            share_dir: self.share_dir,
            run_path: self.run_path,
        };

        run_sumi_vm(&info)
    }
}
