# Kernel Debugging (GDB) & Profiling (perf) -- Design Document

## 1. Goal

Debug sumi kernel and user programs running inside the KVM hypervisor using
standard tools: **GDB** for interactive debugging, **perf** for profiling.

A developer should be able to:
- Attach GDB to a running sumi-vm, set breakpoints in kernel or user code,
  inspect registers and memory, single-step through instructions.
- Profile a workload with `perf record` and get meaningful flame graphs with
  resolved symbols for both kernel and user code.

### Non-goals

- Multi-vCPU debugging (sumi currently runs 1 vCPU; extend later).
- DWARF-level source debugging in the kernel (line-level works if built with
  `-g`, but we don't parse DWARF ourselves -- GDB does).
- In-kernel GDB stub (the stub runs in sumi-vm, the hypervisor).
- Hardware performance counter virtualization inside the guest.
- Live profiling dashboards or continuous profiling infrastructure.

---

## 2. Background

### 2.1 GDB Remote Serial Protocol (RSP)

GDB supports remote debugging via a simple text protocol over TCP or serial.
The **stub** (our hypervisor) speaks RSP, and GDB connects as a client.

Key RSP commands:

| Command | Meaning |
|---------|---------|
| `?` | Stop reason |
| `g` / `G` | Read / write all registers |
| `p N` / `P N=V` | Read / write single register |
| `m addr,len` / `M addr,len:data` | Read / write memory |
| `s` / `c` | Single-step / continue |
| `Z0,addr,len` / `z0,addr,len` | Insert / remove software breakpoint |
| `Z1,addr,len` / `z1,addr,len` | Insert / remove hardware breakpoint |
| `Hg N` | Select thread N |
| `qC` | Current thread |
| `qSupported` | Feature negotiation |
| `qXfer:features:read` | Target description XML |
| `D` | Detach |
| `k` | Kill |

GDB expects an **x86-64 register file** in a specific order (see Section 5.2).

### 2.2 KVM Guest Debugging

KVM provides `KVM_SET_GUEST_DEBUG` ioctl on the vCPU fd:

```c
struct kvm_guest_debug {
    __u32 control;              // KVM_GUESTDBG_ENABLE | flags
    __u32 pad;
    struct kvm_debug_arch arch; // x86: 4 hardware breakpoints (DR0-DR3)
};
```

Control flags:
- `KVM_GUESTDBG_ENABLE` — enable guest debugging.
- `KVM_GUESTDBG_SINGLESTEP` — trap after every instruction.
- `KVM_GUESTDBG_USE_HW_BP` — use hardware breakpoints (DR0-DR3, DR7).
- `KVM_GUESTDBG_USE_SW_BP` — intercept `int3` (0xCC) instructions.

When a debug event occurs, `KVM_RUN` returns `KVM_EXIT_DEBUG` with:
```c
struct kvm_debug_exit_arch {
    __u32 exception;  // 1 = #DB (hw bp / single-step), 3 = #BP (int3)
    __u32 pad;
    __u64 pc;         // RIP at the trap
    __u64 dr6;        // Debug status (which BP hit, single-step, etc.)
    __u64 dr7;        // Debug control
};
```

This is the foundation: sumi-vm sets `KVM_SET_GUEST_DEBUG`, the vCPU run loop
catches `KVM_EXIT_DEBUG`, and the GDB stub handles the rest.

### 2.3 perf and KVM

The host `perf` tool can profile KVM guests in two ways:

1. **Host-side sampling** (`perf record -p <sumi-vm-pid>`): perf samples the
   host process. When the CPU is in guest mode, the sample captures the guest
   RIP. With `perf kvm --guest` and a guest symbol file, perf resolves guest
   addresses to function names.

2. **perf_event_open with KVM_RUN context**: The host kernel tracks guest vs
   host context. `perf record -e cycles:G` captures only guest-mode samples.

Both approaches need a **symbol file** mapping guest virtual addresses to
function names. This is the kernel ELF plus any loaded user program ELFs.

---

## 3. Architecture Overview

```
┌─────────────────────────────────────────────────────────┐
│  GDB client                                             │
│  (gdb) target remote :1234                              │
└────────────┬────────────────────────────────────────────┘
             │ TCP
┌────────────▼────────────────────────────────────────────┐
│  sumi-vm                                                │
│                                                         │
│  ┌──────────────┐    ┌───────────────────────────────┐  │
│  │  GDB stub    │◄──►│  KvmVCpu                      │  │
│  │  (RSP server)│    │                               │  │
│  │              │    │  KVM_SET_GUEST_DEBUG           │  │
│  │  - breakpts  │    │  get_regs / set_regs           │  │
│  │  - memory    │    │  guest memory read/write       │  │
│  │  - registers │    │  KVM_EXIT_DEBUG handling       │  │
│  └──────────────┘    └───────────────────────────────┘  │
│                                                         │
│  ┌──────────────┐                                       │
│  │  Symbol table │  Loaded from kernel ELF + user ELF   │
│  │  (for perf)  │  Exported as /tmp/perf-<pid>.map      │
│  └──────────────┘                                       │
└─────────────────────────────────────────────────────────┘
             │ KVM ioctl
┌────────────▼────────────────────────────────────────────┐
│  KVM guest (sumi-kernel)                                │
│  - Kernel code at KERNEL_CODE_VIRT                      │
│  - User code at 0x400000+ (PIE) or fixed addresses      │
│  - int3 instructions at breakpoints                     │
│  - Single-step via RFLAGS.TF                            │
└─────────────────────────────────────────────────────────┘
```

The GDB stub runs in a dedicated thread in sumi-vm. It communicates with the
vCPU run loop via a shared command channel. When GDB sends a command, the stub
signals the vCPU to pause, executes the operation, and returns the result.

---

## 4. GDB Stub Design

### 4.1 Module Structure

```
sumi-vm/src/
  debug/
    mod.rs          -- GdbServer, public API
    rsp.rs          -- RSP packet parsing / serialization
    commands.rs     -- GDB command handlers (g, m, s, Z, etc.)
    registers.rs    -- x86-64 register file layout for GDB
    breakpoints.rs  -- Software & hardware breakpoint management
```

### 4.2 Core Types

```rust
/// GDB debug server. Owns the TCP listener and communicates
/// with the vCPU via a command channel.
pub struct GdbServer {
    listener: TcpListener,
    breakpoints: BreakpointManager,
    vcpu_ctl: VCpuDebugControl,
}

/// Channel for GDB stub <-> vCPU communication.
pub struct VCpuDebugControl {
    /// GDB sends commands, vCPU thread processes them.
    cmd_tx: Sender<DebugCommand>,
    /// vCPU sends responses / stop events.
    event_rx: Receiver<DebugEvent>,
}

pub enum DebugCommand {
    /// Pause the vCPU (if running).
    Pause,
    /// Resume execution.
    Continue,
    /// Execute one instruction.
    SingleStep,
    /// Read all general-purpose registers.
    ReadRegisters,
    /// Write all general-purpose registers.
    WriteRegisters(GdbRegisterFile),
    /// Read guest memory (virtual address).
    ReadMemory { addr: u64, len: usize },
    /// Write guest memory (virtual address).
    WriteMemory { addr: u64, data: Vec<u8> },
    /// Insert breakpoint.
    InsertBreakpoint(Breakpoint),
    /// Remove breakpoint.
    RemoveBreakpoint(Breakpoint),
    /// Detach debugger.
    Detach,
    /// Kill the VM.
    Kill,
}

pub enum DebugEvent {
    /// vCPU hit a breakpoint or completed single-step.
    Stopped(StopReason),
    /// Response to a register/memory read.
    Registers(GdbRegisterFile),
    Memory(Vec<u8>),
    /// Command completed (ack).
    Ok,
    /// Error.
    Error(String),
}

pub enum StopReason {
    Breakpoint { addr: u64 },
    SingleStep { addr: u64 },
    Signal(u8),    // Unix signal number for GDB
    Exited(u8),
}
```

### 4.3 vCPU Run Loop Integration

The vCPU run loop must be modified to:

1. Check for pending debug commands between VM exits.
2. Handle `KVM_EXIT_DEBUG` alongside existing exits.
3. Apply debug state (breakpoints, single-step) via `KVM_SET_GUEST_DEBUG`.

```rust
// sumi-vm/src/arch/x86_64/kvm/mod.rs

impl KvmVCpu {
    fn run_with_debug(&mut self, dbg: &VCpuDebugReceiver) -> Result<()> {
        loop {
            // Check for pending debug commands (non-blocking).
            while let Ok(cmd) = dbg.cmd_rx.try_recv() {
                self.handle_debug_command(cmd, &dbg.event_tx)?;
            }

            // If paused by debugger, block until Continue/SingleStep.
            if self.debug_state.paused {
                let cmd = dbg.cmd_rx.recv()?;  // blocking
                self.handle_debug_command(cmd, &dbg.event_tx)?;
                if self.debug_state.paused {
                    continue;
                }
            }

            match self.fd.run()? {
                // Existing exits...
                VcpuExit::IoOut(0xE9, data) => { /* debugcon */ }
                VcpuExit::MmioRead(addr, data) => { /* device */ }
                VcpuExit::MmioWrite(addr, data) => { /* device */ }
                VcpuExit::Hlt => {
                    dbg.event_tx.send(DebugEvent::Stopped(
                        StopReason::Exited(0)
                    ))?;
                    return Ok(());
                }

                // NEW: debug exit
                VcpuExit::Debug(debug_exit) => {
                    self.debug_state.paused = true;
                    let reason = match debug_exit.exception {
                        3 => StopReason::Breakpoint { addr: debug_exit.pc },
                        1 => StopReason::SingleStep { addr: debug_exit.pc },
                        _ => StopReason::Signal(5),  // SIGTRAP
                    };
                    dbg.event_tx.send(DebugEvent::Stopped(reason))?;
                }

                VcpuExit::Shutdown => {
                    dbg.event_tx.send(DebugEvent::Stopped(
                        StopReason::Signal(11)  // SIGSEGV
                    ))?;
                    self.debug_state.paused = true;
                }

                other => return Err(Error::UnexpectedExit(format!("{:?}", other))),
            }
        }
    }
}
```

### 4.4 Pausing a Running vCPU

When GDB sends Ctrl+C (interrupt), we need to pause a vCPU that is currently
inside `KVM_RUN`. Two approaches:

**Option A: Signal-based (recommended).** Send a signal (e.g. `SIGUSR1`) to the
vCPU thread. KVM exits with `KVM_EXIT_INTR` when a signal is delivered during
`KVM_RUN`. The vCPU thread checks for a pending pause flag.

```rust
// In the GDB stub thread:
fn pause_vcpu(&self) {
    self.pause_requested.store(true, Ordering::SeqCst);
    // Send SIGUSR1 to the vCPU thread.
    unsafe { libc::pthread_kill(self.vcpu_thread_id, libc::SIGUSR1) };
}

// In the vCPU run loop, after KVM_EXIT_INTR:
VcpuExit::Intr => {
    if self.debug_state.pause_requested.load(Ordering::SeqCst) {
        self.debug_state.pause_requested.store(false, Ordering::SeqCst);
        self.debug_state.paused = true;
        dbg.event_tx.send(DebugEvent::Stopped(StopReason::Signal(2)))?;
        // SIGINT
    }
}
```

**Option B: `KVM_SET_GUEST_DEBUG` with single-step.** Set single-step flag from
the GDB stub thread. Next instruction causes `KVM_EXIT_DEBUG`. Simpler but
wastes one guest instruction.

**Recommendation:** Option A. It's the standard approach used by QEMU, Firecracker,
and other VMMs. No guest instruction is wasted.

### 4.5 Software Breakpoints

Software breakpoints patch guest memory with `int3` (0xCC):

```rust
pub struct SoftBreakpoint {
    addr: u64,                // Guest virtual address
    original_byte: u8,        // Saved byte at addr
}

impl BreakpointManager {
    fn insert_sw_breakpoint(&mut self, addr: u64, mem: &GuestMemoryMmap<()>)
        -> Result<()>
    {
        // Translate guest virtual -> guest physical -> host pointer.
        let paddr = self.translate_guest_vaddr(addr)?;
        let host_ptr = mem.get_host_address(GuestAddress(paddr))
            .map_err(|_| Error::BadAddress(addr))?;

        // Save original byte.
        let original = unsafe { *host_ptr };
        self.breakpoints.insert(addr, SoftBreakpoint { addr, original_byte: original });

        // Write int3.
        unsafe { *host_ptr = 0xCC };
        Ok(())
    }

    fn remove_sw_breakpoint(&mut self, addr: u64, mem: &GuestMemoryMmap<()>)
        -> Result<()>
    {
        let bp = self.breakpoints.remove(&addr)
            .ok_or(Error::BreakpointNotFound(addr))?;
        let paddr = self.translate_guest_vaddr(addr)?;
        let host_ptr = mem.get_host_address(GuestAddress(paddr))?;
        unsafe { *host_ptr = bp.original_byte };
        Ok(())
    }
}
```

When a software breakpoint hits, RIP points to the `int3` byte. To resume:
1. Restore the original byte.
2. Single-step one instruction (to execute the original).
3. Re-insert the `int3`.
4. Continue.

### 4.6 Hardware Breakpoints

x86-64 has 4 hardware debug registers (DR0-DR3) controlled by DR7. KVM exposes
them via `kvm_guest_debug.arch`:

```rust
fn set_hw_breakpoints(&self, breakpoints: &[HwBreakpoint]) -> Result<()> {
    assert!(breakpoints.len() <= 4);

    let mut debug = kvm_guest_debug {
        control: KVM_GUESTDBG_ENABLE | KVM_GUESTDBG_USE_HW_BP,
        ..Default::default()
    };

    let mut dr7: u64 = 0;
    for (i, bp) in breakpoints.iter().enumerate() {
        debug.arch.debugreg[i] = bp.addr;

        // Enable local breakpoint (bits 0,2,4,6 for DR0-3)
        dr7 |= 1 << (i * 2);

        // Set condition (bits 16-17, 20-21, 24-25, 28-29):
        // 00 = execute, 01 = write, 11 = read/write
        let condition = match bp.kind {
            BpKind::Execute => 0b00,
            BpKind::Write   => 0b01,
            BpKind::Access  => 0b11,
        };
        dr7 |= condition << (16 + i * 4);

        // Set length (bits 18-19, 22-23, 26-27, 30-31):
        // 00 = 1 byte, 01 = 2 bytes, 11 = 4 bytes, 10 = 8 bytes
        let len = match bp.len {
            1 => 0b00,
            2 => 0b01,
            4 => 0b11,
            8 => 0b10,
            _ => 0b00,
        };
        dr7 |= len << (18 + i * 4);
    }

    debug.arch.debugreg[7] = dr7;
    self.fd.set_guest_debug(&debug)?;
    Ok(())
}
```

Hardware breakpoints are preferred for:
- Execute breakpoints on read-only code (no memory patching).
- Data watchpoints (break on memory write/access) -- GDB `watch` command.
- Breakpoints in memory-mapped I/O regions.

### 4.7 Guest Virtual Address Translation

The GDB stub must translate guest virtual addresses to guest physical addresses
for memory reads/writes. Guest page tables are in guest physical memory, which
is directly accessible from the host.

```rust
/// Walk guest page tables to translate a guest virtual address
/// to a guest physical address. Reads page tables from guest memory.
fn translate_guest_vaddr(&self, vaddr: u64) -> Result<u64> {
    let cr3 = self.fd.get_sregs()?.cr3;
    let mem = &self.mem;

    // PML4
    let pml4_idx = (vaddr >> 39) & 0x1FF;
    let pml4e = read_u64(mem, (cr3 & !0xFFF) + pml4_idx * 8)?;
    if pml4e & 1 == 0 { return Err(Error::PageNotPresent(vaddr)); }

    // PDPT
    let pdpt_idx = (vaddr >> 30) & 0x1FF;
    let pdpte = read_u64(mem, (pml4e & ADDR_MASK) + pdpt_idx * 8)?;
    if pdpte & 1 == 0 { return Err(Error::PageNotPresent(vaddr)); }
    // 1GB huge page?
    if pdpte & PTE_PS != 0 {
        return Ok((pdpte & ADDR_MASK_1G) | (vaddr & 0x3FFF_FFFF));
    }

    // PD
    let pd_idx = (vaddr >> 21) & 0x1FF;
    let pde = read_u64(mem, (pdpte & ADDR_MASK) + pd_idx * 8)?;
    if pde & 1 == 0 { return Err(Error::PageNotPresent(vaddr)); }
    // 2MB huge page? (sumi uses 2MB pages)
    if pde & PTE_PS != 0 {
        return Ok((pde & ADDR_MASK_2M) | (vaddr & 0x1F_FFFF));
    }

    // PT (4KB -- not used by sumi currently, but handle for completeness)
    let pt_idx = (vaddr >> 12) & 0x1FF;
    let pte = read_u64(mem, (pde & ADDR_MASK) + pt_idx * 8)?;
    if pte & 1 == 0 { return Err(Error::PageNotPresent(vaddr)); }
    Ok((pte & ADDR_MASK_4K) | (vaddr & 0xFFF))
}

fn read_u64(mem: &GuestMemoryMmap<()>, paddr: u64) -> Result<u64> {
    let mut buf = [0u8; 8];
    mem.read_slice(&mut buf, GuestAddress(paddr))?;
    Ok(u64::from_le_bytes(buf))
}
```

### 4.8 Register File Layout

GDB's x86-64 target expects registers in a specific order. Total: 57 registers
(see `gdb/features/i386/64bit-core.xml`):

```rust
/// x86-64 register file as expected by GDB RSP 'g'/'G' commands.
/// Each register is 8 bytes (little-endian), except FP/SSE registers.
#[repr(C)]
pub struct GdbRegisterFile {
    // General purpose (16 × 8 bytes)
    pub rax: u64, pub rbx: u64, pub rcx: u64, pub rdx: u64,
    pub rsi: u64, pub rdi: u64, pub rbp: u64, pub rsp: u64,
    pub r8:  u64, pub r9:  u64, pub r10: u64, pub r11: u64,
    pub r12: u64, pub r13: u64, pub r14: u64, pub r15: u64,

    // Instruction pointer + flags
    pub rip:    u64,
    pub eflags: u32,

    // Segment registers (6 × 4 bytes)
    pub cs: u32, pub ss: u32, pub ds: u32,
    pub es: u32, pub fs: u32, pub gs: u32,

    // x87 FPU (8 × 10 bytes + control registers)
    pub st0: [u8; 10], /* ... st1-st7 ... */
    pub fctrl: u32, pub fstat: u32, pub ftag: u32,
    pub fiseg: u32, pub fioff: u32, pub foseg: u32, pub fooff: u32,
    pub fop:   u32,

    // SSE (16 × 16 bytes + MXCSR)
    pub xmm0: u128, /* ... xmm1-xmm15 ... */
    pub mxcsr: u32,
}

impl GdbRegisterFile {
    /// Build from KVM register structs.
    pub fn from_kvm(regs: &kvm_regs, sregs: &kvm_sregs, fpu: &kvm_fpu) -> Self {
        Self {
            rax: regs.rax, rbx: regs.rbx, rcx: regs.rcx, rdx: regs.rdx,
            rsi: regs.rsi, rdi: regs.rdi, rbp: regs.rbp, rsp: regs.rsp,
            r8: regs.r8, r9: regs.r9, r10: regs.r10, r11: regs.r11,
            r12: regs.r12, r13: regs.r13, r14: regs.r14, r15: regs.r15,
            rip: regs.rip,
            eflags: regs.rflags as u32,
            cs: sregs.cs.selector as u32,
            ss: sregs.ss.selector as u32,
            // ... fill rest from fpu ...
        }
    }
}
```

---

## 5. RSP Protocol Implementation

### 5.1 Packet Format

```
$<data>#<checksum>
```

- `$` = start, `#` = end, checksum = sum of data bytes mod 256 as 2-char hex.
- Response: `+` (ACK) or `-` (NACK, retransmit).
- Binary data uses hex encoding (2 chars per byte).

### 5.2 Packet Parser

```rust
pub struct RspPacket {
    pub data: Vec<u8>,
}

impl RspPacket {
    pub fn parse(stream: &mut TcpStream) -> Result<Self> { /* ... */ }
    pub fn serialize(&self) -> Vec<u8> { /* ... */ }
}

/// Parse an RSP command from packet data.
pub fn parse_command(data: &[u8]) -> Result<GdbCommand> {
    match data[0] {
        b'?' => Ok(GdbCommand::StopReason),
        b'g' => Ok(GdbCommand::ReadRegisters),
        b'G' => Ok(GdbCommand::WriteRegisters(parse_reg_data(&data[1..])?)),
        b'p' => Ok(GdbCommand::ReadRegister(parse_hex(&data[1..])?)),
        b'm' => {
            let (addr, len) = parse_addr_len(&data[1..])?;
            Ok(GdbCommand::ReadMemory { addr, len })
        }
        b'M' => {
            let (addr, len, data) = parse_addr_len_data(&data[1..])?;
            Ok(GdbCommand::WriteMemory { addr, data })
        }
        b's' => Ok(GdbCommand::SingleStep),
        b'c' => Ok(GdbCommand::Continue),
        b'Z' => parse_breakpoint_insert(&data[1..]),
        b'z' => parse_breakpoint_remove(&data[1..]),
        b'H' => Ok(GdbCommand::SetThread(parse_thread(&data[1..])?)),
        b'D' => Ok(GdbCommand::Detach),
        b'k' => Ok(GdbCommand::Kill),
        b'q' => parse_query(&data[1..]),
        b'v' => parse_v_command(&data[1..]),
        _ => Ok(GdbCommand::Unknown),
    }
}
```

### 5.3 Feature Negotiation

```rust
fn handle_qsupported(&self) -> String {
    "PacketSize=4096;swbreak+;hwbreak+;qXfer:features:read+"
}
```

- `swbreak+` — we support software breakpoints.
- `hwbreak+` — we support hardware breakpoints (up to 4).
- `qXfer:features:read+` — we provide target description XML.

### 5.4 Target Description

GDB needs a target description to know the register layout. We provide the
standard `i386:x86-64` target description:

```rust
fn handle_qxfer_features(&self) -> &'static str {
    r#"<?xml version="1.0"?>
    <!DOCTYPE target SYSTEM "gdb-target.dtd">
    <target version="1.0">
      <architecture>i386:x86-64</architecture>
    </target>"#
}
```

This tells GDB to use its built-in x86-64 register layout, avoiding the need
to enumerate all 57 registers in the XML.

---

## 6. CLI Integration

### 6.1 New CLI Flags

```
sumi-vm run [OPTIONS] <kernel>

Options:
    --gdb <port>        Start GDB stub on TCP port (e.g. --gdb 1234)
    --gdb-wait          Wait for GDB to attach before starting the vCPU
    --perf-map          Write /tmp/perf-<pid>.map symbol file for perf
```

### 6.2 Startup Flow

```
sumi-vm run --gdb 1234 --gdb-wait kernel.elf
```

1. Create VM, load kernel ELF, initialize vCPU (as before).
2. Extract symbol table from kernel ELF → `SymbolTable`.
3. If `--perf-map`: write `/tmp/perf-<pid>.map`.
4. If `--gdb`: start GDB stub thread, listening on `0.0.0.0:<port>`.
5. If `--gdb-wait`: block vCPU thread until GDB connects and sends `c`.
6. If not `--gdb-wait`: start vCPU immediately (GDB can attach later).

### 6.3 User Workflow

**Debugging the kernel:**

```bash
# Terminal 1: start VM with GDB stub
cargo run -p sumi-vm -- run --gdb 1234 --gdb-wait \
    target/x86_64-unknown-none/debug/sumi-kernel

# Terminal 2: attach GDB
gdb target/x86_64-unknown-none/debug/sumi-kernel
(gdb) target remote :1234
(gdb) break _start
(gdb) continue
(gdb) info registers
(gdb) x/10i $rip
(gdb) step
```

**Debugging user programs:**

```bash
# Terminal 1: start VM
cargo run -p sumi-vm -- run --gdb 1234 --gdb-wait \
    --share ./rootfs --run /bin/hello \
    target/x86_64-unknown-none/release/sumi-kernel

# Terminal 2: attach GDB, load user symbols
gdb
(gdb) target remote :1234
(gdb) add-symbol-file rootfs/bin/hello 0x400000
(gdb) break main
(gdb) continue
```

---

## 7. Symbol Table for Profiling

### 7.1 perf.map Format

`perf` reads `/tmp/perf-<pid>.map` for JIT/runtime symbol resolution. Format:

```
<hex_start_addr> <hex_size> <symbol_name>
```

Example:
```
ffffffff80001000 150 _start
ffffffff80001150 2a0 kernel_main
ffffffff80002000 80 syscall_entry
ffffffff80002080 340 syscall_dispatch
```

### 7.2 Symbol Extraction

On kernel ELF load, extract `.symtab` / `.dynsym`:

```rust
pub struct SymbolTable {
    symbols: Vec<Symbol>,
}

pub struct Symbol {
    pub name: String,
    pub addr: u64,    // Virtual address in guest
    pub size: u64,
}

impl SymbolTable {
    /// Extract symbols from an ELF binary.
    pub fn from_elf(elf_data: &[u8]) -> Result<Self> {
        let elf = goblin::elf::Elf::parse(elf_data)?;
        let mut symbols = Vec::new();
        for sym in elf.syms.iter() {
            if sym.st_type() == goblin::elf::sym::STT_FUNC && sym.st_size > 0 {
                if let Some(name) = elf.strtab.get_at(sym.st_name) {
                    symbols.push(Symbol {
                        name: name.to_string(),
                        addr: sym.st_value,
                        size: sym.st_size,
                    });
                }
            }
        }
        symbols.sort_by_key(|s| s.addr);
        Ok(Self { symbols })
    }

    /// Write perf map file.
    pub fn write_perf_map(&self, pid: u32) -> Result<()> {
        let path = format!("/tmp/perf-{}.map", pid);
        let mut f = File::create(&path)?;
        for sym in &self.symbols {
            writeln!(f, "{:x} {:x} {}", sym.addr, sym.size, sym.name)?;
        }
        Ok(())
    }
}
```

### 7.3 User Program Symbols

When the kernel loads a user ELF (detected via boot info `run_path`), sumi-vm
can also extract its symbols. Since sumi-vm loads the kernel ELF itself, and
the user ELF is loaded by the kernel at runtime, we have two options:

**Option A: Pre-extract from the shared directory.**
sumi-vm reads the user binary from `--share` dir at startup, extracts symbols
at the user load base (`PIE_LOAD_BASE` for ET_DYN, or absolute for ET_EXEC),
and appends them to the perf map.

**Option B: Kernel notifies hypervisor.**
Add a hypercall (e.g., `out 0xEA`) that the kernel sends after loading a user
ELF, passing the load base and path. sumi-vm extracts symbols on the fly.

**Recommendation:** Option A for Phase 1 (simple, covers most cases). Option B
for Phase 2 (handles dynamically-linked libraries loaded at runtime).

### 7.4 Using perf

```bash
# Start VM with perf map
cargo run -p sumi-vm -- run --perf-map \
    --share ./rootfs --run /bin/workload \
    target/x86_64-unknown-none/release/sumi-kernel &
VM_PID=$!

# Record samples
perf record -g -p $VM_PID -- sleep 10

# Resolve symbols and generate flame graph
perf script | stackcollapse-perf.pl | flamegraph.pl > flame.svg
```

For guest-only samples (exclude sumi-vm host code):

```bash
perf record -e cycles:G -p $VM_PID -- sleep 10
```

---

## 8. Guest Memory Access for Debugging

### 8.1 Problem

GDB sends memory read/write requests with **guest virtual addresses**. The GDB
stub needs to:

1. Walk the guest page tables to translate GVA → GPA (Section 4.7).
2. Use `GuestMemoryMmap` to access the GPA from the host.

### 8.2 Cross-Page Reads

A memory read may span a page boundary. Since sumi uses 2MB pages this is rare,
but the stub must handle it:

```rust
fn read_guest_memory(&self, vaddr: u64, len: usize) -> Result<Vec<u8>> {
    let mut result = Vec::with_capacity(len);
    let mut remaining = len;
    let mut addr = vaddr;

    while remaining > 0 {
        let paddr = self.translate_guest_vaddr(addr)?;
        // Bytes until next page boundary.
        let page_remaining = PAGE_SIZE_2MB - (paddr as usize % PAGE_SIZE_2MB);
        let chunk = remaining.min(page_remaining);

        let mut buf = vec![0u8; chunk];
        self.mem.read_slice(&mut buf, GuestAddress(paddr))?;
        result.extend_from_slice(&buf);

        addr += chunk as u64;
        remaining -= chunk;
    }
    Ok(result)
}
```

### 8.3 Kernel vs User Address Spaces

sumi has a single address space — kernel and user code share the same page
tables. The GDB stub doesn't need to distinguish between them; the page table
walk handles both KERNEL_CODE_VIRT range and user-space addresses transparently.

---

## 9. Thread Model

sumi currently runs a single vCPU. GDB expects at least one thread:

```rust
fn handle_qfthreadinfo(&self) -> String {
    "m1"  // One thread, ID = 1
}

fn handle_qsthreadinfo(&self) -> String {
    "l"   // End of list
}

fn handle_qc(&self) -> String {
    "QC1"  // Current thread = 1
}
```

When multi-vCPU support is added, each vCPU maps to a GDB thread.

---

## 10. Implementation Plan

### Phase 1: GDB Core

Minimal working GDB stub for kernel debugging.

**Step 1: RSP server** (`debug/rsp.rs`)
- [ ] TCP listener, packet parsing, checksum validation
- [ ] ACK/NACK handling
- [ ] Hex encoding/decoding helpers

**Step 2: Register access** (`debug/registers.rs`)
- [ ] `GdbRegisterFile` struct matching GDB x86-64 layout
- [ ] Conversion from `kvm_regs` + `kvm_sregs` + `kvm_fpu`
- [ ] `g` (read all) and `G` (write all) command handlers
- [ ] `p` (read one) and `P` (write one) command handlers

**Step 3: Memory access** (`debug/commands.rs`)
- [ ] Guest page table walker (GVA → GPA)
- [ ] `m` (read memory) and `M` (write memory) handlers
- [ ] Cross-page read support

**Step 4: Execution control** (`debug/commands.rs`)
- [ ] `c` (continue) — resume vCPU
- [ ] `s` (single-step) — `KVM_SET_GUEST_DEBUG` with `KVM_GUESTDBG_SINGLESTEP`
- [ ] Ctrl+C interrupt — `SIGUSR1` to vCPU thread → `KVM_EXIT_INTR`
- [ ] `?` (stop reason) — report why vCPU is stopped

**Step 5: Software breakpoints** (`debug/breakpoints.rs`)
- [ ] `Z0` / `z0` — insert/remove `int3` at guest virtual address
- [ ] `KVM_SET_GUEST_DEBUG` with `KVM_GUESTDBG_USE_SW_BP`
- [ ] Resume-past-breakpoint logic (restore, single-step, re-insert)

**Step 6: vCPU integration** (`arch/x86_64/kvm/mod.rs`)
- [ ] `VCpuDebugControl` channel between GDB stub and vCPU thread
- [ ] Modified run loop handling `KVM_EXIT_DEBUG`
- [ ] `--gdb` and `--gdb-wait` CLI flags

**Step 7: Feature negotiation** (`debug/commands.rs`)
- [ ] `qSupported` response
- [ ] `qXfer:features:read` — target description XML
- [ ] `qfThreadInfo` / `qsThreadInfo` / `qC` — thread queries
- [ ] `Hg` / `Hc` — thread selection (always thread 1)

### Phase 2: Hardware Breakpoints & Watchpoints

- [ ] `Z1` / `z1` — hardware execution breakpoints (DR0-DR3)
- [ ] `Z2` / `z2` — write watchpoints
- [ ] `Z3` / `z3` — read watchpoints
- [ ] `Z4` / `z4` — access watchpoints
- [ ] `KVM_SET_GUEST_DEBUG` with `KVM_GUESTDBG_USE_HW_BP`

### Phase 3: perf Integration

- [ ] Symbol table extraction from kernel ELF
- [ ] `/tmp/perf-<pid>.map` generation
- [ ] `--perf-map` CLI flag
- [ ] User binary symbol extraction from `--share` directory

### Phase 4: Advanced

- [ ] User program load notification (hypercall 0xEA)
- [ ] Dynamic symbol map updates for `dlopen`'d libraries
- [ ] Multi-vCPU thread support in GDB stub
- [ ] `qXfer:exec-file:read` — report the loaded binary path
- [ ] Conditional breakpoints (server-side evaluation)
- [ ] `perf inject --jit` integration for richer symbol info

---

## 11. KVM API Requirements

### 11.1 New kvm-ioctls Usage

| API | Current | Needed |
|-----|---------|--------|
| `get_regs` / `set_regs` | Shutdown dump only | Every debug stop |
| `get_sregs` / `set_sregs` | Init only | Page table walks (CR3) |
| `get_fpu` / `set_fpu` | Not used | Register reads (FP/SSE) |
| `set_guest_debug` | Not used | **New**: breakpoints, single-step |
| `get_debug_regs` | Not used | **New**: hardware watchpoint state |

### 11.2 kvm-ioctls Crate

The `kvm-ioctls` crate already wraps `KVM_SET_GUEST_DEBUG`. Verify:

```rust
// kvm_ioctls::VcpuFd
fn set_guest_debug(&self, debug: &kvm_guest_debug) -> Result<()>;
```

If not available, use raw ioctl:

```rust
const KVM_SET_GUEST_DEBUG: u64 = 0x4048_ae9a;

unsafe {
    let ret = libc::ioctl(vcpu_fd, KVM_SET_GUEST_DEBUG, &debug as *const _);
    if ret < 0 { return Err(io::Error::last_os_error()); }
}
```

### 11.3 `KVM_EXIT_DEBUG` Handling

The `kvm-ioctls` crate exposes `VcpuExit::Debug` (or similar). If not, we
parse the raw `kvm_run` struct:

```rust
const KVM_EXIT_DEBUG: u32 = 4;

// kvm_run.exit_reason == KVM_EXIT_DEBUG
// kvm_run.debug.arch.exception — exception vector (1 = #DB, 3 = #BP)
// kvm_run.debug.arch.pc — instruction pointer
// kvm_run.debug.arch.dr6 — debug status register
// kvm_run.debug.arch.dr7 — debug control register
```

---

## 12. Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| `kvm-ioctls` doesn't expose `set_guest_debug` | Can't use safe wrapper | Fall back to raw ioctl; submit PR upstream |
| `KVM_EXIT_DEBUG` not in `VcpuExit` enum | Can't match on it | Parse `kvm_run` manually; submit PR upstream |
| Software breakpoints corrupt memory if not cleaned up | Crash on detach | `Detach` handler restores all patched bytes |
| Signal delivery to vCPU thread races with debug commands | Deadlock or missed pause | Use `AtomicBool` flag + `SIGUSR1`; signal handler is no-op (just interrupts `KVM_RUN`) |
| perf samples during host-mode (not guest) | Noisy profiles | Use `perf record -e cycles:G` for guest-only sampling |
| Guest RIP in perf doesn't account for KASLR | Wrong symbols | sumi has no KASLR; addresses are deterministic |
| Single-step over `syscall` instruction is complex | Skip or double-execute | KVM handles it correctly; `KVM_EXIT_DEBUG` fires after `syscall` completes |
| Breakpoint at `int3`-heavy code (e.g., `__debugbreak`) | False positives | Track which `int3` we inserted vs pre-existing; only intercept ours |
| GDB stub thread panic kills the VM | Lost debug session | Catch panics in the stub thread; log error and continue VM without debug |

---

## 13. Open Questions

1. **`vCont` support**: Modern GDB prefers `vCont` over bare `s`/`c` for
   multi-threaded targets. Implement in Phase 1 (single thread, simple) or
   defer to Phase 4 (multi-vCPU)?
   **Recommendation:** Implement basic `vCont;c;s` in Phase 1 — GDB may
   refuse to work without it depending on version.

2. **Kernel-side `int3` support**: If the guest has no IDT, a guest `int3`
   instruction causes a triple fault → `KVM_EXIT_SHUTDOWN`, not
   `KVM_EXIT_DEBUG`. Fix: sumi-kernel must set up an IDT with at least a `#BP`
   handler, OR use `KVM_GUESTDBG_USE_SW_BP` which intercepts `int3` at the
   hypervisor level before the guest IDT is consulted.
   **Answer:** `KVM_GUESTDBG_USE_SW_BP` intercepts `int3` at the KVM level.
   The guest IDT is not involved. No kernel changes needed.

3. **`perf kvm` vs `perf record`**: `perf kvm --guest` requires a guest
   `vmlinux` file and uses its own symbol resolution. `perf record` with
   `perf-<pid>.map` is simpler. Support both or just the map file?
   **Recommendation:** Start with `perf-<pid>.map` (Phase 3). Add `perf kvm`
   support later if needed.

4. **GDB `monitor` commands**: Should the stub support custom monitor commands
   (e.g., `monitor info palloc` to dump page allocator state)?
   **Recommendation:** Defer. Nice to have but not essential for core
   debugging. Can be added incrementally.

5. **Debug build symbols**: The kernel built with `--release` has no debug
   info. Should `--gdb` imply a debug build recommendation?
   **Recommendation:** Document that `--gdb` works best with debug builds
   (`cargo build -p sumi-kernel --target x86_64-unknown-none` without
   `--release`, or with `debug = true` in the release profile).
