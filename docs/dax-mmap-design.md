# DAX Window and File mmap — Design Document

## 1. Background

sumi currently supports file I/O only through FUSE_READ/FUSE_WRITE syscalls. Each read
causes a synchronous VM exit, data copy from host to guest, and return. This works but
blocks two critical capabilities:

1. **Dynamic linking** — `ld.so` loads shared libraries via `mmap(fd, MAP_PRIVATE)`.
   Without file mmap, only statically-linked ELF binaries can run.
2. **Memory-mapped I/O** — applications that use `mmap` for efficient file access
   (databases, log parsers, multimedia decoders) cannot function.

The current `sys_mmap` returns `ENOSYS` for any non-`MAP_ANONYMOUS` call.

### Goals

- File-backed `mmap` with `MAP_PRIVATE` and `MAP_SHARED` semantics.
- Zero-copy read access to host files via a DAX (Direct Access) shared memory window.
- Enable dynamic linking (loading `.so` files via `mmap`).
- Minimal kernel complexity — reuse the synchronous MMIO model.

### Non-goals

- Demand paging / lazy fault-in (all pages mapped eagerly on `mmap`).
- 4KB page granularity (remains 2MB huge pages).
- Copy-on-write (MAP_PRIVATE writable mappings get a full copy upfront).
- `mremap` support.
- Page cache / shared mapping deduplication across multiple `mmap` calls.

---

## 2. Architecture Overview

```
  Guest (sumi-kernel)                         Host (sumi-vm)
 +-------------------------------+           +-------------------------------+
 | User binary                   |           |                               |
 |   mmap(fd, off, len, PRIVATE) |           |                               |
 +-----------+-------------------+           |                               |
             | syscall                       |                               |
 +-----------v-------------------+           |                               |
 | sys_mmap (memory.rs)          |           |                               |
 |   file-backed → DAX path      |           |                               |
 +-----------+-------------------+           |                               |
             |                               |                               |
 +-----------v-------------------+           |                               |
 | DAX Slot Allocator            |           |                               |
 |   allocate N 2MB slots        |           |                               |
 +-----------+-------------------+           |                               |
             |                               |                               |
 +-----------v-------------------+  MMIO     +-------------+-----------------+
 | VirtioFs: FUSE_SETUPMAPPING   +---------->| FUSE Server                   |
 |   (fh, file_off, len,        |           |   mmap(host_fd, MAP_FIXED)    |
 |    dax_off, flags)           |           |   into DAX backing memory     |
 +-----+-------------------------+           +-------------+-----------------+
       |                          MMIO return              |
       |<--------------------------------------------------+
 +-----v-------------------------+
 | Map DAX phys pages into       |           +-------------------------------+
 | user virtual address space    |           | DAX Window (KVM memslot 1)    |
 | via KERNEL_PAGE_TABLE         |           |  host mmap'd region, 512 MB   |
 +-------------------------------+           |  guest sees as physical memory |
                                             +-------------------------------+
 +-------------------------------+
 | User binary                   |
 |   *ptr → direct memory access |
 |   (no VM exit, no FUSE)       |
 +-------------------------------+
```

### Data Flow: Read-Only MAP_PRIVATE (e.g., loading .so .text)

1. `sys_mmap(fd, offset, len, MAP_PRIVATE, PROT_READ|PROT_EXEC)`.
2. Kernel allocates DAX slots covering `align_up_2mb(len)`.
3. Kernel sends `FUSE_SETUPMAPPING(fh, offset, len, dax_offset, FUSE_SETUPMAPPING_FLAG_READ)`.
4. Host `mmap`s the file region at the corresponding offset in the DAX backing memory.
5. Kernel maps DAX physical pages into user virtual address space.
6. User reads file content directly — no VM exits.

### Data Flow: Writable MAP_PRIVATE (e.g., loading .so .data)

1. `sys_mmap(fd, offset, len, MAP_PRIVATE, PROT_READ|PROT_WRITE)`.
2. Kernel allocates anonymous physical pages (not DAX slots).
3. Kernel reads file content into those pages via `FUSE_READ` (chunked).
4. Kernel maps the anonymous pages into user virtual address space.
5. User reads and writes freely — changes stay in guest memory, never written back.

### Data Flow: MAP_SHARED

1. `sys_mmap(fd, offset, len, MAP_SHARED, PROT_READ|PROT_WRITE)`.
2. Same as read-only MAP_PRIVATE: allocate DAX slots, `FUSE_SETUPMAPPING` with
   `FLAG_READ | FLAG_WRITE`.
3. User writes go directly to the DAX window → visible on host immediately.
4. `munmap` sends `FUSE_REMOVEMAPPING` to release DAX slots.

---

## 3. DAX Window

A contiguous physical memory region shared between host and guest. The host can
`mmap` files into it; the guest can read/write it without VM exits.

### 3.1 Physical Layout

```
Guest Physical Address Space:

0x0000_0000_0000_0000  ┌──────────────────────────┐
                       │ Kernel + RAM              │  KVM memslot 0
                       │ (up to ~128 TB)           │
0x0000_0010_0000_0000  ├──────────────────────────┤
                       │ VirtIO MMIO registers     │  No memslot (MMIO exits)
                       │ 4KB per device             │
0x0000_0010_0001_0000  ├──────────────────────────┤
                       │ (gap)                     │
0x0000_0020_0000_0000  ├──────────────────────────┤
                       │ DAX Window (128 GB)       │  KVM memslot 1
                       │ 65536 × 2MB slots         │
0x0000_0040_0000_0000  └──────────────────────────┘
```

Placed at **128 GB** (`0x20_0000_0000`), above the VirtIO MMIO region (64 GB), below
`MAX_PHYSICAL_ADDR` (128 TB). Within the direct-map range so the kernel can access it
at `DIRECT_MAP_OFFSET + DAX_WINDOW_BASE`.

### 3.2 Constants

```rust
// sumi-abi/src/arch/x86_64/layout.rs

/// Base physical address of the DAX shared memory window.
pub const DAX_WINDOW_BASE: PhysicalAddr = PhysicalAddr::new(0x20_0000_0000);   // 128 GB

/// Total DAX window size (128 GB = 65536 × 2MB slots).
pub const DAX_WINDOW_SIZE: usize = 128 * 1024 * 1024 * 1024;

/// Number of 2MB slots in the DAX window.
pub const DAX_SLOT_COUNT: usize = DAX_WINDOW_SIZE / PAGE_SIZE;  // 65536
```

128 GB is generous enough for large workloads with many shared libraries and
memory-mapped files. The host allocates this region lazily (anonymous mmap),
so physical memory is only consumed when the guest actually maps file pages.

### 3.3 Slot Allocator

A bitmap-based allocator manages DAX window slots. Identical pattern to `PageAllocator`
but operates on DAX offsets instead of physical addresses.

```rust
// sumi-kernel/src/fs/dax.rs

const BITMAP_U64S: usize = (DAX_SLOT_COUNT + 63) / 64;  // 1024

pub struct DaxAllocator {
    bitmap: [u64; BITMAP_U64S],  // 1 = allocated, 0 = free
}

impl DaxAllocator {
    /// Allocate `count` contiguous 2MB slots. Returns offset from DAX_WINDOW_BASE.
    pub fn alloc(&mut self, count: usize) -> Result<usize, DaxError>;

    /// Free `count` slots starting at `offset`.
    pub fn free(&mut self, offset: usize, count: usize);
}
```

Global instance:

```rust
pub static DAX_ALLOCATOR: spin::Mutex<DaxAllocator> = spin::Mutex::new(DaxAllocator::new());
```

### 3.4 Host Setup (KVM memslot)

The VM allocates the DAX backing memory as an anonymous mmap region and registers it
as KVM memslot 1:

```rust
// sumi-vm/src/arch/x86_64/kvm/mod.rs  (in setup_memory)

let dax_host_ptr = unsafe {
    libc::mmap(
        std::ptr::null_mut(),
        DAX_WINDOW_SIZE,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
        -1, 0,
    )
};

unsafe {
    vm_fd.set_user_memory_region(kvm_userspace_memory_region {
        slot: 1,
        guest_phys_addr: DAX_WINDOW_BASE.as_u64(),
        memory_size: DAX_WINDOW_SIZE as u64,
        userspace_addr: dax_host_ptr as u64,
        flags: 0,
    })?;
}
```

When the FUSE server handles `FUSE_SETUPMAPPING`, it uses `mmap(MAP_FIXED)` over
portions of this region to map host files:

```rust
// sumi-vm/src/devices/virtio_fs.rs

fn handle_setupmapping(&mut self, fh: u64, file_offset: u64,
                       len: u64, dax_offset: u64, flags: u64) {
    let host_fd = self.file_handles[fh as usize].as_ref().unwrap();
    let prot = if flags & FUSE_SETUPMAPPING_FLAG_WRITE != 0 {
        libc::PROT_READ | libc::PROT_WRITE
    } else {
        libc::PROT_READ
    };

    unsafe {
        libc::mmap(
            self.dax_host_ptr.add(dax_offset as usize),
            len as usize,
            prot,
            libc::MAP_SHARED | libc::MAP_FIXED,
            host_fd.as_raw_fd(),
            file_offset as i64,
        );
    }
}
```

When `FUSE_REMOVEMAPPING` arrives, the host replaces the file mapping with anonymous
memory (effectively zeroing it):

```rust
fn handle_removemapping(&mut self, dax_offset: u64, len: u64) {
    unsafe {
        libc::mmap(
            self.dax_host_ptr.add(dax_offset as usize),
            len as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
            -1, 0,
        );
    }
}
```

---

## 4. FUSE Protocol Extensions

### 4.1 Feature Negotiation

During `FUSE_INIT`, the kernel announces DAX support:

```
FUSE_MAP_ALIGNMENT  (bit 26)  — guest supports SETUPMAPPING/REMOVEMAPPING
```

The init response includes `map_alignment = 21` (2^21 = 2 MB), telling the server
that all DAX mapping offsets and lengths must be 2MB-aligned.

### 4.2 New Operations

| Opcode | Name                  | Descriptor Chain                         |
|--------|-----------------------|------------------------------------------|
| 48     | `FUSE_SETUPMAPPING`   | [hdr + SetupMappingIn] → [out_hdr]      |
| 49     | `FUSE_REMOVEMAPPING`  | [hdr + RemoveMappingIn + RemoveMappingOne] → [out_hdr] |

### 4.3 Structures

```rust
// sumi-abi/src/fuse.rs

pub const FUSE_SETUPMAPPING: u32 = 48;
pub const FUSE_REMOVEMAPPING: u32 = 49;

pub const FUSE_SETUPMAPPING_FLAG_READ:  u64 = 1;
pub const FUSE_SETUPMAPPING_FLAG_WRITE: u64 = 2;

#[repr(C)]
pub struct FuseSetupMappingIn {
    pub fh:          u64,   // file handle
    pub foffset:     u64,   // offset in file
    pub len:         u64,   // length to map
    pub flags:       u64,   // FUSE_SETUPMAPPING_FLAG_{READ,WRITE}
    pub moffset:     u64,   // offset in DAX window
}

#[repr(C)]
pub struct FuseRemoveMappingIn {
    pub count: u32,         // number of FuseRemoveMappingOne entries
}

#[repr(C)]
pub struct FuseRemoveMappingOne {
    pub moffset: u64,       // offset in DAX window
    pub len:     u64,       // length to unmap
}
```

### 4.4 VirtioFsClient Extensions

```rust
// sumi-kernel/src/fs/virtio_fs.rs

impl VirtioFsClient {
    pub fn setup_mapping(&self, fh: u64, file_offset: u64, len: u64,
                         dax_offset: u64, flags: u64) -> Result<(), i64>;
    pub fn remove_mapping(&self, dax_offset: u64, len: u64) -> Result<(), i64>;
}
```

Both use the same synchronous MMIO pattern as existing FUSE operations: build
descriptor chain, write `QueueNotify`, host processes and returns.

---

## 5. Virtual Memory Areas (VMAs)

The kernel needs to track which virtual address ranges are file-backed to handle
`munmap`, `msync`, and future `mremap`. Introduce a minimal VMA structure.

### 5.1 Design

```rust
// sumi-kernel/src/memory/vma.rs

pub enum MappingBacking {
    /// Anonymous pages owned by the guest.
    Anonymous,
    /// DAX window mapping — pages are in the DAX physical region.
    Dax {
        dax_offset: usize,     // offset into DAX window
        fuse_fh: u64,          // for future msync/writeback
        fuse_nodeid: u64,      // for cleanup on munmap
        file_offset: u64,      // offset in file
    },
    /// Private file copy — anonymous pages populated from file content.
    PrivateFile {
        fuse_fh: u64,
        fuse_nodeid: u64,
    },
}

pub struct Vma {
    pub start: VirtualAddr,     // page-aligned
    pub end: VirtualAddr,       // exclusive, page-aligned
    pub backing: MappingBacking,
}
```

### 5.2 VMA Table

Fixed-size array. `mmap` regions are bounded; 256 VMAs is sufficient for typical
workloads (shared library count + application mappings).

```rust
pub const MAX_VMAS: usize = 256;

pub struct VmaTable {
    vmas: [Option<Vma>; MAX_VMAS],
}

impl VmaTable {
    pub fn insert(&mut self, vma: Vma) -> Result<usize, VmaError>;
    pub fn remove(&mut self, start: VirtualAddr) -> Option<Vma>;
    pub fn find(&self, addr: VirtualAddr) -> Option<&Vma>;
}
```

Global instance:

```rust
pub static VMA_TABLE: spin::Mutex<VmaTable> = spin::Mutex::new(VmaTable::new());
```

---

## 6. Updated sys_mmap

### 6.1 Dispatch Logic

```
sys_mmap(addr, len, prot, flags, fd, offset)
  │
  ├─ flags & MAP_ANONYMOUS ──→ existing anonymous path (unchanged)
  │
  └─ file-backed ─┬─ MAP_PRIVATE + PROT_WRITE ──→ private file copy path
                   │
                   ├─ MAP_PRIVATE (read-only) ───→ DAX read-only path
                   │
                   └─ MAP_SHARED ────────────────→ DAX shared path
```

### 6.2 DAX Path (MAP_PRIVATE read-only, MAP_SHARED)

```
1. Look up fd → (fuse_fh, fuse_nodeid)
2. aligned_len = align_up_2mb(len + (offset % PAGE_SIZE))
3. pages = aligned_len / PAGE_SIZE
4. dax_offset = DAX_ALLOCATOR.alloc(pages)            // reserve DAX slots
5. FUSE_SETUPMAPPING(fh, align_down_2mb(offset),      // tell host to map file
       aligned_len, dax_offset, flags)
6. vaddr = allocate user virtual range (from MMAP_NEXT)
7. for i in 0..pages:
       paddr = DAX_WINDOW_BASE + dax_offset + i * PAGE_SIZE
       KERNEL_PAGE_TABLE.map_2mb(vaddr + i * PAGE_SIZE, paddr)
8. VMA_TABLE.insert(Vma { start: vaddr, end: vaddr + aligned_len,
       backing: Dax { dax_offset, fh, nodeid, file_offset } })
9. return vaddr + (offset % PAGE_SIZE)   // sub-page offset for unaligned requests
```

### 6.3 Private File Copy Path (MAP_PRIVATE + PROT_WRITE)

```
1. Look up fd → (fuse_fh, fuse_nodeid, ...)
2. aligned_len = align_up_2mb(len)
3. pages = aligned_len / PAGE_SIZE
4. vaddr = allocate user virtual range
5. for i in 0..pages:
       paddr = PAGE_ALLOCATOR.alloc(1)
       zero_page(paddr)
       KERNEL_PAGE_TABLE.map_2mb(vaddr + i * PAGE_SIZE, paddr)
6. Read file content via FUSE_READ into mapped pages
       (reuse fs_transfer_chunked with physical addresses)
7. VMA_TABLE.insert(Vma { start, end, backing: PrivateFile { fh, nodeid } })
8. return vaddr + (offset % PAGE_SIZE)
```

### 6.4 Updated sys_munmap

```
sys_munmap(addr, len):
  1. aligned range = [align_down_2mb(addr), align_up_2mb(addr + len))
  2. Find VMA covering this range
  3. Match backing:
       Anonymous → unmap pages, free physical memory (existing code)
       Dax → unmap pages, FUSE_REMOVEMAPPING, DAX_ALLOCATOR.free()
       PrivateFile → unmap pages, free physical memory
  4. VMA_TABLE.remove(vma)
```

### 6.5 MAP_FIXED Support

`MAP_FIXED` requests a specific virtual address. The kernel must:
1. `munmap` any existing mapping that overlaps the requested range.
2. Proceed with the new mapping at the fixed address.

This is required for dynamic linking — `ld.so` uses `MAP_FIXED` to place library
segments at computed addresses.

---

## 7. 2MB Alignment Implications

All page table operations use 2MB huge pages. This has consequences for file mappings:

### 7.1 Internal Fragmentation

A 4KB file mapped via DAX consumes a 2MB DAX slot. Worst case: 256 small file
mappings exhaust the entire 512MB DAX window. This is acceptable because:
- Shared libraries are typically multi-MB.
- Small files are better served by `read()` into heap buffers.
- The window size is configurable.

### 7.2 Sub-page File Offsets

When `mmap(fd, 0x1000, 0x2000, ...)` requests mapping at file offset 0x1000:
- The kernel rounds down to 2MB: maps from file offset 0.
- Returns `vaddr + 0x1000` so the caller sees the correct data.
- The surrounding 2MB is accessible but contains other file data (or zeros past EOF).

This matches Linux behavior with huge pages — no security concern in a unikernel
where the application already has full address space access.

### 7.3 Partial Page at EOF

If the file is smaller than the 2MB slot, bytes past EOF are zero (the host `mmap`
zero-fills past EOF per POSIX). Writes past EOF through a `MAP_SHARED` mapping are
silently lost (no file extension via mmap).

---

## 8. Dynamic Linking Support

The primary motivator for file mmap. Here's the expected call sequence when
running a dynamically-linked ELF binary:

### 8.1 ELF Loader Changes

Currently, `exec.rs` loads only `ET_EXEC` binaries. To support dynamic linking:

1. Accept `ET_DYN` (PIE) binaries — load at a chosen base address.
2. Detect `PT_INTERP` segment → extract interpreter path (e.g., `/lib/ld-musl-x86_64.so.1`).
3. Load the interpreter ELF from virtio-fs (same as main binary).
4. Set entry point to the interpreter's `e_entry`, not the main binary's.
5. Pass main binary info via auxiliary vector: `AT_BASE` (interpreter base),
   `AT_PHDR`, `AT_PHNUM`, `AT_ENTRY` (main binary's entry).

### 8.2 ld.so mmap Sequence

The dynamic linker then:

```
open("/lib/libc.so")                          → fd
fstat(fd)                                     → get file size
mmap(NULL, size, PROT_READ, MAP_PRIVATE, fd, 0) → map ELF header + phdrs
  // parse PT_LOAD segments
mmap(base, text_sz, PROT_READ|PROT_EXEC, MAP_PRIVATE|MAP_FIXED, fd, text_off)
  → DAX path (read-only)
mmap(base+gap, data_sz, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_FIXED, fd, data_off)
  → private copy path (writable)
close(fd)
```

### 8.3 Required Syscalls

Beyond `mmap`, dynamic linking may need:

| Syscall | Status | Notes |
|---------|--------|-------|
| `mmap` (file-backed) | **New** | This design |
| `mprotect` | Exists (no-op) | OK — 2MB pages are all RWX |
| `munmap` | Exists | Update for DAX cleanup |
| `mremap` | Stub (ENOSYS) | Not needed by musl ld.so |
| `openat` | Exists | Used to open .so files |
| `fstat`/`newfstatat` | Exists | Used to get file sizes |
| `pread64` | Exists | Some loaders use pread instead of mmap |
| `set_tid_address` | Stub needed | musl calls at startup |
| `arch_prctl(SET_FS)` | Stub needed | TLS setup |

---

## 9. Module Layout (New and Changed Files)

### New Files

```
sumi-kernel/src/
├── fs/
│   └── dax.rs               DaxAllocator: bitmap slot allocator
├── memory/
│   └── vma.rs               VmaTable, Vma, MappingBacking
```

### Changed Files

```
sumi-abi/src/
├── fuse.rs                  + FUSE_SETUPMAPPING/REMOVEMAPPING types, opcodes, flags
├── arch/x86_64/layout.rs    + DAX_WINDOW_BASE, DAX_WINDOW_SIZE, DAX_SLOT_COUNT

sumi-kernel/src/
├── fs/virtio_fs.rs          + setup_mapping(), remove_mapping() FUSE operations
├── syscall/handlers/memory.rs  Rewrite sys_mmap: file-backed dispatch, DAX/copy paths
│                               Update sys_munmap: VMA-aware cleanup
├── lib.rs                   + DAX_ALLOCATOR, VMA_TABLE globals

sumi-vm/src/
├── arch/x86_64/kvm/mod.rs   + DAX window host allocation, KVM memslot 1
├── devices/virtio_fs.rs      + handle_setupmapping(), handle_removemapping()
│                             + FUSE_INIT: announce MAP_ALIGNMENT feature
│                             + Store dax_host_ptr for mmap(MAP_FIXED) operations
```

---

## 10. Implementation Phases

### Phase 1: Private File Copy (enables dynamic linking)

Simplest path that unblocks `.so` loading. No DAX, no shared memory window.

- `sys_mmap(file_fd)` → allocate anonymous pages, `FUSE_READ` file content into them.
- Update `sys_munmap` to free anonymous pages for file mappings.
- Add VMA tracking (needed for `munmap` to know which pages to free).
- ELF loader: support `ET_DYN` + `PT_INTERP`.

**Deliverable**: dynamically-linked musl binary runs under sumi.

### Phase 2: DAX Window

Zero-copy read access. The real performance win.

- Host: allocate DAX backing memory, register KVM memslot 1.
- ABI: add FUSE_SETUPMAPPING/REMOVEMAPPING types.
- Kernel: `DaxAllocator`, wire DAX path into `sys_mmap`.
- VM: handle `FUSE_SETUPMAPPING` → `mmap(MAP_FIXED)`, `FUSE_REMOVEMAPPING`.
- `MAP_PRIVATE` read-only → DAX, `MAP_PRIVATE` writable → still private copy.
- `MAP_SHARED` → DAX with write flag.

**Deliverable**: `.so` .text segments loaded without data copy. `MAP_SHARED` works.

### Phase 3: Demand Paging (future)

Replace eager mapping with fault-driven population.

- Implement `#PF` exception handler (IDT entry 14).
- On page fault in VMA range: `FUSE_SETUPMAPPING` + `map_2mb` on demand.
- `mmap` only creates VMA metadata, no physical pages allocated.
- Requires KVM `KVM_SET_GUEST_DEBUG` or exception bitmap configuration to
  intercept `#PF` inside the guest (currently no IDT is set up).

**Deliverable**: large files can be mmap'd without upfront memory cost.

---

## 11. Safety Considerations

### 11.1 DAX and MAP_PRIVATE Read-Only Trust Model

With 2MB pages, all page table entries are `RWX`. The kernel cannot enforce read-only
access at the hardware level. A MAP_PRIVATE mapping served via DAX (Section 6.2) relies
on the application not writing to pages it declared read-only. If it does write:

- `MAP_PRIVATE` read-only via DAX: write corrupts the host file. **This is UB in the
  application** — POSIX says writing to a `PROT_READ`-only mapping is undefined.
- Acceptable for a unikernel that trusts its single user application.

If this is too risky for a specific workload, the kernel can be configured to always
use the private copy path (Phase 1), at the cost of memory and startup time.

### 11.2 DAX Window Exhaustion

If all 65536 DAX slots are consumed, `sys_mmap` falls back to the private file copy path
automatically. This is a graceful degradation, not an error.

### 11.3 File Truncation

If the host file is truncated while DAX-mapped, guest reads past the new EOF produce
`SIGBUS` on Linux. In sumi, this would be a KVM memfault or unexpected data. The
unikernel does not handle this — same as running with `MAP_SHARED` on a truncated file
in any single-process environment.

### 11.4 munmap Correctness

`sys_munmap` must always:
1. Unmap page table entries (prevent stale TLB access).
2. `FUSE_REMOVEMAPPING` for DAX slots (prevent host file handle leaks).
3. Free DAX slots from `DaxAllocator`.
4. Remove VMA from `VMA_TABLE`.

Failure in any step leaks resources. Steps are ordered so that the most critical
(page table unmap) happens first.

---

## 12. Testing Strategy

### Unit Tests (cargo test)

| Test | Verifies |
|------|----------|
| `dax_alloc_free` | DaxAllocator alloc/free, bitmap state |
| `dax_alloc_contiguous` | Multi-slot contiguous allocation |
| `dax_alloc_exhaustion` | Returns error when window is full |
| `dax_double_free` | Panic or error on double-free |
| `vma_insert_find_remove` | VmaTable basic CRUD |
| `vma_overlapping_reject` | Overlapping insert fails |
| `vma_find_by_addr` | Lookup returns correct VMA |
| `vma_table_full` | Returns error when 256 VMAs used |

### KVM Integration Tests (make self-test)

| Test | Verifies |
|------|----------|
| `mmap_file_private_read` | mmap(MAP_PRIVATE, PROT_READ) returns file content |
| `mmap_file_private_write` | mmap(MAP_PRIVATE, PROT_WRITE) returns file content, writes don't persist |
| `mmap_file_shared_write` | mmap(MAP_SHARED) write visible after FUSE_READ |
| `mmap_fixed` | MAP_FIXED places mapping at requested address |
| `mmap_munmap_reuse` | munmap frees DAX slots, re-allocation succeeds |
| `mmap_beyond_eof` | Bytes past EOF read as zero |

---

## 13. Open Questions

1. **DAX window size**: 128 GB (65536 slots) is generous. Should this be
   configurable at runtime via a VM command-line flag?

2. **msync semantics**: `MAP_SHARED` writes are immediately visible to the host
   (write-through via DAX). Should `sys_msync` be a no-op or issue an `mfence`?

3. **Multiple mappings of the same file region**: Should the kernel deduplicate DAX
   slots when the same file offset is mapped multiple times? Initial answer: no —
   keep it simple, each `mmap` gets its own DAX slots.

4. **TLS and thread-local storage**: Dynamic linking with musl requires
   `arch_prctl(ARCH_SET_FS)` to set the FS base for TLS. This needs a `wrmsr` to
   `IA32_FS_BASE`. Should this be part of this design or a separate task?

5. **PIE base address selection**: When loading `ET_DYN` binaries, what virtual base
   address should the kernel choose? Linux uses ASLR; sumi can use a fixed address
   (e.g., `0x4000_0000`).
