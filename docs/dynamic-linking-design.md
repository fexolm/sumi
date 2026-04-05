# Dynamic Linking (Shared Libraries) -- Design Document

## 1. Goal

Run dynamically-linked ELF binaries under sumi. Primary target: **musl-libc**.
A simple `hello-world` compiled with `musl-gcc` (dynamically linked) should boot,
load `libc.so` via the interpreter, and execute to completion.

### Non-goals

- glibc support (complex ldso, NSS, locale machinery).
- `dlopen` / `dlclose` (runtime library loading/unloading).
- ASLR (fixed base addresses are fine for a unikernel).
- 4 KB page support (remains 2 MB huge pages only).
- Full copy-on-write with page faults (COW is done eagerly at MAP_FIXED time).

---

## 2. Background: How Dynamic Linking Works

The kernel's role is small. It loads two ELFs (the program and the interpreter),
sets up the stack with metadata, and jumps to the interpreter. Everything else --
library discovery, segment mapping, symbol resolution, relocations -- is done in
userspace by the dynamic linker (`ld.so`).

```
Kernel                              Userspace (ld.so)
------                              -----------------
1. Parse main binary ELF
2. Detect PT_INTERP --> "/lib/ld-musl-x86_64.so.1"
3. Load main binary PT_LOAD segments
4. Load interpreter PT_LOAD segments
5. Build stack: argc, argv, envp, auxv
   (AT_BASE, AT_PHDR, AT_ENTRY, ...)
6. Jump to interpreter e_entry
                                    7.  Read auxv from stack
                                    8.  Find main binary's PT_DYNAMIC via AT_PHDR
                                    9.  Walk DT_NEEDED entries
                                    10. For each library:
                                        openat("/lib/libfoo.so")
                                        mmap(fd, MAP_PRIVATE) -- file-backed
                                        mmap(MAP_FIXED) -- per-segment placement
                                        close(fd)
                                    11. Perform relocations (GOT/PLT patching)
                                    12. Set up TLS (arch_prctl ARCH_SET_FS)
                                    13. Call main binary's entry point
```

### musl specifics

In musl, the interpreter (`ld-musl-x86_64.so.1`) IS `libc.so` -- same binary,
symlinked. So for a binary that only needs `libc.so`, the interpreter recognizes
it's already loaded and skips step 10 entirely. This means the simplest case
(hello world) doesn't require any library mmap from userspace -- the kernel loads
everything.

---

## 3. ELF Loader Changes

**File:** `sumi-kernel/src/exec.rs`

### 3.1 Current State

- Only `ET_EXEC` (static, non-PIE) binaries supported.
- `PT_INTERP` is not checked.
- All segments loaded at their absolute `p_vaddr`.
- Entry point is the main binary's `e_entry`.

### 3.2 New Flow

```
exec_user_program_inner(path):
  file_data = read_file(path)
  elf = parse(file_data)

  // NEW: determine binary type and base address
  base = match elf.header.e_type:
    ET_EXEC => 0                        // absolute addresses
    ET_DYN  => PIE_LOAD_BASE            // relocate to fixed base
    _       => error

  // Load main binary segments (with base offset)
  brk_base = load_segments_at_base(file_data, elf, base)

  // NEW: check for dynamic linker
  interp_info = if let Some(interp_path) = elf.interpreter:
    interp_data = read_file(interp_path)
    interp_elf = parse(interp_data)
    assert(interp_elf.header.e_type == ET_DYN)
    load_segments_at_base(interp_data, interp_elf, INTERP_LOAD_BASE)
    Some(InterpInfo {
      base: INTERP_LOAD_BASE,
      entry: INTERP_LOAD_BASE + interp_elf.entry,
    })
  else:
    None

  // Build auxv with interpreter info
  entry = interp_info.map(|i| i.entry).unwrap_or(base + elf.entry)
  sp = setup_stack(path, elf_info, interp_info)

  // Set brk
  BRK_BASE = brk_base
  BRK_CURRENT = brk_base

  jump_to_user(entry, sp)
```

### 3.3 `load_segments_at_base`

Refactor of existing `load_segments`. Adds `base` parameter to all address
calculations:

```rust
fn load_segments_at_base(
    file_data: &[u8],
    elf: &Elf,
    base: u64,
) -> Result<VirtualAddr, ExecError> {
    for ph in elf.program_headers.iter().filter(|p| p.p_type == PT_LOAD) {
        let vaddr = base + ph.p_vaddr;       // <-- apply base offset
        let seg_end = vaddr + ph.p_memsz;

        // ... validation (same as before, but using vaddr/seg_end) ...

        let start = align_down_2mb(vaddr);
        let end = align_up_2mb(seg_end);

        // Map 2 MB pages (check for already-mapped first)
        for page in (start..end).step_by(PAGE_SIZE) {
            let va = VirtualAddr::new(page as usize);
            if KERNEL_PAGE_TABLE.get_if_present(va)?.is_none() {
                let paddr = PAGE_ALLOCATOR.alloc(1)?;
                zero_page(paddr);
                KERNEL_PAGE_TABLE.map_2mb(va, paddr)?;
            }
        }

        // Copy segment data at the offset address
        copy(file_data[ph.p_offset..], vaddr, ph.p_filesz);
        // BSS already zeroed

        brk_end = max(brk_end, seg_end);
    }
    Ok(align_up_2mb(brk_end))
}
```

For `ET_EXEC`, `base = 0` so behavior is identical to the current code.

### 3.4 Base Address Constants

```rust
// sumi-abi/src/arch/x86_64/layout.rs

/// Base address for loading PIE (ET_DYN) main binaries.
/// Linux default for non-ASLR PIE on x86_64.
pub const PIE_LOAD_BASE: u64 = 0x0040_0000;

/// Base address for loading the dynamic linker (interpreter).
/// Placed high in user space, well below the mmap region.
pub const INTERP_LOAD_BASE: u64 = 0x7f00_0000_0000;
```

Address space after loading a PIE binary with interpreter:

```
0x0000_0000_0040_0000  Main binary (ET_DYN, base = PIE_LOAD_BASE)
                       .text, .rodata, .data, .bss
0x0000_0000_00??_????  brk base (heap grows up)

        ~127 TB of free space

0x0000_7f00_0000_0000  Interpreter / ld.so (INTERP_LOAD_BASE)
                       .text, .rodata, .data, .bss

        ~255 GB gap

0x0000_7FFF_0000_0000  mmap region (grows down from USER_MMAP_BASE)
                       libraries loaded by ld.so via mmap
0x0000_7FFF_FF80_0000  User stack (8 MB, grows down)
0x0000_7FFF_FFFF_F000  USER_STACK_TOP
```

The mmap region (where ld.so places libraries) starts at `0x7FFF_0000_0000` and
grows downward. With ~1 TB between the mmap region and the interpreter, there's
ample space for library mappings.

---

## 4. Auxiliary Vector Changes

**File:** `sumi-kernel/src/exec.rs`

### 4.1 Current auxv

| Key | Constant | Value |
|-----|----------|-------|
| `AT_PHDR` | 3 | phdr virtual address |
| `AT_PHENT` | 4 | 56 (sizeof Elf64_Phdr) |
| `AT_PHNUM` | 5 | number of program headers |
| `AT_PAGESZ` | 6 | 4096 |
| `AT_ENTRY` | 9 | main binary entry point |
| `AT_NULL` | 0 | terminator |

### 4.2 New entries

| Key | Constant | Value | Why |
|-----|----------|-------|-----|
| `AT_BASE` | 7 | interpreter load base (or 0) | ld.so needs to know its own base for self-relocation |
| `AT_RANDOM` | 25 | pointer to 16 bytes on stack | musl reads this for stack canary initialization; crash if absent |
| `AT_UID` | 11 | 0 | musl libc init reads these |
| `AT_EUID` | 12 | 0 | |
| `AT_GID` | 13 | 0 | |
| `AT_EGID` | 14 | 0 | |
| `AT_SECURE` | 23 | 0 | tells ld.so binary is not suid |

### 4.3 AT_PHDR for ET_DYN

For `ET_EXEC`: `AT_PHDR` = absolute address of program headers (existing behavior).

For `ET_DYN`: `AT_PHDR` = `base + elf.header.e_phoff` (or `base + PT_PHDR.p_vaddr`).
The dynamic linker uses `AT_PHDR` to locate the main binary's `PT_DYNAMIC` segment,
which contains `DT_NEEDED`, relocation tables, etc.

### 4.4 AT_ENTRY for interpreter case

When an interpreter is loaded:
- `AT_ENTRY` = main binary's entry point (`base + elf.entry`), NOT the interpreter's.
- The actual jump target is the interpreter's entry (`INTERP_LOAD_BASE + interp.entry`).
- The interpreter uses `AT_ENTRY` to know where to transfer control after initialization.

### 4.5 AT_RANDOM implementation

Allocate 16 bytes on the stack before auxv and fill with deterministic data (no
entropy source in the unikernel). musl uses this for stack canary; the value
doesn't need to be cryptographically random for a unikernel.

```rust
// Write 16 "random" bytes
sp -= 16;
let random_addr = sp;
unsafe {
    // Deterministic but non-zero (musl checks for zero canary)
    let random: [u8; 16] = [
        0x73, 0x75, 0x6D, 0x69, // "sumi"
        0x72, 0x61, 0x6E, 0x64, // "rand"
        0x01, 0x02, 0x03, 0x04,
        0x05, 0x06, 0x07, 0x08,
    ];
    core::ptr::copy_nonoverlapping(random.as_ptr(), sp as *mut u8, 16);
}
// ... later in auxv:
push_auxv(sp, AT_RANDOM, random_addr);
```

---

## 5. mmap MAP_FIXED Changes

**File:** `sumi-kernel/src/syscall/handlers/memory.rs`

### 5.1 Problem

The dynamic linker loads library segments using this sequence:

```c
// 1. Reserve address range (file-backed, covers entire .so span)
base = mmap(NULL, span_size, prot, MAP_PRIVATE, fd, file_offset);

// 2. Per-segment placement (MAP_FIXED at 4KB-aligned offsets)
mmap(base + seg_vaddr_4k_aligned, seg_size, prot,
     MAP_PRIVATE | MAP_FIXED, fd, seg_offset_4k_aligned);
```

Step 2 fails because `sys_mmap` rejects `MAP_FIXED` with non-2MB-aligned addresses.

Additionally, step 2's VMA overlap removal tears down the entire reservation VMA
from step 1 (freeing all pages), then re-allocates pages for just the segment range.
If two segments share a 2 MB page, the second MAP_FIXED would destroy the first
segment's data.

### 5.2 Key Insight

With 2 MB pages and the ELF congruence rule (`p_vaddr ≡ p_offset mod PAGE_SIZE`),
the file data at any virtual address after the initial mmap (step 1) is identical
to what MAP_FIXED would load. The MAP_FIXED calls exist only to change page
protection -- which is a no-op in sumi (all pages RWX).

Therefore, for file-backed `MAP_FIXED` into already-mapped pages, we can:
1. Verify pages are mapped
2. Re-read file data for correctness (handles edge cases)
3. Return the requested address
4. Skip VMA modifications entirely

### 5.3 New MAP_FIXED Handler

```rust
pub fn sys_mmap(args: &SyscallArgs) -> SyscallResult {
    // ... existing arg parsing ...

    // Relax alignment: accept 4KB-aligned addresses for MAP_FIXED.
    // Linux requires page alignment; we accept 4KB (the reported AT_PAGESZ).
    if flags & MAP_FIXED != 0 && addr_hint % 4096 != 0 {
        return EINVAL;
    }

    // File-backed MAP_FIXED: fast path for dynamic linker segment placement.
    if flags & MAP_FIXED != 0 && flags & MAP_ANONYMOUS == 0 {
        return map_fixed_file(addr_hint, len, fd, offset);
    }

    // ... rest of existing dispatch (anonymous, non-fixed file, etc.) ...
}
```

```rust
/// Handle file-backed MAP_FIXED: ensure pages are mapped, read file data.
/// Does not modify VMA table -- pages are tracked by the reservation VMA.
fn map_fixed_file(addr: u64, len: usize, fd: i32, offset: usize) -> SyscallResult {
    let (fuse_fh, _) = /* look up fd */;

    // Compute 2 MB-aligned page range covering [addr, addr + len).
    let aligned_start = align_down_2mb(addr);
    let aligned_end = align_up_2mb(addr + len as u64);

    // Ensure all 2 MB pages in range are mapped.
    for page in (aligned_start..aligned_end).step_by(PAGE_SIZE) {
        let va = VirtualAddr::new(page as usize);
        match KERNEL_PAGE_TABLE.get_if_present(va) {
            Ok(Some(entry)) => {
                // Page exists. Check if it's a DAX page that needs COW.
                let paddr = entry.address();
                if is_dax_page(paddr) {
                    // Replace DAX page with private copy (see 5.4).
                    replace_dax_with_private(va, paddr)?;
                }
                // Otherwise: regular page, reuse it.
            }
            Ok(None) => {
                // Page not mapped -- allocate.
                let paddr = PAGE_ALLOCATOR.alloc(1)?;
                zero_page(paddr);
                KERNEL_PAGE_TABLE.map_2mb(va, paddr)?;
            }
            Err(_) => return ENOMEM,
        }
    }

    // Read file data at the exact requested address.
    if let Some(fs) = VIRTIO_FS.get() {
        let read_len = len.min(u32::MAX as usize) as u32;
        let _ = fs_transfer_chunked(
            |off, pa, cnt| fs.read(fuse_fh, off, pa, cnt),
            offset as u64,
            addr,
            read_len,
        );
    }

    addr as SyscallResult
}
```

### 5.4 DAX Page Replacement (COW at MAP_FIXED time)

The initial reservation mmap for a library uses the DAX path (MAP_PRIVATE
read-only .text). When a subsequent MAP_FIXED targets the same 2 MB page with
PROT_WRITE (.data segment), we replace the DAX page with a private physical
copy. This preserves zero-copy for read-only pages while preventing writes from
reaching the host file.

```rust
fn is_dax_page(paddr: PhysicalAddr) -> bool {
    paddr >= DAX_WINDOW_BASE && paddr < DAX_WINDOW_BASE.add(DAX_WINDOW_SIZE)
}

fn replace_dax_with_private(va: VirtualAddr, dax_paddr: PhysicalAddr)
    -> Result<(), SyscallResult>
{
    let new_paddr = PAGE_ALLOCATOR.alloc(1).map_err(|_| ENOMEM)?;
    // Copy DAX content to new page.
    let src = dax_paddr.to_virtual(&KERNEL_DIRECT_MAP);
    let dst = new_paddr.to_virtual(&KERNEL_DIRECT_MAP);
    unsafe {
        core::ptr::copy_nonoverlapping(
            src.as_ptr::<u8>(), dst.as_mut_ptr::<u8>(), PAGE_SIZE,
        );
    }
    // Replace mapping.
    KERNEL_PAGE_TABLE.unmap_2mb(va).map_err(|_| ENOMEM)?;
    KERNEL_PAGE_TABLE.map_2mb(va, new_paddr).map_err(|_| ENOMEM)?;
    Ok(())
}
```

### 5.5 DAX Slot Cleanup on Partial Replacement

A single DAX allocation may cover N contiguous 2 MB slots (one `DaxAllocator::alloc(N)`
call). When `map_fixed_file` replaces only some of those pages, we have a partial
DAX teardown problem: the allocator tracks contiguous ranges, not individual slots.

**Approach: deferred cleanup.**

When `replace_dax_with_private` converts a DAX page, it does NOT free the DAX slot
immediately. The slot remains allocated but the page table no longer points to it.
The DAX slot is freed when the **entire VMA** is torn down (via `munmap` or VM exit).

This is correct because:
1. The DAX window is large (128 GB / 65536 slots). A few orphaned slots per library
   load are negligible.
2. The VMA still tracks the original DAX allocation range. On `tear_down_vma`, the
   full range is freed via `FUSE_REMOVEMAPPING` + `DaxAllocator::free()`.
3. Pages that were replaced with private copies are freed as regular physical memory
   during `tear_down_vma` -- `unmap_2mb` returns their physical address, and we check
   whether it's a DAX address or a regular page to decide the cleanup path.

Updated `tear_down_vma` for mixed DAX/private pages:

```rust
fn tear_down_vma(vma: Vma) {
    let start = vma.start.as_usize();
    let end = vma.end.as_usize();

    match vma.backing {
        MappingBacking::Dax { dax_offset, .. } => {
            // Unmap all pages. Some may be original DAX, some replaced private copies.
            let mut vaddr = start;
            while vaddr < end {
                if let Ok(paddr) = KERNEL_PAGE_TABLE.unmap_2mb(VirtualAddr::new(vaddr)) {
                    if !is_dax_page(paddr) {
                        // This page was replaced with a private copy -- free it.
                        let _ = PAGE_ALLOCATOR.free(paddr);
                    }
                    // DAX pages don't need individual freeing -- the slot range
                    // is freed below in one call.
                }
                vaddr += PAGE_SIZE;
            }
            // Release the entire DAX slot range.
            if let Some(fs) = VIRTIO_FS.get() {
                let len = (end - start) as u64;
                let _ = fs.remove_mapping(dax_offset, len);
            }
            let slot_count = (end - start) / PAGE_SIZE;
            DAX_ALLOCATOR.lock().free(dax_offset, slot_count);
        }
        MappingBacking::Anonymous | MappingBacking::PrivateFile { .. } => {
            // Existing behavior: unmap and free all physical pages.
            let mut vaddr = start;
            while vaddr < end {
                if let Ok(paddr) = KERNEL_PAGE_TABLE.unmap_2mb(VirtualAddr::new(vaddr)) {
                    let _ = PAGE_ALLOCATOR.free(paddr);
                }
                vaddr += PAGE_SIZE;
            }
        }
    }
}
```

**Why not split the DAX allocation?**

Splitting a contiguous DAX range (e.g., freeing slot 3 out of [0..5]) would require
the `DaxAllocator` to track free holes within allocated ranges. The current bitmap
allocator only tracks allocated/free at the slot level and requires contiguous
`alloc(N)` / `free(offset, N)` pairs. Adding split support adds complexity for a
rare case (most libraries fit in 1-2 pages). The deferred cleanup approach is
simpler and wastes at most a few 2 MB DAX slots per library -- negligible in a
128 GB window.

### 5.6 MAP_FIXED for Anonymous Mappings

`MAP_FIXED | MAP_ANONYMOUS` (e.g., explicit address reservation) keeps existing
behavior: remove overlapping VMAs, allocate fresh pages, create VMA. Only relax
the alignment check from 2 MB to 4 KB.

---

## 6. Syscall Readiness

All syscalls needed by musl's dynamic linker are already implemented:

| Syscall | Nr | Status | Notes |
|---------|----|--------|-------|
| `openat` | 257 | Done | opens `.so` files |
| `fstat` / `newfstatat` | 5/262 | Done | file size for mmap |
| `mmap` (MAP_PRIVATE file) | 9 | Done | loads library segments |
| `mmap` (MAP_FIXED) | 9 | **Needs fix** | 4KB alignment (Section 5) |
| `mprotect` | 10 | Done (no-op) | all pages RWX |
| `munmap` | 11 | Done | cleanup |
| `close` | 3 | Done | |
| `read` / `pread64` | 0/17 | Done | ELF header reads |
| `arch_prctl` (SET_FS) | 158 | Done | TLS base (wrmsr FS_BASE) |
| `set_tid_address` | 218 | Done | returns TID=1 |
| `rt_sigprocmask` | 14 | Done | signal mask during init |
| `brk` | 12 | Done | heap |
| `uname` | 63 | Done | returns "sumi" |
| `prlimit64` | 302 | Done | resource limits |
| `readlink` | 89 | Done | `/proc/self/exe` may be called |

The **only blocking change** beyond the ELF loader is the `MAP_FIXED` alignment
relaxation in `sys_mmap`.

---

## 7. Filesystem Layout

The shared directory (virtio-fs root) must contain the interpreter and any needed
libraries. For musl:

```
shared-dir/
  bin/
    hello                        # dynamically-linked binary
  lib/
    ld-musl-x86_64.so.1 -> libc.so   # interpreter (symlink)
    libc.so                           # musl libc
```

The binary's `PT_INTERP` typically says `/lib/ld-musl-x86_64.so.1`. The kernel
resolves this path on the virtio-fs filesystem.

musl's library search order:
1. `DT_RPATH` / `DT_RUNPATH` from the binary
2. `/etc/ld-musl-x86_64.path`
3. Default: `/lib:/usr/local/lib:/usr/lib`

Since all paths resolve against the virtio-fs root, the user just places libraries
in `/lib/` under the shared directory.

---

## 8. 2 MB Alignment Implications

### 8.1 Memory Waste

Each library occupies at least one 2 MB page, regardless of actual size.
musl `libc.so` is ~800 KB, so ~1.2 MB is wasted per library. For a unikernel
running a single binary with a few libraries, this is acceptable.

| Library | Size | Pages (2 MB) | Waste |
|---------|------|--------------|-------|
| musl libc.so | ~800 KB | 1 | ~1.2 MB |
| libm.so (musl) | ~450 KB | 1 | ~1.5 MB |
| libpthread.so (musl) | included in libc | 0 | 0 |
| Typical application total | | 2-5 pages | 4-10 MB |

### 8.2 Segment Placement

With 2 MB pages, `.text` and `.data` segments of a small library (~1 MB total)
share a single 2 MB page. This means:
- No memory protection between segments (fine -- all pages are RWX anyway).
- `MAP_FIXED` calls for different segments may target the same 2 MB page.
- The `map_fixed_file` handler reuses already-mapped pages (Section 5.3).

For large libraries (> 2 MB), segments span multiple pages and work naturally.

---

## 9. Implementation Plan

### Phase 1: Core Dynamic Linking

Minimal changes to make a musl dynamically-linked binary work.

**Step 1: ELF loader** (`exec.rs`)
- [ ] Refactor `load_segments` into `load_segments_at_base(data, elf, base)`
- [ ] Accept `ET_DYN` main binaries (with `base = PIE_LOAD_BASE`)
- [ ] Detect `PT_INTERP`, read interpreter from virtio-fs
- [ ] Load interpreter at `INTERP_LOAD_BASE`
- [ ] Jump to interpreter's entry when present

**Step 2: Auxiliary vector** (`exec.rs`)
- [ ] Add `AT_BASE` (interpreter base, or 0 if no interpreter)
- [ ] Add `AT_RANDOM` (16 deterministic bytes on stack)
- [ ] Add `AT_UID`, `AT_EUID`, `AT_GID`, `AT_EGID`, `AT_SECURE`
- [ ] For ET_DYN: `AT_PHDR = base + phdr_offset`
- [ ] For interpreter: `AT_ENTRY = base + main_binary.e_entry`

**Step 3: MAP_FIXED** (`memory.rs`)
- [ ] Relax alignment check from 2 MB to 4 KB
- [ ] Add `map_fixed_file()` handler for file-backed MAP_FIXED
- [ ] Ensure pages mapped, read file data, return requested address
- [ ] DAX page detection: `is_dax_page()` checks if paddr is in DAX window
- [ ] `replace_dax_with_private()`: alloc page, copy DAX content, remap
- [ ] Update `tear_down_vma` for mixed DAX/private pages in Dax-backed VMAs

**Step 4: Layout constants** (`layout.rs`)
- [ ] Add `PIE_LOAD_BASE` and `INTERP_LOAD_BASE`

### Phase 2: Polish

- [ ] VMA splitting for partial munmap (proper overlap handling)
- [ ] `readlink("/proc/self/exe")` returns the binary path
- [ ] Handle `ET_DYN` interpreter that itself has `PT_INTERP` (error)

### Phase 3: Beyond musl (future)

- [ ] Support glibc's `ld.so` (more syscalls, `/proc` requirements)
- [ ] `dlopen` / `dlclose` (requires proper VMA lifecycle)
- [ ] Multiple library search paths (LD_LIBRARY_PATH equivalent)

---

## 10. Testing Strategy

### Unit Tests (`cargo test`)

| Test | Verifies |
|------|----------|
| `load_segments_at_base_zero` | base=0 behaves like current `load_segments` |
| `load_segments_at_base_offset` | segments placed at base+vaddr correctly |
| `auxv_with_interpreter` | AT_BASE, AT_ENTRY, AT_RANDOM present and correct |
| `auxv_without_interpreter` | AT_BASE=0, AT_ENTRY=main entry (regression) |
| `map_fixed_4kb_aligned` | MAP_FIXED at 4KB boundary succeeds |
| `map_fixed_reuse_page` | MAP_FIXED into already-mapped 2MB page reuses it |
| `map_fixed_dax_cow` | MAP_FIXED over DAX page copies content to private page |
| `tear_down_mixed_vma` | Dax VMA with some replaced pages frees both correctly |

### KVM Integration Tests (`make self-test`)

| Test | Verifies |
|------|----------|
| Static binary (regression) | Existing ET_EXEC binary still works |
| musl hello-world (dynamic) | Loads interpreter, prints "hello world", exits 0 |
| musl with libm | `sin(1.0)` -- loads libc.so + uses math |
| ET_DYN PIE binary | PIE main binary + interpreter |

### Manual Verification

```bash
# Compile a musl dynamically-linked binary
musl-gcc -o hello hello.c

# Prepare shared directory
mkdir -p shared/lib shared/bin
cp hello shared/bin/
cp /lib/ld-musl-x86_64.so.1 shared/lib/
cp /usr/lib/x86_64-linux-musl/libc.so shared/lib/

# Run under sumi
cargo run -p sumi-vm -- run --share shared /bin/hello
```

Expected output: "Hello, world!" followed by clean exit (code 0).

---

## 11. Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| musl reads `/proc/self/exe` | crash on readlink | Implement readlink for `/proc/self/exe` (return binary path) |
| musl uses `mremap` | ENOSYS crash | musl doesn't use mremap; glibc does. Not a risk for Phase 1 |
| 2 MB page waste for many libraries | high memory use | Acceptable for unikernel. Future: 4 KB page support |
| Library path not found | ld.so fails to load | Document required fs layout; provide error message |
| Missing auxv entry | ld.so crash | Test with strace on Linux to identify all reads; add entries |
| AT_RANDOM is deterministic | weak stack canary | Acceptable for unikernel (no multi-tenant threat model) |
| DAX pages + writable MAP_FIXED | writes to host file | `replace_dax_with_private()` copies DAX page to private memory before write (Section 5.4) |

---

## 12. Open Questions

1. **`/proc/self/exe`**: musl's `dlopen` reads this. Do we need a minimal `/proc`
   for Phase 1, or only for `dlopen` in Phase 2?

2. **Environment variables**: Should the kernel pass `LD_LIBRARY_PATH` from the host
   via envp? Currently envp is empty.

3. **`ET_EXEC` with `PT_INTERP`**: Some compilers produce non-PIE dynamically-linked
   binaries (ET_EXEC + PT_INTERP). These have absolute addresses for the main binary
   but still need an interpreter. Support in Phase 1?
   **Recommendation: Yes.** It's trivial -- `base = 0` for ET_EXEC, rest of the
   interpreter loading logic is the same.

4. **`NEEDED` library count limit**: With 256 VMA slots, each library needing ~1-2
   VMAs (reservation + per-segment), we support ~80-120 libraries. Enough?
   **Probably yes** for typical workloads. Can increase `MAX_VMAS` if needed.

5. **`mmap` with `PROT_NONE`**: musl does NOT use `PROT_NONE` for the reservation
   (it maps the file directly). But other linkers might. Should we optimize
   `PROT_NONE` to skip page allocation?
   **Not for Phase 1.** Allocating eagerly is correct; optimize later if memory
   pressure becomes an issue.
