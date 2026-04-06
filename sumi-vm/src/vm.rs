use goblin::elf::Elf;
use goblin::elf::program_header::PT_LOAD;
use sumi_abi::arch::layout::{INTERP_LOAD_BASE, PIE_LOAD_BASE};
use sumi_abi::layout::{KERNEL_CODE_PHYS, KERNEL_CODE_SIZE, KERNEL_CODE_VIRT};
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

use crate::debug::{self, GdbServer, VCpuDebugReceiver};
use crate::devices::DeviceRegistry;
use crate::error::{Error, Result};
use std::{
    fmt::{self, Display},
    fs::File,
    io::Write as _,
    path::PathBuf,
    process::Command,
    sync::{Arc, Mutex},
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
    pub kernel_path: PathBuf,
    pub share_dir: Option<PathBuf>,
    pub run_path: Option<String>,
    pub gdb_port: Option<u16>,
}

pub trait VirtBackend: Sized {
    type VCpuType: VCpu;

    fn new(info: &VmCreateInfo) -> Result<Self>;

    fn initialize_memory(&self, mem: &GuestMemoryMmap<()>) -> Result<()>;

    /// Return the host pointer for the DAX window, if one was set up.
    fn dax_host_ptr(&self) -> *mut u8;

    fn create_vcpu(
        &self,
        devices: Arc<Mutex<DeviceRegistry>>,
        mem: Arc<GuestMemoryMmap<()>>,
    ) -> Result<Self::VCpuType>;
}

/// Symbol file info for GDB: host path + .text load address.
#[derive(Clone)]
struct SymbolFile {
    host_path: PathBuf,
    text_load_addr: u64,
}

/// Info about user binaries for GDB auto-loading.
#[derive(Clone)]
struct UserDebugInfo {
    binary: SymbolFile,
    interpreter: Option<SymbolFile>,
}

pub struct SumiVm<Backend: VirtBackend + 'static> {
    mem: Arc<GuestMemoryMmap<()>>,
    _backend: Backend,
    kernel_entry: u64,
    vcpus: Vec<Backend::VCpuType>,
    gdb_port: Option<u16>,
    kernel_path: PathBuf,
    user_debug: Option<UserDebugInfo>,
}

impl<Backend: VirtBackend + 'static> SumiVm<Backend> {
    pub fn new(info: &VmCreateInfo) -> Result<Self> {
        let backend = Backend::new(info)?;

        let mem = Arc::new(
            GuestMemoryMmap::from_ranges(&[(GuestAddress(0), info.mem_size + KERNEL_CODE_SIZE)])?,
        );

        backend.initialize_memory(&mem)?;

        let dax_host_ptr = backend.dax_host_ptr();
        let devices = Arc::new(Mutex::new(DeviceRegistry::new(
            info.share_dir.as_deref(),
            dax_host_ptr,
        )));

        let mut vcpus = Vec::new();
        for _ in 0..info.vcpu_count {
            vcpus.push(backend.create_vcpu(Arc::clone(&devices), Arc::clone(&mem))?);
        }

        let kernel_entry = Self::load_elf(&mem, &info.kernel_path)?;
        let tsc_khz = vcpus.first().map(|v| v.tsc_khz()).unwrap_or(0);
        Self::write_boot_info(&mem, info, tsc_khz)?;

        // Resolve user binary path and load address for GDB symbol loading.
        let user_debug = match (&info.share_dir, &info.run_path) {
            (Some(share), Some(run)) => {
                let rel = run.strip_prefix('/').unwrap_or(run);
                let host_path = share.join(rel);
                Self::resolve_user_debug(share, &host_path).ok()
            }
            _ => None,
        };

        // Always emit /tmp/perf-<pid>.map for `perf record`. The cost is a one-time
        // ELF parse at startup and there is no runtime overhead.
        Self::write_perf_map(&info.kernel_path, &info.share_dir, &info.run_path)?;

        Ok(Self {
            mem,
            vcpus,
            _backend: backend,
            kernel_entry,
            gdb_port: info.gdb_port,
            kernel_path: info.kernel_path.clone(),
            user_debug,
        })
    }

    pub fn run(self) -> Result<()> {
        if let Some(port) = self.gdb_port {
            // Capture GDB launch info before moving fields out of self.
            let kernel_path = self.kernel_path.clone();
            let user_debug = self.user_debug.clone();

            // Debug mode: single vCPU with GDB stub
            let (cmd_tx, event_rx, vcpu_dbg) = debug::create_debug_channels();
            let mem = Arc::clone(&self.mem);

            let mut vcpus = self.vcpus;
            let mut cpu = vcpus.remove(0);
            let kernel_entry = self.kernel_entry;

            // Spawn vCPU thread
            let vcpu_thread = thread::spawn(move || {
                cpu.init(kernel_entry)?;
                cpu.run_debug(vcpu_dbg)
            });

            // Spawn GDB stub thread
            let stub_thread = thread::spawn(move || {
                let server = GdbServer::new(cmd_tx, event_rx, mem);
                server.run(port);
            });

            // Launch GDB as a child process
            Self::launch_gdb(&kernel_path, user_debug.as_ref(), port);

            // Wait for threads to finish
            let _ = stub_thread.join();
            match vcpu_thread.join() {
                Ok(r) => r?,
                Err(_) => eprintln!("[vm] vCPU thread panicked"),
            }
        } else {
            // Normal (non-debug) mode
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
        }

        Ok(())
    }

    /// Parse user ELF to find .text load address and interpreter info.
    fn resolve_user_debug(share_dir: &PathBuf, host_path: &PathBuf) -> Result<UserDebugInfo> {
        let data = std::fs::read(host_path)?;
        let elf = Elf::parse(&data)?;

        let base: u64 = match elf.header.e_type {
            goblin::elf::header::ET_EXEC => 0,
            goblin::elf::header::ET_DYN => PIE_LOAD_BASE as u64,
            _ => 0,
        };

        let binary = SymbolFile {
            host_path: host_path.clone(),
            text_load_addr: base + Self::find_text_vaddr(&elf),
        };

        // Check for dynamic linker (PT_INTERP)
        let interpreter = elf.interpreter.and_then(|interp_path| {
            let rel = interp_path.strip_prefix('/').unwrap_or(interp_path);
            let interp_host = share_dir.join(rel);
            let interp_data = std::fs::read(&interp_host).ok()?;
            let interp_elf = Elf::parse(&interp_data).ok()?;
            Some(SymbolFile {
                host_path: interp_host,
                text_load_addr: INTERP_LOAD_BASE + Self::find_text_vaddr(&interp_elf),
            })
        });

        Ok(UserDebugInfo {
            binary,
            interpreter,
        })
    }

    fn find_text_vaddr(elf: &Elf) -> u64 {
        elf.section_headers
            .iter()
            .find(|sh| elf.shdr_strtab.get_at(sh.sh_name) == Some(".text"))
            .map(|sh| sh.sh_addr)
            .unwrap_or(0)
    }

    /// Extract function symbols from an ELF and append them to the perf map file.
    fn append_elf_symbols(f: &mut File, elf_data: &[u8], base: u64) -> Result<()> {
        use goblin::elf::sym::STT_FUNC;

        let elf = Elf::parse(elf_data)?;

        // Collect from both .symtab and .dynsym
        let all_syms = elf.syms.iter().chain(elf.dynsyms.iter());
        for sym in all_syms {
            if sym.st_type() == STT_FUNC && sym.st_value != 0 {
                if let Some(name) = elf.strtab.get_at(sym.st_name)
                    .or_else(|| elf.dynstrtab.get_at(sym.st_name))
                {
                    if name.is_empty() {
                        continue;
                    }
                    let addr = base + sym.st_value;
                    // Use st_size if known, otherwise default to 1
                    let size = if sym.st_size > 0 { sym.st_size } else { 1 };
                    writeln!(f, "{:x} {:x} {}", addr, size, name)
                        .map_err(Error::Io)?;
                }
            }
        }
        Ok(())
    }

    /// Write /tmp/perf-<pid>.map with symbols from kernel + user binaries.
    fn write_perf_map(
        kernel_path: &PathBuf,
        share_dir: &Option<PathBuf>,
        run_path: &Option<String>,
    ) -> Result<()> {
        let pid = std::process::id();
        let path = format!("/tmp/perf-{}.map", pid);
        let mut f = File::create(&path).map_err(Error::Io)?;

        // Kernel symbols (base = 0, addresses are already virtual)
        let kernel_data = std::fs::read(kernel_path)?;
        Self::append_elf_symbols(&mut f, &kernel_data, 0)?;

        // User binary symbols
        if let (Some(share), Some(run)) = (share_dir, run_path) {
            let rel = run.strip_prefix('/').unwrap_or(run);
            let user_path = share.join(rel);
            if let Ok(user_data) = std::fs::read(&user_path) {
                let elf = Elf::parse(&user_data)?;
                let base: u64 = match elf.header.e_type {
                    goblin::elf::header::ET_EXEC => 0,
                    goblin::elf::header::ET_DYN => PIE_LOAD_BASE as u64,
                    _ => 0,
                };
                Self::append_elf_symbols(&mut f, &user_data, base)?;

                // Interpreter symbols
                if let Some(interp_path) = elf.interpreter {
                    let interp_rel = interp_path.strip_prefix('/').unwrap_or(interp_path);
                    let interp_host = share.join(interp_rel);
                    if let Ok(interp_data) = std::fs::read(&interp_host) {
                        Self::append_elf_symbols(&mut f, &interp_data, INTERP_LOAD_BASE)?;
                    }
                }
            }
        }

        eprintln!("[perf] wrote {}", path);
        Ok(())
    }

    /// Build GDB command-line args and spawn GDB interactively.
    fn launch_gdb(kernel_path: &PathBuf, user_debug: Option<&UserDebugInfo>, port: u16) {
        let kernel_path_str = kernel_path.display().to_string();

        let mut args: Vec<String> = vec![
            "-q".into(),
            "-ex".into(), "set confirm off".into(),
            "-ex".into(), format!("file {}", kernel_path_str),
            "-ex".into(), format!("target remote :{}", port),
        ];

        // If there's a user binary, set up automatic symbol loading
        if let Some(info) = user_debug {
            // Break at jump_to_user_asm — at this point the kernel has loaded
            // and mapped the user ELF, so we can safely add symbols.
            args.extend([
                "-ex".into(), "break jump_to_user_asm".into(),
                "-ex".into(), "continue".into(),
                "-ex".into(), "delete breakpoints".into(),
                "-ex".into(),
                format!(
                    "add-symbol-file {} {:#x}",
                    info.binary.host_path.display(),
                    info.binary.text_load_addr
                ),
            ]);
            if let Some(ref interp) = info.interpreter {
                args.extend([
                    "-ex".into(),
                    format!(
                        "add-symbol-file {} {:#x}",
                        interp.host_path.display(),
                        interp.text_load_addr
                    ),
                ]);
            }
        }

        eprintln!("[gdb] launching GDB...");
        match Command::new("gdb").args(&args).status() {
            Ok(status) => {
                if !status.success() {
                    eprintln!("[gdb] GDB exited with {}", status);
                }
            }
            Err(e) => {
                eprintln!("[gdb] failed to launch GDB: {}", e);
                eprintln!("[gdb] connect manually: gdb -ex 'target remote :{}'", port);
                // Fall back to waiting for the stub thread
                std::thread::park();
            }
        }
    }

    fn write_boot_info(mem: &GuestMemoryMmap<()>, info: &VmCreateInfo, tsc_khz: u32) -> Result<()> {
        use sumi_abi::arch::layout::{BOOT_INFO_ADDR, BOOT_INFO_MAX_SIZE};
        use sumi_abi::boot_info::*;
        use std::time::{SystemTime, UNIX_EPOCH};

        let mut flags = 0u32;
        let mut path_bytes: &[u8] = &[];

        if let Some(ref path) = info.run_path {
            flags |= BOOT_INFO_FLAG_HAS_RUN_PATH;
            path_bytes = path.as_bytes();
        }

        let header_size = core::mem::size_of::<BootInfo>();
        let total_size = header_size + path_bytes.len();
        if total_size > BOOT_INFO_MAX_SIZE {
            return Err(Error::InvalidVmConfig(format!(
                "boot info too large: {} bytes (max {})",
                total_size, BOOT_INFO_MAX_SIZE
            )));
        }

        let (wall_clock_sec, wall_clock_nsec) = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| (d.as_secs(), d.subsec_nanos()))
            .unwrap_or((0, 0));

        let mut rng_seed = [0u8; 32];
        // SAFETY: rng_seed is a valid 32-byte buffer. getrandom may return fewer
        // than 32 bytes (or -1 on error); the remainder stays zeroed, which is
        // non-fatal — the kernel fallback RNG can tolerate a partially-seeded buffer.
        let ret = unsafe {
            libc::getrandom(
                rng_seed.as_mut_ptr() as *mut libc::c_void,
                32,
                0,
            )
        };
        if ret != 32 {
            // Non-fatal: leave whatever bytes getrandom wrote (possibly none).
        }

        let boot_info = BootInfo {
            magic: BOOT_INFO_MAGIC,
            version: BOOT_INFO_VERSION,
            flags,
            _reserved: 0,
            mem_size: info.mem_size as u64,
            run_path_offset: header_size as u32,
            run_path_len: path_bytes.len() as u32,
            tsc_freq_khz: tsc_khz,
            wall_clock_sec,
            wall_clock_nsec,
            rng_seed,
        };

        // SAFETY: BootInfo is repr(C) with no padding holes that matter.
        let struct_bytes: &[u8] = unsafe {
            core::slice::from_raw_parts(
                &boot_info as *const _ as *const u8,
                header_size,
            )
        };
        mem.write_slice(struct_bytes, GuestAddress(BOOT_INFO_ADDR.as_u64()))?;

        if !path_bytes.is_empty() {
            mem.write_slice(
                path_bytes,
                GuestAddress(BOOT_INFO_ADDR.as_u64() + header_size as u64),
            )?;
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
            if phys_end == 0
                || phys_end - 1 > guest_memory_end
                || phys_end > KERNEL_CODE_SIZE as u64
            {
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
    fn run_debug(&mut self, dbg: VCpuDebugReceiver) -> Result<()>;
    fn tsc_khz(&self) -> u32;
}
