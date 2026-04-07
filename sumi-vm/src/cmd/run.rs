use std::path::PathBuf;

use clap::Args;
use sumi_vm::{Hypervisor, VmCreateInfo, run_sumi_vm};

#[derive(Debug, Args)]
pub struct RunCommand {
    /// Path to the kernel ELF binary that will be loaded into the VM.
    #[arg(value_name = "KERNEL")]
    program: PathBuf,

    /// Host directory exposed to the guest as its root filesystem.
    /// Defaults to "/" so the guest sees the host filesystem natively
    /// (every absolute path resolves to the same file as on the host),
    /// which means glibc-linked binaries can be run without staging
    /// `ld-linux-x86-64.so.2` and `libc.so.6` into a separate share dir.
    #[arg(long = "share", value_name = "DIR", default_value = "/")]
    share_dir: PathBuf,

    /// Path to the user program, interpreted inside the guest's view of
    /// the share root. With the default `--share /`, this is just the
    /// host's absolute path to the binary (e.g. `/tmp/hello`).
    #[arg(long = "run", value_name = "PATH")]
    run_path: Option<String>,

    /// Start GDB stub on this TCP port (e.g. --gdb 1234).
    #[arg(long = "gdb", value_name = "PORT")]
    gdb_port: Option<u16>,
}

impl RunCommand {
    pub fn execute(self) -> Result<(), sumi_vm::error::Error> {
        let info = VmCreateInfo {
            vcpu_count: 1,
            hypervisor: Hypervisor::Kvm,
            mem_size: 2 << 30,
            kernel_path: self.program,
            share_dir: Some(self.share_dir),
            run_path: self.run_path,
            gdb_port: self.gdb_port,
        };

        run_sumi_vm(&info)
    }
}
