use goblin::elf::Elf;
use goblin::elf::program_header::PT_LOAD;
use sumi_abi::arch::layout::{HYPERCALL_MMIO_BASE, INTERP_LOAD_BASE, PIE_LOAD_BASE};
use sumi_abi::hypercall::{HC_KICK_CPU, HC_SHUTDOWN, HYPERCALL_MMIO_SIZE};
use sumi_abi::layout::{KERNEL_CODE_PHYS, KERNEL_CODE_SIZE, KERNEL_CODE_VIRT};
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

use crate::debug::{self, GdbServer, VCpuDebugReceiver};
use crate::devices::DeviceRegistry;
use crate::devices::virtio_net::GatewayChannel;
use crate::error::{Error, Result};
use crate::net::gateway::{self, GatewayHandle};
use std::{
    fmt::{self, Display},
    fs::File,
    io::Write as _,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering},
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

/// Run-loop control verdict returned from `HypercallContext::dispatch_mmio`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HypercallControl {
    /// Re-enter `KVM_RUN`; the hypercall was a kick or flush for a peer.
    Continue,
    /// This vCPU executed `HC_SHUTDOWN`; its run loop must terminate.
    Stop,
}

/// Decoded intent of a hypercall MMIO write. Pure data — no side effects,
/// so the decoder is unit-testable without constructing a `HypercallContext`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HypercallAction {
    /// Wake one specific peer vCPU (the issuing CPU is excluded by decode).
    SignalOne(u32),
    /// Wake every peer in the bitmask (issuing CPU's bit is cleared by decode).
    SignalMask(u64),
    /// Set shutdown + exit code; signal every peer; issuing vCPU stops.
    Shutdown { exit_code: i32 },
    /// Address was outside the hypercall range — fall back to device dispatcher.
    NotHypercall,
    /// Address was in range but data length was wrong, or offset was unknown.
    Malformed,
}

/// Shared per-VM state used by the hypercall MMIO dispatcher inside
/// each vCPU's run loop. Constructed once by `SumiVm::run` and cloned
/// into every vCPU thread's closure via `Arc`.
pub struct HypercallContext {
    /// One slot per vCPU id; entry `i` holds the host pthread tid of
    /// vCPU `i` (or `u64::MAX` if not yet published).
    pub tid_slots: Box<[Arc<AtomicU64>]>,
    /// One sticky wake bit per vCPU. `HC_KICK_CPU` sets this before sending
    /// SIGUSR1 so a kick that races just before a userspace HLT handler is
    /// not lost.
    pub wake_pending: Box<[Arc<AtomicBool>]>,
    /// Set to `true` when any vCPU executes `HC_SHUTDOWN`.
    pub shutdown: Arc<AtomicBool>,
    /// Last guest-supplied exit code. Written by the vCPU that executes
    /// `HC_SHUTDOWN`; read by the supervisor after joining.
    pub exit_code: Arc<AtomicI32>,
}

impl HypercallContext {
    /// Construct a context using the given per-vCPU host-tid slots.
    /// `tid_slots[i]` MUST be the slot for vCPU `i`; ordering is the
    /// caller's responsibility.
    pub fn new(tid_slots: Vec<Arc<AtomicU64>>) -> Self {
        let wake_pending: Vec<Arc<AtomicBool>> = (0..tid_slots.len())
            .map(|_| Arc::new(AtomicBool::new(false)))
            .collect();
        Self {
            tid_slots: tid_slots.into_boxed_slice(),
            wake_pending: wake_pending.into_boxed_slice(),
            shutdown: Arc::new(AtomicBool::new(false)),
            exit_code: Arc::new(AtomicI32::new(0)),
        }
    }

    /// Test-only convenience constructor: allocates `vcpu_count` fresh
    /// sentinel slots so tests don't have to build `Vec<Arc<AtomicU64>>`.
    #[cfg(test)]
    pub fn new_for_test(vcpu_count: usize) -> Self {
        let slots: Vec<Arc<AtomicU64>> = (0..vcpu_count)
            .map(|_| Arc::new(AtomicU64::new(u64::MAX)))
            .collect();
        Self::new(slots)
    }

    /// Construct a single-CPU hypercall context for debug mode (one
    /// vCPU, no peers to signal). Used by `KvmVCpu::run_debug`.
    pub fn single_cpu_for_debug() -> Self {
        Self::new(vec![Arc::new(AtomicU64::new(u64::MAX))])
    }

    /// Pure decoder. No side effects, no `pthread_kill`. Unit-testable.
    ///
    /// Returns `HypercallAction::NotHypercall` when the address is outside
    /// the hypercall MMIO range (caller should fall through to device
    /// dispatcher). Returns `Malformed` for any in-range write that is
    /// syntactically wrong (wrong width, unknown offset).
    pub fn decode(addr: u64, data: &[u8], issuing_cpu_id: u32) -> HypercallAction {
        let base = HYPERCALL_MMIO_BASE.as_u64();
        let end = base + HYPERCALL_MMIO_SIZE as u64;
        if !(base..end).contains(&addr) {
            return HypercallAction::NotHypercall;
        }
        if data.len() != 8 {
            return HypercallAction::Malformed;
        }
        let arg = u64::from_le_bytes(data.try_into().unwrap());
        let offset = (addr - base) as usize;
        match offset {
            HC_KICK_CPU => {
                let target = arg as u32;
                if target == issuing_cpu_id {
                    // Self-kick is a no-op: the issuing CPU is currently running
                    // so it doesn't need a signal to wake up. Map to an empty
                    // signal mask so the applier still returns Continue.
                    HypercallAction::SignalMask(0)
                } else {
                    HypercallAction::SignalOne(target)
                }
            }
            HC_SHUTDOWN => HypercallAction::Shutdown {
                exit_code: arg as u32 as i32,
            },
            _ => HypercallAction::Malformed,
        }
    }

    /// Apply a decoded action: perform `pthread_kill`, store flags,
    /// and return the run-loop control verdict.
    ///
    /// Order for `Shutdown`: store exit_code (Release) → store shutdown
    /// (Release) → broadcast SIGUSR1. This ensures every AP that observes
    /// the SIGUSR1 and re-checks the shutdown flag will see it as `true`.
    pub fn apply(&self, action: HypercallAction, issuing_cpu_id: u32) -> HypercallControl {
        match action {
            HypercallAction::NotHypercall | HypercallAction::Malformed => {
                HypercallControl::Continue
            }
            HypercallAction::SignalOne(target) => {
                self.signal_cpu(target);
                HypercallControl::Continue
            }
            HypercallAction::SignalMask(mask) => {
                for cpu in 0..self.tid_slots.len() {
                    if (mask >> cpu) & 1 != 0 {
                        self.signal_cpu(cpu as u32);
                    }
                }
                HypercallControl::Continue
            }
            HypercallAction::Shutdown { exit_code } => {
                self.exit_code.store(exit_code, Ordering::Release);
                self.shutdown.store(true, Ordering::Release);
                // Signal every peer vCPU. The issuing vCPU is excluded:
                // it returns Stop and its run loop exits directly, so
                // signaling itself is wasteful (and would be re-entrant).
                for cpu_id in 0..self.tid_slots.len() {
                    if cpu_id as u32 == issuing_cpu_id {
                        continue;
                    }
                    self.signal_cpu(cpu_id as u32);
                }
                HypercallControl::Stop
            }
        }
    }

    /// Decode + apply in one call. Returns `None` when the address is
    /// outside the hypercall range so the caller can fall through to the
    /// device MMIO dispatcher.
    pub fn dispatch_mmio(
        &self,
        addr: u64,
        data: &[u8],
        issuing_cpu_id: u32,
    ) -> Option<HypercallControl> {
        match Self::decode(addr, data, issuing_cpu_id) {
            HypercallAction::NotHypercall => None,
            other => Some(self.apply(other, issuing_cpu_id)),
        }
    }

    fn signal_cpu(&self, cpu_id: u32) {
        if let Some(slot) = self.tid_slots.get(cpu_id as usize) {
            if let Some(pending) = self.wake_pending.get(cpu_id as usize) {
                pending.store(true, Ordering::Release);
            }
            let tid = slot.load(Ordering::Acquire);
            if tid != u64::MAX {
                // SAFETY: pthread_kill with a valid tid and SIGUSR1 is
                // well-defined; the no-op handler is installed once at
                // SumiVm::run startup. Release-Acquire on the tid store
                // ensures we observe a valid pthread_t here.
                unsafe {
                    libc::pthread_kill(tid as libc::pthread_t, libc::SIGUSR1);
                }
            }
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
    /// Arguments for the guest program (argv[1..]; argv[0] is `run_path`).
    /// Passed to the kernel as a NUL-joined tail after the run path in the
    /// boot-info buffer — NUL cannot appear inside a path or argument, so
    /// no escaping is needed and the BootInfo layout is unchanged.
    pub run_args: Vec<String>,
    pub gdb_port: Option<u16>,
    /// `--hostfwd` rules: forward a host TCP port to a guest TCP port
    /// through the network gateway (see `net::gateway`). Empty by default.
    pub hostfwd: Vec<gateway::HostForward>,
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
    ap_entry: u64,
    vcpus: Vec<Backend::VCpuType>,
    gdb_port: Option<u16>,
    kernel_path: PathBuf,
    user_debug: Option<UserDebugInfo>,
    /// The host userspace network gateway (see `net::gateway`). Always
    /// present — the virtio-net device is always registered too, even when
    /// no guest program uses it.
    gateway: Option<GatewayHandle>,
}

/// No-op SIGUSR1 handler. Installed exactly once per process before
/// any vCPU host thread enters `KVM_RUN`, so that
/// `pthread_kill(tid, SIGUSR1)` causes KVM_RUN to return
/// `VcpuExit::Intr` instead of terminating the process.
///
/// Must be a plain `extern "C"` function with an empty body: calling
/// any libc function that is not async-signal-safe from inside a
/// signal handler is UB.
extern "C" fn sumi_sigusr1_handler(_: libc::c_int) {}

fn install_sigusr1_handler_once() -> Result<()> {
    use std::sync::OnceLock;
    static INSTALLED: OnceLock<()> = OnceLock::new();
    let mut res: Result<()> = Ok(());
    INSTALLED.get_or_init(|| {
        // SAFETY: sigaction with a plain function pointer and a
        // zeroed sigset is always well-defined. The handler itself
        // is signal-safe (empty body). We take no action if
        // sigaction fails — the signal will remain at its default
        // action of terminating the process, which the run() path
        // will notice when it tries to join the joined-on thread.
        unsafe {
            let mut sa: libc::sigaction = core::mem::zeroed();
            sa.sa_sigaction = sumi_sigusr1_handler as *const () as usize;
            // SA_RESTART is deliberately NOT set: we WANT KVM_RUN's
            // underlying ioctl to return EINTR so we can inspect the
            // shutdown flag between calls.
            libc::sigemptyset(&mut sa.sa_mask);
            if libc::sigaction(libc::SIGUSR1, &sa, core::ptr::null_mut()) != 0 {
                res = Err(Error::Io(std::io::Error::last_os_error()));
            }
        }
    });
    res
}

impl<Backend: VirtBackend + 'static> SumiVm<Backend> {
    pub fn new(info: &VmCreateInfo) -> Result<Self> {
        let backend = Backend::new(info)?;

        let mem = Arc::new(GuestMemoryMmap::from_ranges(&[(
            GuestAddress(0),
            info.mem_size + KERNEL_CODE_SIZE,
        )])?);

        backend.initialize_memory(&mem)?;

        let dax_host_ptr = backend.dax_host_ptr();
        let net_chan = GatewayChannel::new();
        let devices = Arc::new(Mutex::new(DeviceRegistry::new(
            info.share_dir.as_deref(),
            dax_host_ptr,
            Arc::clone(&net_chan),
        )));
        let gateway = Some(gateway::spawn(net_chan, info.hostfwd.clone()));

        let mut vcpus = Vec::new();
        for _ in 0..info.vcpu_count {
            vcpus.push(backend.create_vcpu(Arc::clone(&devices), Arc::clone(&mem))?);
        }

        let kernel_entry = Self::load_elf(&mem, &info.kernel_path)?;

        // Look up ap_start_asm once at construction so a missing symbol
        // surfaces before any KVM_RUN.
        let kernel_elf_data = std::fs::read(&info.kernel_path)?;
        let ap_entry = Self::find_symbol(&kernel_elf_data, "ap_start_asm")?;

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
            ap_entry,
            gdb_port: info.gdb_port,
            kernel_path: info.kernel_path.clone(),
            user_debug,
            gateway,
        })
    }

    pub fn run(mut self) -> Result<i32> {
        // Install the SIGUSR1 no-op handler unconditionally so it is
        // present in both the normal run path and the debug path (which
        // may call self.run() after detach).
        install_sigusr1_handler_once()?;

        let gateway = self.gateway.take();

        let result = if let Some(port) = self.gdb_port {
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

            // Debug mode always returns exit code 0.
            Ok(0)
        } else {
            // Normal (non-debug) mode: BSP on vCPU 0, APs on the rest.
            let kernel_entry = self.kernel_entry;
            let ap_entry = self.ap_entry;
            let vcpu_count = self.vcpus.len();

            // Snapshot every vCPU's tid slot so the hypercall context can
            // fan out signals. The order matches cpu_id (== index in vcpus).
            let tid_slots: Vec<Arc<AtomicU64>> =
                self.vcpus.iter().map(|c| c.host_tid_slot()).collect();
            let hc_ctx = Arc::new(HypercallContext::new(tid_slots));
            let shutdown = Arc::clone(&hc_ctx.shutdown);

            // Counter incremented by EVERY vCPU (BSP included) after it
            // stores its tid. The supervisor spins until all slots are
            // published before it sends any signal, so it never calls
            // pthread_kill with a stale u64::MAX tid.
            let tid_published_count = Arc::new(AtomicUsize::new(0));

            let mut handles: Vec<thread::JoinHandle<Result<()>>> =
                Vec::with_capacity(vcpu_count);

            for (cpu_id, mut cpu) in self.vcpus.into_iter().enumerate() {
                let tid_slot = cpu.host_tid_slot();
                let hc_ctx = Arc::clone(&hc_ctx);
                let tid_published_count = Arc::clone(&tid_published_count);
                let cpu_id = cpu_id as u32;
                handles.push(thread::spawn(move || -> Result<()> {
                    // Every vCPU (BSP included) publishes its tid so the
                    // hypercall dispatcher can fan out SIGUSR1 to any peer
                    // when any CPU issues HC_SHUTDOWN.
                    // SAFETY: pthread_self never fails.
                    let tid = unsafe { libc::pthread_self() } as u64;
                    tid_slot.store(tid, Ordering::Release);
                    // Release pairs with the Acquire spin in the supervisor.
                    tid_published_count.fetch_add(1, Ordering::Release);

                    if cpu_id == 0 {
                        cpu.init(kernel_entry)?;
                    } else {
                        cpu.init_ap(ap_entry, cpu_id)?;
                    }
                    cpu.run(cpu_id, &hc_ctx)
                }));
            }

            // Wait until every vCPU has stored its tid before joining anyone:
            // every slot is valid before we signal.
            while tid_published_count.load(Ordering::Acquire) < vcpu_count {
                std::hint::spin_loop();
            }

            // Wait for the BSP to finish. Its run loop exits either from
            // VcpuExit::Hlt (legacy halt fallback) or from HypercallControl::Stop
            // (HC_SHUTDOWN path).
            let bsp_handle = handles.remove(0);
            let bsp_result = bsp_handle.join();

            // If the BSP halted without HC_SHUTDOWN, publish shutdown here.
            // If HC_SHUTDOWN already did it, this store is a no-op.
            shutdown.store(true, Ordering::Release);

            // Kick every AP out of KVM_RUN. HC_SHUTDOWN already did this
            // from inside the issuing CPU's dispatcher, but we re-issue
            // here for the BSP-hlt fallback path.
            for slot in hc_ctx.tid_slots.iter().skip(1) {
                let tid = slot.load(Ordering::Acquire);
                if tid != u64::MAX {
                    // SAFETY: pthread_kill with a valid tid and SIGUSR1 is
                    // well-defined; the handler is already installed.
                    unsafe { libc::pthread_kill(tid as libc::pthread_t, libc::SIGUSR1) };
                }
            }

            // Surface any errors.
            let mut first_err: Option<Error> = None;
            match bsp_result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => first_err = Some(e),
                Err(_) => {
                    eprintln!("[vm] BSP vCPU thread panicked");
                    first_err = Some(Error::VcpuThreadPanicked);
                }
            }
            for h in handles {
                match h.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) if first_err.is_none() => first_err = Some(e),
                    Ok(Err(_)) => {}
                    Err(_) => {
                        eprintln!("[vm] AP vCPU thread panicked");
                        if first_err.is_none() {
                            first_err = Some(Error::VcpuThreadPanicked);
                        }
                    }
                }
            }
            match first_err {
                Some(e) => Err(e),
                None => {
                    // Return the guest exit code if HC_SHUTDOWN was called,
                    // else 0 (BSP halted without a hypercall).
                    let code = if hc_ctx.shutdown.load(Ordering::Acquire) {
                        hc_ctx.exit_code.load(Ordering::Acquire)
                    } else {
                        0
                    };
                    Ok(code)
                }
            }
        };

        if let Some(g) = gateway {
            g.shutdown();
        }

        result
    }

    /// Look up the virtual address of a symbol in the kernel ELF's
    /// symbol tables. Returns an error if the symbol is missing — this
    /// is a hard kernel-build bug, not a runtime edge case, so it must
    /// never reach production silently.
    fn find_symbol(elf_data: &[u8], name: &str) -> Result<u64> {
        let elf = Elf::parse(elf_data)?;
        for sym in elf.syms.iter().chain(elf.dynsyms.iter()) {
            if sym.st_value == 0 {
                continue;
            }
            let sym_name = elf
                .strtab
                .get_at(sym.st_name)
                .or_else(|| elf.dynstrtab.get_at(sym.st_name));
            if sym_name == Some(name) {
                return Ok(sym.st_value);
            }
        }
        Err(Error::Parsing(goblin::error::Error::Malformed(format!(
            "kernel ELF does not export symbol `{}` (LTO may have dropped it — \
             mark the definition `#[unsafe(no_mangle)]`)",
            name
        ))))
    }

    /// Parse user ELF to find .text load address and interpreter info.
    fn resolve_user_debug(share_dir: &Path, host_path: &Path) -> Result<UserDebugInfo> {
        let data = std::fs::read(host_path)?;
        let elf = Elf::parse(&data)?;

        let base: u64 = match elf.header.e_type {
            goblin::elf::header::ET_EXEC => 0,
            goblin::elf::header::ET_DYN => PIE_LOAD_BASE,
            _ => 0,
        };

        let binary = SymbolFile {
            host_path: host_path.to_path_buf(),
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
            if sym.st_type() == STT_FUNC
                && sym.st_value != 0
                && let Some(name) = elf
                    .strtab
                    .get_at(sym.st_name)
                    .or_else(|| elf.dynstrtab.get_at(sym.st_name))
                && !name.is_empty()
            {
                let addr = base + sym.st_value;
                // Use st_size if known, otherwise default to 1
                let size = if sym.st_size > 0 { sym.st_size } else { 1 };
                writeln!(f, "{:x} {:x} {}", addr, size, name).map_err(Error::Io)?;
            }
        }
        Ok(())
    }

    /// Write /tmp/perf-<pid>.map with symbols from kernel + user binaries.
    fn write_perf_map(
        kernel_path: &Path,
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
                    goblin::elf::header::ET_DYN => PIE_LOAD_BASE,
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
    fn launch_gdb(kernel_path: &Path, user_debug: Option<&UserDebugInfo>, port: u16) {
        let kernel_path_str = kernel_path.display().to_string();

        let mut args: Vec<String> = vec![
            "-q".into(),
            "-ex".into(),
            "set confirm off".into(),
            "-ex".into(),
            format!("file {}", kernel_path_str),
            "-ex".into(),
            format!("target remote :{}", port),
        ];

        // If there's a user binary, set up automatic symbol loading
        if let Some(info) = user_debug {
            // Break at jump_to_user_asm — at this point the kernel has loaded
            // and mapped the user ELF, so we can safely add symbols.
            args.extend([
                "-ex".into(),
                "break jump_to_user_asm".into(),
                "-ex".into(),
                "continue".into(),
                "-ex".into(),
                "delete breakpoints".into(),
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
        use std::time::{SystemTime, UNIX_EPOCH};
        use sumi_abi::arch::layout::{BOOT_INFO_ADDR, BOOT_INFO_MAX_SIZE};
        use sumi_abi::boot_info::*;

        let mut flags = 0u32;
        // Guest command line: run path, then each argument, NUL-joined. The
        // kernel splits on NUL to build argv (see exec::exec_user_program).
        let mut cmdline = String::new();

        if let Some(ref path) = info.run_path {
            flags |= BOOT_INFO_FLAG_HAS_RUN_PATH;
            cmdline.push_str(path);
            for arg in &info.run_args {
                cmdline.push('\0');
                cmdline.push_str(arg);
            }
        }
        let path_bytes: &[u8] = cmdline.as_bytes();

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
        let ret = unsafe { libc::getrandom(rng_seed.as_mut_ptr() as *mut libc::c_void, 32, 0) };
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
            num_cpus: info.vcpu_count as u32,
            _pad1: 0,
        };

        // SAFETY: BootInfo is repr(C) with no padding holes that matter.
        let struct_bytes: &[u8] = unsafe {
            core::slice::from_raw_parts(&boot_info as *const _ as *const u8, header_size)
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
            mem.write_slice(&data[file_offset..file_end], GuestAddress(ph.p_paddr))?;

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
    /// Initialise the BSP: sregs for long mode, RIP = kernel entry,
    /// RSP = top of KERNEL_STACK, RDI = undefined (BSP takes no arg).
    fn init(&mut self, entry_point: u64) -> Result<()>;

    /// Initialise an application processor: sregs identical to BSP,
    /// RIP = `ap_entry`, RSP = undefined (the asm stub overwrites it
    /// from `AP_BOOT_STACKS[cpu_id]`), RDI = `cpu_id`.
    fn init_ap(&mut self, ap_entry: u64, cpu_id: u32) -> Result<()>;

    fn run(&mut self, cpu_id: u32, hc_ctx: &HypercallContext) -> Result<()>;
    fn run_debug(&mut self, dbg: VCpuDebugReceiver) -> Result<()>;
    fn tsc_khz(&self) -> u32;

    /// Return the host pthread id of this vCPU's worker thread once
    /// it has been spawned. Used by the supervising thread to
    /// `pthread_kill(SIGUSR1)` the vCPU out of KVM_RUN during
    /// shutdown. Release on store / Acquire on load — required because
    /// the supervisor reads the tid after observing the BSP join, and
    /// only then signals the AP. Without the pairing the supervisor's
    /// load could observe a stale `u64::MAX` even after the worker stored.
    fn host_tid_slot(&self) -> Arc<AtomicU64>;
}

#[cfg(test)]
mod hypercall_tests {
    use super::*;
    use sumi_abi::arch::layout::HYPERCALL_MMIO_BASE;
    use sumi_abi::hypercall::{HC_KICK_CPU, HC_SHUTDOWN, HYPERCALL_MMIO_SIZE};

    fn base() -> u64 {
        HYPERCALL_MMIO_BASE.as_u64()
    }

    #[test]
    fn decode_returns_not_hypercall_for_address_below_base() {
        let action = HypercallContext::decode(base() - 0x1000, &[0u8; 8], 0);
        assert_eq!(action, HypercallAction::NotHypercall);
    }

    #[test]
    fn decode_returns_not_hypercall_for_address_above_range() {
        let addr = base() + HYPERCALL_MMIO_SIZE as u64;
        let action = HypercallContext::decode(addr, &[0u8; 8], 0);
        assert_eq!(action, HypercallAction::NotHypercall);
    }

    #[test]
    fn decode_returns_malformed_for_short_write() {
        let addr = base() + HC_SHUTDOWN as u64;
        let action = HypercallContext::decode(addr, &[0u8; 4], 0);
        assert_eq!(action, HypercallAction::Malformed);
    }

    #[test]
    fn decode_returns_malformed_for_long_write() {
        let addr = base() + HC_SHUTDOWN as u64;
        let action = HypercallContext::decode(addr, &[0u8; 16], 0);
        assert_eq!(action, HypercallAction::Malformed);
    }

    #[test]
    fn decode_returns_malformed_for_unknown_offset() {
        // Offset 0x18 is reserved but not yet used.
        let action = HypercallContext::decode(base() + 0x18, &0u64.to_le_bytes(), 0);
        assert_eq!(action, HypercallAction::Malformed);
    }

    #[test]
    fn decode_hc_kick_cpu_returns_signal_one() {
        let arg = 3u64.to_le_bytes();
        let action = HypercallContext::decode(base() + HC_KICK_CPU as u64, &arg, 0);
        assert_eq!(action, HypercallAction::SignalOne(3));
    }

    #[test]
    fn decode_hc_kick_cpu_self_kick_is_dropped() {
        let arg = 2u64.to_le_bytes();
        let action = HypercallContext::decode(base() + HC_KICK_CPU as u64, &arg, 2);
        assert_eq!(action, HypercallAction::SignalMask(0));
    }

    #[test]
    fn decode_hc_shutdown_carries_exit_code_signed() {
        // Negative exit codes round-trip via the u32-as-i32 bit reinterpretation
        // that the kernel-side `shutdown(-1)` uses.
        let arg = ((-1i32) as u32 as u64).to_le_bytes();
        let action = HypercallContext::decode(base() + HC_SHUTDOWN as u64, &arg, 0);
        assert_eq!(action, HypercallAction::Shutdown { exit_code: -1 });
    }

    #[test]
    fn decode_hc_shutdown_zero_exit_code() {
        let arg = 0u64.to_le_bytes();
        let action = HypercallContext::decode(base() + HC_SHUTDOWN as u64, &arg, 0);
        assert_eq!(action, HypercallAction::Shutdown { exit_code: 0 });
    }

    #[test]
    fn apply_shutdown_sets_flag_and_exit_code() {
        let ctx = HypercallContext::new_for_test(2);
        let ctl = ctx.apply(HypercallAction::Shutdown { exit_code: 42 }, 0);
        assert_eq!(ctl, HypercallControl::Stop);
        assert!(ctx.shutdown.load(Ordering::Acquire));
        assert_eq!(ctx.exit_code.load(Ordering::Acquire), 42);
    }

    #[test]
    fn apply_signal_one_returns_continue() {
        let ctx = HypercallContext::new_for_test(4);
        let ctl = ctx.apply(HypercallAction::SignalOne(3), 0);
        assert_eq!(ctl, HypercallControl::Continue);
        // Slot was sentinel u64::MAX, so signal_cpu took the
        // not-published branch. Verifying it did NOT panic.
    }

    #[test]
    fn dispatch_mmio_outside_range_returns_none() {
        let ctx = HypercallContext::new_for_test(1);
        let res = ctx.dispatch_mmio(0xDEAD_BEEF, &0u64.to_le_bytes(), 0);
        assert!(res.is_none());
    }

    #[test]
    fn dispatch_mmio_shutdown_returns_stop() {
        let ctx = HypercallContext::new_for_test(1);
        let arg = 7u64.to_le_bytes();
        let res = ctx.dispatch_mmio(base() + HC_SHUTDOWN as u64, &arg, 0);
        assert_eq!(res, Some(HypercallControl::Stop));
        assert_eq!(ctx.exit_code.load(Ordering::Acquire), 7);
    }

    #[test]
    fn new_for_test_allocates_n_slots_at_sentinel() {
        let ctx = HypercallContext::new_for_test(8);
        assert_eq!(ctx.tid_slots.len(), 8);
        for s in ctx.tid_slots.iter() {
            assert_eq!(s.load(Ordering::Acquire), u64::MAX);
        }
    }
}
