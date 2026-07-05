use std::path::PathBuf;

use clap::Args;
use sumi_vm::net::gateway::HostForward;
use sumi_vm::{Hypervisor, VmCreateInfo, run_sumi_vm};

fn default_vcpus() -> usize {
    num_cpus::get().clamp(1, 64)
}

#[derive(Debug, Args)]
pub struct RunCommand {
    /// Path to the kernel ELF binary that will be loaded into the VM.
    #[arg(value_name = "KERNEL")]
    program: PathBuf,

    /// Host directory exposed to the guest as its root filesystem.
    #[arg(long = "share", value_name = "DIR", default_value = "/")]
    share_dir: PathBuf,

    /// Path to the user program, interpreted inside the guest's view of
    /// the share root.
    #[arg(long = "run", value_name = "PATH")]
    run_path: Option<String>,

    /// Start GDB stub on this TCP port (e.g. --gdb 1234).
    #[arg(long = "gdb", value_name = "PORT")]
    gdb_port: Option<u16>,

    /// Number of vCPUs (1..=64). Defaults to `num_cpus::get()` clamped.
    /// CPU 0 is the BSP; CPUs 1..N-1 boot as APs and enter the scheduler
    /// idle loop.
    #[arg(long = "vcpus", value_name = "N", default_value_t = default_vcpus())]
    vcpus: usize,

    /// Forward a host TCP port to a guest TCP port through the network
    /// gateway: `tcp:HOST_IP:HOST_PORT-GUEST_IP:GUEST_PORT` (e.g.
    /// `tcp:127.0.0.1:3307-10.0.2.15:3306`). May be repeated.
    #[arg(long = "hostfwd", value_name = "tcp:HOST_IP:HOST_PORT-GUEST_IP:GUEST_PORT")]
    hostfwd: Vec<HostForward>,

    /// Arguments passed to the guest program (its argv[1..]), given after
    /// `--`: `sumi-vm run KERNEL --run /bin/prog -- --flag value`.
    #[arg(last = true, value_name = "GUEST_ARGS")]
    guest_args: Vec<String>,
}

impl RunCommand {
    pub fn execute(self) -> Result<(), sumi_vm::error::Error> {
        // GDB debug mode is BSP-only; reject --vcpus > 1 + --gdb to
        // avoid surprising behaviour (the GDB stub only attaches to
        // vCPU 0 — see vm::run).
        let vcpus = if self.gdb_port.is_some() {
            if self.vcpus > 1 {
                eprintln!("[vm] --gdb forces --vcpus 1");
            }
            1
        } else {
            self.vcpus.clamp(1, 64)
        };

        let info = VmCreateInfo {
            vcpu_count: vcpus,
            hypervisor: Hypervisor::Kvm,
            mem_size: 2 << 30,
            kernel_path: self.program,
            share_dir: Some(self.share_dir),
            run_path: self.run_path,
            run_args: self.guest_args,
            gdb_port: self.gdb_port,
            hostfwd: self.hostfwd,
        };

        let exit_code = run_sumi_vm(&info)?;
        std::process::exit(exit_code);
    }
}
