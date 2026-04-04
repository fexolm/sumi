# User Program Execution — Design Document

## 1. Background

sumi boots a kernel, initializes virtio-fs, runs selftests, and halts. It cannot yet execute
user programs. This document describes how to load and run a host-side statically-linked Linux
ELF binary inside the guest.

### Goals

- Add `--run <path>` flag to `sumi-vm` to specify a program to execute.
- Pass the program path to the guest kernel at boot via a `BootInfo` structure.
- Guest kernel reads the file via virtio-fs, parses ELF, maps segments into user virtual
  memory, and transfers control to the program's entry point.
- Support statically-linked `ET_EXEC` x86_64 ELF binaries (e.g., musl-static `hello world`).

### Non-goals (for now)

- Dynamic linking / shared libraries / `ld.so`.
- PIE binaries (`ET_DYN`) — phase 2.
- Multiple processes / `fork` / `exec`.
- Signal delivery.
- File-backed `mmap`.
- `argv` / `envp` passed from the host (phase 1: `argc=1, argv=[program_path]`).

---

## 2. Architecture Overview

```
  Host (sumi-vm)                            Guest (sumi-kernel)
 +---------------------------+             +--------------------------------+
 | CLI: --run /path/to/bin   |             |                                |
 |                           |             |  _start()                      |
 | 1. Write BootInfo to      |  VM boot    |    ↓                           |
 |    guest phys memory      +------------>|  2. Read BootInfo              |
 |    at BOOT_INFO_ADDR      |             |    ↓                           |
 |                           |             |  3. Open file via virtio-fs    |
 | share_dir must contain    |             |    ↓                           |
 | the binary                |             |  4. Parse ELF header           |
 +---------------------------+             |    ↓                           |
                                           |  5. Create user page table     |
                                           |    ↓                           |
                                           |  6. Map PT_LOAD segments       |
                                           |    ↓                           |
                                           |  7. Allocate + prepare stack   |
                                           |    ↓                           |
                                           |  8. Switch CR3, jump to entry  |
                                           +--------------------------------+
```

---

## 3. Host-side Changes (sumi-vm)

### 3.1 CLI

Add `--run <path>` to the `run` subcommand:

```
sumi-vm run <kernel> --share <dir> --run <path-relative-to-share-root>
```

`--run` requires `--share` — the binary must be accessible within the shared directory.
If `--run` is omitted, the kernel falls back to selftests (current behavior).

```rust
// sumi-vm/src/cmd/run.rs
#[derive(Debug, Args)]
pub struct RunCommand {
    #[arg(value_name = "KERNEL")]
    kernel: PathBuf,

    #[arg(long = "share", value_name = "DIR")]
    share_dir: Option<PathBuf>,

    /// Path to the user program, relative to the share root.
    #[arg(long = "run", value_name = "PATH")]
    run_path: Option<String>,
}
```

### 3.2 Boot Info Structure

A `BootInfo` structure is written to a fixed physical address in guest memory before the
vCPU starts. This is the only boot-time communication channel between host and guest.

```rust
// sumi-abi/src/boot_info.rs

pub const BOOT_INFO_MAGIC: u32 = 0x5355_4D49; // "SUMI"
pub const BOOT_INFO_VERSION: u32 = 1;

/// Boot-time parameters written by the host, read by the guest.
/// Placed at BOOT_INFO_ADDR in guest physical memory.
#[repr(C)]
pub struct BootInfo {
    pub magic: u32,            // BOOT_INFO_MAGIC
    pub version: u32,          // BOOT_INFO_VERSION
    pub flags: u32,            // bit 0: has_run_path
    pub _reserved: u32,        // alignment padding
    pub mem_size: u64,         // guest physical memory size in bytes
    pub run_path_offset: u32,  // byte offset from struct start to path string
    pub run_path_len: u32,     // path string length (UTF-8, no null terminator)
}
// sizeof(BootInfo) = 32 bytes
```

The path string is stored immediately after the struct:

```
BOOT_INFO_ADDR + 0x00:  BootInfo struct (32 bytes)
BOOT_INFO_ADDR + 0x20:  run_path string (up to 4064 bytes)
```

Total region: 4 KB (one x86 page). This fits within the reserved area before the page
allocator starts.

### 3.3 Boot Info Flags

| Bit | Name           | Meaning                                    |
|-----|----------------|--------------------------------------------|
| 0   | `HAS_RUN_PATH` | A user program path is present in BootInfo |
| 1–31 | —             | Reserved, must be zero                     |

### 3.4 Physical Placement

`BOOT_INFO_ADDR` is placed at a fixed low address within the reserved kernel region.
Address `0x7000` (28 KB) is chosen — well within the first page of the kernel binary,
safely below the page table structures and kernel stack.

```rust
// sumi-abi/src/arch/x86_64/layout.rs
pub const BOOT_INFO_ADDR: PhysicalAddr = PhysicalAddr::new(0x7000);
pub const BOOT_INFO_MAX_SIZE: usize = 0x1000; // 4 KB
```

This address is in the kernel code region (0..KERNEL_CODE_SIZE), which is part of guest
memory and writable by the host before boot. The kernel's `.text` section starts at
virtual offset 0 but the linker places code at a higher offset — `0x7000` is within the
ELF file's uninitialized gap (between the ELF header and the first section) and is safe to
reuse.

**Alternative**: If `0x7000` conflicts with ELF data, move it to a dedicated page just
before `KERNEL_STACK`. The exact address doesn't matter as long as both host and guest
agree via the shared constant.

### 3.5 Host Write Sequence

In `SumiVm::new()`, after `load_elf()` and before starting vCPUs:

```
1. Construct BootInfo { magic, version, flags, mem_size, run_path_offset, run_path_len }
2. Serialize the struct to guest memory at BOOT_INFO_ADDR
3. Write the path string at BOOT_INFO_ADDR + sizeof(BootInfo)
4. Start vCPUs
```

---

## 4. Guest-side Changes (sumi-kernel)

### 4.1 Modified Boot Sequence

```
_start()
  → KernelState::new()
  → syscall::init()
  → VirtioFsClient::init()
  → read_boot_info()              // NEW
  → if has_run_path:
      exec_user_program(path)     // NEW — never returns
    else:
      selftest::run_all()
      halt_forever()
```

### 4.2 ELF Parser — goblin

We use the `goblin` crate for ELF parsing in the guest kernel, same as in `sumi-vm`.
goblin supports `no_std` via feature flags (`default-features = false, features = ["elf64"]`).

```toml
# sumi-kernel/Cargo.toml
[dependencies]
goblin = { version = "0.10", default-features = false, features = ["elf64"] }
```

The kernel reads the entire binary into a buffer via virtio-fs, then parses it with
`goblin::elf::Elf::parse()`. This gives us validated `ProgramHeader` entries for free —
no custom parser, no maintenance burden, same code path as the host.

```rust
// sumi-kernel/src/exec.rs

use goblin::elf::Elf;
use goblin::elf::program_header::PT_LOAD;

fn load_elf(file_data: &[u8]) -> Result<ElfInfo, ExecError> {
    let elf = Elf::parse(file_data).map_err(ExecError::Elf)?;

    // Validate: must be static executable for x86_64
    if elf.header.e_type != goblin::elf::header::ET_EXEC {
        return Err(ExecError::UnsupportedType(elf.header.e_type));
    }
    if elf.header.e_machine != goblin::elf::header::EM_X86_64 {
        return Err(ExecError::UnsupportedMachine(elf.header.e_machine));
    }

    // Validate all PT_LOAD vaddrs are in user space (below DIRECT_MAP_OFFSET)
    for ph in elf.program_headers.iter().filter(|p| p.p_type == PT_LOAD) {
        if ph.p_vaddr >= DIRECT_MAP_OFFSET.as_u64() {
            return Err(ExecError::BadAddress(ph.p_vaddr));
        }
    }

    Ok(ElfInfo {
        entry: elf.entry,
        phdrs: &elf.program_headers,
    })
}
```

goblin requires `alloc` in `no_std` mode (it uses `Vec` for program headers internally).
This is already available via `KernelAllocator` implementing `GlobalAlloc`.

---

## 5. User Address Space

The user program runs in the **same address space** as the kernel — no separate page table,
no CR3 switch. ELF segments are mapped into the lower half of the kernel page table
(PML4 entries 0–255), which is currently unused.

This is the natural design for a unikernel: one address space, one ring, one process.
It eliminates TLB flushes, page table duplication, and syscall stack switching.

### 5.1 Virtual Memory Layout

```
0x0000_0000_0000_0000  ┌─────────────────────────────────────┐
                       │  (unmapped — null-pointer guard)     │
0x0000_0000_0040_0000  ├─────────────────────────────────────┤
                       │  ELF segments (ET_EXEC default base) │
                       │  .text, .rodata, .data, .bss         │
                       │  loaded from PT_LOAD headers          │
                       ├─────────────────────────────────────┤
brk_base               │  Heap (grows ↑ via brk)              │
                       │                                       │
                       │         ... free space ...             │
                       │                                       │
0x0000_7FFF_0000_0000  ├─────────────────────────────────────┤
                       │  mmap region (grows ↓)                │
                       │  anonymous pages, future file maps    │
                       ├─────────────────────────────────────┤
0x0000_7FFF_FF80_0000  │  User stack (8 MB, grows ↓)          │
                       │  RSP starts at USER_STACK_TOP         │
0x0000_7FFF_FFFF_FFFF  └─────────────────────────────────────┘
                       ← USER_PML4_LIMIT (PML4 index 256) ──→
0xFFFF_8880_0000_0000  ├─────────────────────────────────────┤
                       │  Direct map (kernel, already exists)  │
0xFFFF_FFFF_8000_0000  ├─────────────────────────────────────┤
                       │  Kernel code (already exists)         │
0xFFFF_FFFF_FFFF_FFFF  └─────────────────────────────────────┘
```

The upper half (kernel code, direct map) is already set up by the hypervisor at boot.
The lower half is populated by `exec_user_program()` by mapping pages into the existing
`KERNEL_PAGE_TABLE`. After `exit_group`, those lower-half mappings are freed.

### 5.2 New Layout Constants

```rust
// sumi-abi/src/arch/x86_64/layout.rs

/// Default stack top for the user program.
pub const USER_STACK_TOP: VirtualAddr = VirtualAddr::new(0x0000_7FFF_FFFF_F000);
pub const USER_STACK_SIZE: usize = 8 * 1024 * 1024; // 8 MB

/// Base address for mmap allocations (grows downward).
pub const USER_MMAP_BASE: VirtualAddr = VirtualAddr::new(0x0000_7FFF_0000_0000);

/// Default base address for PIE binaries (phase 2).
pub const USER_PIE_BASE: VirtualAddr = VirtualAddr::new(0x0000_0000_1000_0000);
```

---

## 6. Loading Process

### 6.1 High-level Flow

```rust
fn exec_user_program(path: &str) -> ! {
    // 1. Open the file and read it entirely into memory
    let fd = virtio_fs_open(path, O_RDONLY);
    let file_size = virtio_fs_getattr(fd).size;
    let file_buf = kalloc.alloc(file_size);
    virtio_fs_pread(fd, file_buf, file_size, 0);
    virtio_fs_close(fd);

    // 2. Parse ELF with goblin
    let elf = Elf::parse(file_buf).expect("invalid ELF");

    // 3. Map and load each PT_LOAD segment into the kernel page table
    let mut brk_end: u64 = 0;
    for ph in elf.program_headers.iter().filter(|p| p.p_type == PT_LOAD) {
        let start = align_down_2mb(ph.p_vaddr);
        let end = align_up_2mb(ph.p_vaddr + ph.p_memsz);

        // Allocate physical pages and map into lower half of KERNEL_PAGE_TABLE
        for vaddr in (start..end).step_by(PAGE_SIZE) {
            let paddr = PAGE_ALLOCATOR.alloc(1);
            zero_page(paddr);
            KERNEL_PAGE_TABLE.map_2mb(VirtualAddr::new(vaddr), paddr);
        }

        // Copy segment data from file buffer to mapped virtual address
        let dst = ph.p_vaddr as *mut u8;
        let src = &file_buf[ph.p_offset as usize..(ph.p_offset + ph.p_filesz) as usize];
        copy_nonoverlapping(src.as_ptr(), dst, src.len());
        // BSS (p_memsz - p_filesz) is already zeroed from zero_page()

        brk_end = max(brk_end, ph.p_vaddr + ph.p_memsz);
    }

    // 4. Free the file buffer — segment data is now in mapped pages
    kalloc.free(file_buf);

    // 5. Initialize brk to the end of the last segment (page-aligned)
    set_brk_base(align_up_2mb(brk_end));

    // 6. Allocate and map user stack (4 × 2MB = 8 MB)
    let stack_bottom = USER_STACK_TOP.as_u64() - USER_STACK_SIZE as u64;
    for i in 0..4 {
        let vaddr = stack_bottom + i * PAGE_SIZE;
        let paddr = PAGE_ALLOCATOR.alloc(1);
        zero_page(paddr);
        KERNEL_PAGE_TABLE.map_2mb(VirtualAddr::new(vaddr), paddr);
    }

    // 7. Prepare initial stack layout (Linux ABI)
    let sp = prepare_initial_stack(USER_STACK_TOP, path, &elf);

    // 8. Jump to entry point (no page table switch — same address space)
    jump_to_user(elf.entry, sp);
}
```

### 6.2 Segment Loading

Because user pages are mapped directly into the kernel page table, we can write to the
user virtual addresses immediately after mapping — no direct-map translation needed.
The pages are visible from the same address space we're executing in.

```
for each PT_LOAD:
    1. Map 2 MB pages covering [align_down(p_vaddr), align_up(p_vaddr + p_memsz))
    2. memcpy from file_buf[p_offset..p_offset+p_filesz] to p_vaddr
    3. BSS (p_memsz - p_filesz) is zero because pages were zero-filled on allocation
```

### 6.3 Page Granularity

Phase 1 uses 2 MB huge pages for user mappings. This means:

- All pages are mapped RWX (can't separate .text R-X from .data RW- within one 2 MB page).
- A segment at vaddr `0x401000` occupies the 2 MB page starting at `0x400000`.
  The first `0x1000` bytes of that page are zeroed (below the segment's load address).
- Memory waste is bounded: at most 2 MB per segment boundary.

This is acceptable for a ring-0 unikernel. Phase 2 can add 4 KB page support by extending
the page table to 4 levels (PML4 → PDPT → PD → PT).

---

## 7. Initial Stack Layout

At `_start`, the stack must conform to the System V x86_64 ABI for process initialization.
`RSP` points to `argc`:

```
high addresses
┌──────────────────────────────────┐
│ "program_path\0"                 │  string data
│ (padding to 16-byte align)       │
├──────────────────────────────────┤
│ AT_NULL    (0)     │ 0           │  auxiliary vector (two u64s each)
│ AT_PAGESZ  (6)     │ 4096       │
│ AT_ENTRY   (9)     │ entry_addr │
│ AT_PHNUM   (5)     │ phnum      │
│ AT_PHENT   (4)     │ 56         │  sizeof(Elf64_Phdr)
│ AT_PHDR    (3)     │ phdr_addr  │
├──────────────────────────────────┤
│ NULL                             │  end of envp
├──────────────────────────────────┤
│ NULL                             │  end of argv
│ argv[0] → "program_path\0"      │  pointer to string above
├──────────────────────────────────┤
│ argc = 1                         │  ← RSP (16-byte aligned)
└──────────────────────────────────┘
low addresses
```

### 7.1 Auxiliary Vector Entries

Minimal set for a statically-linked binary:

| Key        | Value | Description                                |
|------------|-------|--------------------------------------------|
| `AT_PHDR`  | 3     | Virtual address of program header table    |
| `AT_PHENT` | 4     | Size of one program header entry (56)      |
| `AT_PHNUM` | 5     | Number of program header entries            |
| `AT_PAGESZ`| 6     | System page size (4096 for ABI compat)     |
| `AT_ENTRY` | 9     | Program entry point                        |
| `AT_NULL`  | 0     | Terminator                                 |

`AT_PAGESZ` reports 4096 (not 2 MB) because user-space code (musl, glibc) uses this value
for `mmap` alignment calculations and expects the standard x86_64 page size.

### 7.2 Stack Preparation

The stack is built top-down in the already-mapped stack pages. Since user pages are mapped
in the kernel page table, we write directly to user virtual addresses:

```
fn prepare_initial_stack(stack_top: VirtualAddr, path: &str, elf: &ElfInfo) -> u64 {
    let mut sp = stack_top.as_u64();

    // 1. Write argv string
    sp -= path.len() + 1;  // include null terminator
    write_to_stack(sp, path.as_bytes());
    write_to_stack(sp + path.len(), &[0]);
    let argv0_addr = sp;

    // 2. Align to 16 bytes
    sp = align_down_16(sp);

    // 3. Calculate total entries to ensure final RSP is 16-byte aligned
    //    auxv: 6 pairs = 12 u64s
    //    envp NULL: 1 u64
    //    argv NULL: 1 u64
    //    argv[0]: 1 u64
    //    argc: 1 u64
    //    total: 16 u64s = 128 bytes — already 16-byte aligned

    // 4. Write auxiliary vector (bottom to top: AT_NULL first in memory)
    sp = push_auxv(sp, AT_NULL, 0);
    sp = push_auxv(sp, AT_PAGESZ, 4096);
    sp = push_auxv(sp, AT_ENTRY, elf.entry);
    sp = push_auxv(sp, AT_PHNUM, elf.phnum);
    sp = push_auxv(sp, AT_PHENT, 56);
    sp = push_auxv(sp, AT_PHDR, elf.phdr_vaddr);

    // 5. envp (empty)
    sp = push_u64(sp, 0);  // NULL

    // 6. argv
    sp = push_u64(sp, 0);          // NULL terminator
    sp = push_u64(sp, argv0_addr); // argv[0]

    // 7. argc
    sp = push_u64(sp, 1);

    sp  // return initial RSP
}
```

---

## 8. Entry Transfer

No page table switch is needed — user code is mapped in the kernel page table.
The trampoline sets RSP and jumps to the entry point with clean registers.

```asm
// sumi-kernel/src/arch/x86_64/exec.S
//
// jump_to_user(entry: u64, sp: u64)
//   RDI = entry point virtual address
//   RSI = initial RSP value

.global jump_to_user
jump_to_user:
    // Set stack pointer
    mov     %rsi, %rsp

    // Save entry point before clearing RDI
    mov     %rdi, %rax

    // Clear all general-purpose registers (clean state for user program)
    xor     %rbx, %rbx
    xor     %rcx, %rcx
    xor     %rdx, %rdx
    xor     %rdi, %rdi
    xor     %rsi, %rsi
    xor     %r8,  %r8
    xor     %r9,  %r9
    xor     %r10, %r10
    xor     %r11, %r11
    xor     %r12, %r12
    xor     %r13, %r13
    xor     %r14, %r14
    xor     %r15, %r15
    xor     %rbp, %rbp

    // Jump to user entry point
    jmp     *%rax
```

Everything stays in ring 0. The user program's `syscall` instructions hit `LSTAR`
(the existing syscall trampoline) which works because kernel and user share the same
address space. No TLB flush, no page table switch overhead.

---

## 9. Page Table Extensions

### 9.1 Mapping Interface

`RootPageTable` needs explicit map/unmap methods for the lower half:

```rust
impl<'i, DM: DirectMap> RootPageTable<'i, DM> {
    /// Map a 2 MB huge page: vaddr → paddr.
    /// Allocates intermediate PML4/PDPT entries on demand.
    /// vaddr must be in the lower half (< USER_PML4_LIMIT).
    /// Returns error if a mapping already exists at vaddr.
    pub fn map_2mb(
        &mut self,
        vaddr: VirtualAddr,
        paddr: PhysicalAddr,
    ) -> Result<()> {
        let entry = self.get(vaddr)?;  // walks PML4→PDPT→PD, allocating as needed
        if entry.is_present() {
            return Err(MemoryError::AlreadyMapped { addr: vaddr.as_usize() });
        }
        entry.set_paddr(paddr);
        Ok(())
    }

    /// Unmap the 2 MB page at vaddr. Returns the physical address of the freed page.
    pub fn unmap_2mb(&mut self, vaddr: VirtualAddr) -> Result<PhysicalAddr> {
        let entry = self.get(vaddr)?;
        if !entry.is_present() {
            return Err(MemoryError::NotMapped { addr: vaddr.as_usize() });
        }
        let paddr = entry.addr();
        entry.0 = 0;
        Ok(paddr)
    }
}
```

### 9.2 User Mapping Lifecycle

User mappings live in PML4 entries 0–255 of the kernel page table (the lower half, which
is currently empty). No separate page table is created.

1. `exec_user_program()` maps ELF segments and stack via `KERNEL_PAGE_TABLE.map_2mb()`.
2. `brk`/`mmap` syscalls add pages to the same table at runtime.
3. On `exit_group`: walk PML4 entries 0–`USER_PML4_LIMIT`, free all mapped pages and
   intermediate page tables. Then halt.

TLB invalidation: after unmapping pages (e.g., `munmap`, `exit_group`), issue `invlpg`
for each unmapped address. Bulk cleanup on exit can skip this since we halt immediately.

---

## 10. Required Syscall Implementations

### 10.1 Phase 1 — Minimal (hello world)

These must work for any program that calls `write()` and `exit()`:

| Nr  | Syscall        | Implementation                                           |
|-----|----------------|----------------------------------------------------------|
| 1   | `write`        | Already routed through virtio-fs. Needs stdout/stderr.   |
| 12  | `brk`          | Track current brk, allocate/map 2 MB pages on grow.      |
| 60  | `exit`         | Free lower-half mappings, halt with exit code.            |
| 231 | `exit_group`   | Same as `exit` (single-threaded).                         |

### 10.2 Phase 2 — musl libc init

musl's `__libc_start_main` calls these before `main()`:

| Nr  | Syscall          | Implementation                                          |
|-----|------------------|---------------------------------------------------------|
| 9   | `mmap`           | Anonymous only. Find free region, allocate, map.         |
| 10  | `mprotect`       | No-op (2 MB pages, all RWX). Return 0.                  |
| 11  | `munmap`         | Unmap pages, free physical memory.                       |
| 63  | `uname`          | Return "sumi" as sysname, "0.1.0" as release.           |
| 158 | `arch_prctl`     | `SET_FS`: write `FS_BASE` MSR directly (ring 0).        |
| 218 | `set_tid_address` | Store pointer, return PID (1).                          |
| 302 | `prlimit64`      | Return default limits (RLIMIT_STACK = 8 MB, etc.).      |

### 10.3 brk Implementation

```rust
// Global state
static BRK_BASE: spin::Mutex<VirtualAddr> = spin::Mutex::new(VirtualAddr::new(0));
static BRK_CURRENT: spin::Mutex<VirtualAddr> = spin::Mutex::new(VirtualAddr::new(0));

fn sys_brk(args: &SyscallArgs) -> SyscallResult {
    let requested = args.arg0 as u64;
    let mut current = BRK_CURRENT.lock();
    let base = BRK_BASE.lock();

    if requested == 0 {
        return *current as SyscallResult;
    }

    if requested < base.as_u64() {
        return *current as SyscallResult;  // refuse to shrink below base
    }

    let old_end = align_up_2mb(current.as_u64());
    let new_end = align_up_2mb(requested);

    if new_end > old_end {
        // Grow: allocate and map new pages
        for vaddr in (old_end..new_end).step_by(PAGE_SIZE) {
            let paddr = palloc.alloc(1);
            user_page_table.map_2mb(VirtualAddr::new(vaddr), paddr);
        }
    } else if new_end < old_end {
        // Shrink: unmap and free pages
        for vaddr in (new_end..old_end).step_by(PAGE_SIZE) {
            let paddr = user_page_table.unmap_2mb(VirtualAddr::new(vaddr));
            palloc.free(paddr);
        }
    }

    *current = VirtualAddr::new(requested as usize);
    requested as SyscallResult
}
```

### 10.4 Anonymous mmap (Phase 2)

```rust
fn sys_mmap(args: &SyscallArgs) -> SyscallResult {
    let len = args.arg1 as usize;
    let flags = args.arg3 as i32;

    if flags & MAP_ANONYMOUS == 0 {
        return -ENOSYS;  // file-backed not supported yet
    }

    let aligned_len = align_up_2mb(len);
    let vaddr = mmap_allocator.find_free(aligned_len);

    for offset in (0..aligned_len).step_by(PAGE_SIZE) {
        let paddr = palloc.alloc(1);
        zero_page(paddr);
        user_page_table.map_2mb(vaddr + offset, paddr);
    }

    vaddr as SyscallResult
}
```

### 10.5 arch_prctl (SET_FS)

```rust
fn sys_arch_prctl(args: &SyscallArgs) -> SyscallResult {
    let code = args.arg0 as i32;
    let addr = args.arg1;

    match code {
        ARCH_SET_FS => {
            // Write FS_BASE MSR directly — we're in ring 0
            unsafe { wrmsr(IA32_FS_BASE, addr) };
            0
        }
        ARCH_GET_FS => {
            let val = unsafe { rdmsr(IA32_FS_BASE) };
            // Write val to user pointer at addr
            unsafe { *(addr as *mut u64) = val };
            0
        }
        _ => -EINVAL,
    }
}
```

---

## 11. stdout / stderr Routing

User programs write to fd 1 (stdout) and fd 2 (stderr). Phase 1 routes these to the
existing `debugcon` I/O port (`0xE9`), which the host captures.

The FD table must pre-populate entries for fds 0, 1, 2 at program load time:

| fd | Type   | Backing              |
|----|--------|----------------------|
| 0  | stdin  | Returns EOF (read=0) |
| 1  | stdout | debugcon port `0xE9` |
| 2  | stderr | debugcon port `0xE9` |

`sys_write(fd=1/2, buf, count)`: iterate `buf[0..count]`, write each byte to port `0xE9`.

---

## 12. Module Layout

```
sumi-abi/
  src/
    boot_info.rs             NEW   BootInfo struct, magic, flags
    arch/x86_64/layout.rs    MOD   BOOT_INFO_ADDR, USER_STACK_TOP, USER_MMAP_BASE

sumi-vm/
  src/
    cmd/run.rs               MOD   --run flag, pass run_path through VmCreateInfo
    vm.rs                    MOD   write BootInfo to guest memory before vCPU start

sumi-kernel/
  Cargo.toml                 MOD   add goblin dependency (no_std, elf64)
  src/
    exec.rs                  NEW   exec_user_program(), prepare_initial_stack(),
                                   jump_to_user (inline asm)
    arch/x86_64/
      pagetable.rs           MOD   map_2mb(), unmap_2mb() methods
    syscall/handlers/
      memory.rs              MOD   sys_brk, sys_mmap (anonymous), sys_munmap
      process.rs             MOD   sys_exit_group (clean shutdown), sys_arch_prctl
    kernel_main.rs           MOD   read BootInfo, call exec_user_program()
```

---

## 13. Implementation Plan

### Phase 1: Static Hello World

1. Define `BootInfo` in `sumi-abi`, add `BOOT_INFO_ADDR` to layout constants.
2. Modify `sumi-vm`: add `--run` flag, write `BootInfo` to guest memory.
3. Add `goblin` dependency to `sumi-kernel` (no_std, elf64 features).
4. Add `map_2mb` / `unmap_2mb` to `RootPageTable`.
5. Write `exec_user_program()`: parse ELF with goblin, load segments into kernel page
   table lower half, set up stack, jump to entry.
6. Implement `sys_brk` (minimal), `sys_exit_group` (halt with code).
7. Pre-populate fd 0/1/2, route stdout/stderr to debugcon.
8. **Milestone**: `sumi-vm run kernel --share ./test --run /hello` prints "Hello, world!"
   and exits cleanly.

### Phase 2: musl libc Support

1. Implement `sys_arch_prctl` (SET_FS / GET_FS for TLS).
2. Implement `sys_mmap` (anonymous, MAP_PRIVATE).
3. Implement `sys_munmap`.
4. Stub `sys_mprotect` (return 0).
5. Implement `sys_set_tid_address`, `sys_uname`, `sys_prlimit64`.
6. **Milestone**: musl-static linked binary with full libc init runs (e.g., `cat`, `ls`).

### Phase 3: PIE + 4 KB Pages

1. Support `ET_DYN` with a fixed base address.
2. Extend page table to 4 levels for 4 KB user pages.
3. Sub-page allocator for 4 KB physical pages.
4. Proper permission bits (NX for data, read-only for .rodata).
5. **Milestone**: PIE-compiled static binary runs.

---

## 14. Open Questions

1. **BootInfo placement** — `0x7000` is within the kernel ELF. If the linker places data
   there, we'll need a different address. Verify with `readelf -l sumi-kernel` that no
   `PT_LOAD` segment's `[p_paddr, p_paddr+p_memsz)` covers `0x7000`. If it does, allocate
   a dedicated page after the page table structures.

2. **goblin alloc requirement** — goblin in `no_std` mode requires the `alloc` crate
   (it uses `Vec` for program headers). This means `KernelAllocator` must implement
   `GlobalAlloc` and be registered with `#[global_allocator]`. Verify this works before
   committing to goblin; if not, a minimal hand-rolled parser is the fallback.

3. **File buffer size** — We read the entire binary into memory before parsing. A large
   statically-linked binary (10+ MB for Go) requires that much contiguous allocation.
   The kernel allocator may need to request multiple pages from palloc. An alternative
   is to parse only the header (first 4 KB), then read segments one at a time directly
   into their mapped pages.

4. **Page allocator fragmentation** — Loading many segments may fragment the page allocator.
   Currently it scans linearly for contiguous free runs. Single-page allocations (which is
   what ELF loading does) always succeed if memory is available, so this is not a concern.

5. **Lower-half cleanup on exit** — `exit_group` must walk PML4 entries 0–255 and free
   all mapped pages + intermediate page tables. The existing `PageTable::free()` already
   does this (it stops at `USER_PML4_LIMIT`), but it frees the PML4 itself — we need a
   variant that frees the lower-half contents without freeing the kernel page table root.
