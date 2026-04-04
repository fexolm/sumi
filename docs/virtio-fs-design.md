# Host Filesystem via virtio-fs — Design Document

## 1. Background

sumi runs Linux ELF binaries that expect a POSIX filesystem. Since the unikernel has no
block device or local filesystem, all file I/O must be forwarded to the host. We use virtio
as the transport and a subset of the FUSE protocol for filesystem operations.

**Key insight**: Because sumi is a single-process unikernel under KVM, every MMIO write
causes a synchronous VM exit. The VM process handles the request and returns — from the
kernel's perspective, the MMIO write blocks until the response is ready. This eliminates
the need for interrupt injection, async completion, or polling. Each filesystem operation
is a single MMIO round-trip.

### Goals

- Read and write host files from guest Linux binaries.
- Zero-copy where possible: virtqueue descriptors point directly to the caller's buffer.
- Minimal complexity: no interrupt controller, no async I/O, no FUSE daemon.
- Reusable transport: the virtio MMIO layer supports future devices (net, console).

### Non-goals (for now)

- DAX / shared memory mapping (Phase 2).
- mmap of files (requires DAX or page-fault forwarding).
- File locking (flock/fcntl).
- Extended attributes, ACLs, inotify.

---

## 2. Architecture Overview

```
  Guest (sumi-kernel)                      Host (sumi-vm)
 +-----------------------+               +-------------------------+
 | Linux binary          |               |                         |
 |   read(fd, buf, n)    |               |                         |
 +---------+-------------+               |                         |
           | syscall                      |                         |
 +---------v-------------+               |                         |
 | Syscall Dispatch      |               |                         |
 |   sys_read()          |               |                         |
 +---------+-------------+               |                         |
           |                              |                         |
 +---------v-------------+               |                         |
 | FD Table              |               |                         |
 |   fd -> {fh, nodeid,  |               |                         |
 |          offset, type} |               |                         |
 +---------+-------------+               |                         |
           |                              |                         |
 +---------v-------------+               |                         |
 | VirtioFs Client       |               |                         |
 |   FUSE_READ request   |               |                         |
 +---------+-------------+               |                         |
           |                              |                         |
 +---------v-------------+    MMIO exit   +----------+--------------+
 | Virtqueue             +--------------->| VirtIO MMIO Backend     |
 |   submit descriptor   |               |   read virtqueue        |
 |   write QueueNotify   |               +----------+--------------+
 +---------+-------------+               |          |               |
           |                              | +--------v------------+ |
           |                              | | FUSE Server         | |
           |                              | |   nodeid -> path    | |
           |                              | |   fh -> host fd     | |
           |                              | +--------+------------+ |
           |                              |          |               |
           |                              | +--------v------------+ |
           |                              | | Host Syscalls       | |
           |                              | |   pread(host_fd,    | |
           |                              | |     guest_buf, n)   | |
           |                              | +--------+------------+ |
           |               MMIO return    |          |               |
           |<-----------------------------+----------+               |
 +---------v-------------+               +-------------------------+
 | sys_read returns      |
 |   bytes_read to RAX   |
 +-----------------------+
```

---

## 3. VirtIO MMIO Transport

We use the VirtIO MMIO transport (virtio spec v1.2, section 4.2) instead of PCI because
sumi has no PCI bus. Each virtio device occupies a 4KB MMIO register region at a fixed
physical address known at compile time.

### 3.1 MMIO Register Layout

All registers are 32-bit unless noted. Offset from device base address:

| Offset | Name              | R/W | Description                          |
|--------|-------------------|-----|--------------------------------------|
| 0x000  | MagicValue        | R   | Must be `0x74726976` ("virt")        |
| 0x004  | Version           | R   | `2` (virtio v1.0+)                   |
| 0x008  | DeviceID          | R   | `26` = filesystem device             |
| 0x00C  | VendorID          | R   | `0x554D4953` ("SUMI")                |
| 0x010  | DeviceFeatures    | R   | Features bits (selected by selector) |
| 0x014  | DeviceFeaturesSel | W   | Feature word selector (0 or 1)       |
| 0x020  | DriverFeatures    | W   | Acknowledged features                |
| 0x024  | DriverFeaturesSel | W   | Feature word selector                |
| 0x030  | QueueSel          | W   | Queue index selector                 |
| 0x034  | QueueNumMax       | R   | Max descriptors for selected queue   |
| 0x038  | QueueNum          | W   | Queue size (must be power of 2)      |
| 0x044  | QueueReady        | RW  | Queue is ready                       |
| 0x050  | QueueNotify       | W   | **The kick register** — VM exit      |
| 0x060  | InterruptStatus   | R   | (unused — we poll)                   |
| 0x064  | InterruptACK      | W   | (unused)                             |
| 0x070  | Status            | RW  | Device status bits                   |
| 0x080  | QueueDescLow      | W   | Descriptor table phys addr [31:0]    |
| 0x084  | QueueDescHigh     | W   | Descriptor table phys addr [63:32]   |
| 0x090  | QueueAvailLow     | W   | Available ring phys addr [31:0]      |
| 0x094  | QueueAvailHigh    | W   | Available ring phys addr [63:32]     |
| 0x0A0  | QueueUsedLow      | W   | Used ring phys addr [31:0]           |
| 0x0A4  | QueueUsedHigh     | W   | Used ring phys addr [63:32]          |
| 0x100+ | Config space      | RW  | Device-specific (virtio-fs: tag)     |

### 3.2 Device Initialization Sequence (kernel-side)

```
1. Read MagicValue, verify == 0x74726976
2. Read Version, verify == 2
3. Read DeviceID, verify == 26 (filesystem)
4. Write Status = 0 (reset)
5. Write Status |= ACKNOWLEDGE (1)
6. Write Status |= DRIVER (2)
7. Read/negotiate features (DeviceFeatures/DriverFeatures)
8. Write Status |= FEATURES_OK (8)
9. Read Status, verify FEATURES_OK still set
10. Set up virtqueues (see section 4)
11. Write Status |= DRIVER_OK (4)
```

### 3.3 MMIO Address

```rust
// sumi-abi/src/arch/x86_64/layout.rs

/// Base physical address for virtio MMIO devices.
/// Placed at 127 TB — well above any reasonable guest RAM size,
/// but within the 128 TB direct-map range so page tables cover it.
pub const VIRTIO_MMIO_BASE: PhysicalAddr = PhysicalAddr::new(0x7F00_0000_0000);

/// Each virtio device occupies 4KB (one MMIO page).
pub const VIRTIO_MMIO_STRIDE: usize = 0x1000;

/// Device 0 = virtio-fs.
pub const VIRTIO_FS_MMIO: PhysicalAddr = VIRTIO_MMIO_BASE;
```

The kernel accesses these through the direct map (128 TB, 1 GB huge pages):
`VirtualAddr = DIRECT_MAP_OFFSET + VIRTIO_FS_MMIO`.

The VM process does NOT register a KVM memory region for this range. Any guest access
triggers `KVM_EXIT_MMIO`, which the VM handles.

---

## 4. Split Virtqueue

We use the split virtqueue format (virtio spec section 2.7). It consists of three
physically-contiguous regions, each allocated by the kernel from `PageAllocator`.

### 4.1 Descriptor Table

Array of `QueueNum` entries:

```rust
#[repr(C)]
pub struct VirtqDesc {
    pub addr:  u64,    // Physical address of the buffer
    pub len:   u32,    // Buffer length in bytes
    pub flags: u16,    // NEXT (1), WRITE (2), INDIRECT (4)
    pub next:  u16,    // Next descriptor index (if NEXT flag set)
}
```

Size: `QueueNum * 16` bytes.

Descriptors form chains: a FUSE request typically uses 2-3 descriptors chained via `next`:
- Descriptor 0: FUSE request header (device-readable)
- Descriptor 1: Response header + data buffer (device-writable, `WRITE` flag)

For writes, an extra descriptor carries the write data (device-readable) before the
response descriptor.

### 4.2 Available Ring

The kernel publishes descriptor chain heads here for the VM to consume:

```rust
#[repr(C)]
pub struct VirtqAvail {
    pub flags: u16,          // VIRTQ_AVAIL_F_NO_INTERRUPT = 1
    pub idx:   u16,          // Next entry the kernel will write
    pub ring:  [u16; QUEUE_SIZE],  // Descriptor chain head indices
}
```

Size: `4 + 2 * QueueNum` bytes (+ 2 bytes padding for used_event, ignored).

### 4.3 Used Ring

The VM writes completed descriptor chain heads here:

```rust
#[repr(C)]
pub struct VirtqUsed {
    pub flags: u16,
    pub idx:   u16,          // Next entry the VM will write
    pub ring:  [VirtqUsedElem; QUEUE_SIZE],
}

#[repr(C)]
pub struct VirtqUsedElem {
    pub id:  u32,   // Descriptor chain head index
    pub len: u32,   // Total bytes written by the device
}
```

Size: `4 + 8 * QueueNum` bytes.

### 4.4 Queue Size

`QUEUE_SIZE = 256` — sized for up to 256 concurrent vCPUs. Each vCPU may have one
FUSE request in flight, and each request uses 2-3 descriptors chained together.
256 descriptors allow ~85 concurrent requests (at 3 descriptors each).

### 4.5 Queue Count

virtio-fs defines two queues:
- **Queue 0 (hiprio)**: For `FUSE_FORGET` / `FUSE_INTERRUPT`. Initially unused — we
  handle forget inline.
- **Queue 1 (request)**: All FUSE requests go here.

We allocate both but only use queue 1 initially.

### 4.6 Synchronous Completion Model

With up to 256 vCPUs, multiple FUSE requests can be in flight simultaneously. Each
vCPU independently submits descriptors and triggers KVM exits.

```
 vCPU A (kernel)     vCPU B (kernel)        VM process (host threads)
   |                    |                      |
   | lock queue         |                      |
   | submit desc        |                      |
   | unlock queue       |                      |
   | QueueNotify --KVM_EXIT-->  thread A:      |
   | (A paused)         |      read avail      |
   |                    | lock queue            |
   |                    | submit desc           |
   |                    | unlock queue          |
   |                    | QueueNotify --EXIT--> | thread B:
   |                    | (B paused)            | read avail
   |                    |          thread A:    | process B's FUSE req
   |                    |          process A's  | host syscall
   |                    |          FUSE request | write used ring
   |                    |          host syscall |
   |                    |          write used   |
   | <--KVM_RUN-------- |          ring         |
   | read used ring     | <--KVM_RUN---------- |
   |                    | read used ring        |
   v                    v                       v
```

Each vCPU's `KVM_EXIT_MMIO` is handled by its own host thread. The VM processes the
request and returns — from that vCPU's perspective, the MMIO write blocks until the
response is ready. Different vCPUs can have requests in flight concurrently.

The guest kernel protects the virtqueue with a spinlock. The lock is held only during
descriptor submission (fast — no I/O), not during the MMIO exit. Each vCPU identifies
its response in the used ring by matching the FUSE `unique` field.

The `avail.flags` is set to `VIRTQ_AVAIL_F_NO_INTERRUPT` since we never need
host→guest notifications.

---

## 5. FUSE Protocol

FUSE (Filesystem in Userspace) protocol defines request/response messages carried over
the virtqueue. We implement FUSE 7.31 (Linux 5.x compatible).

### 5.1 Message Format

Every FUSE message starts with a header:

```rust
/// Sent by kernel → VM
#[repr(C)]
pub struct FuseInHeader {
    pub len:     u32,   // Total message length (header + body)
    pub opcode:  u32,   // FUSE operation code
    pub unique:  u64,   // Request ID (echoed in response)
    pub nodeid:  u64,   // Inode number (operation target)
    pub uid:     u32,   // (unused, set to 0)
    pub gid:     u32,   // (unused, set to 0)
    pub pid:     u32,   // (unused, set to 0)
    pub padding: u32,
}
// Size: 40 bytes

/// Sent by VM → kernel
#[repr(C)]
pub struct FuseOutHeader {
    pub len:    u32,    // Total response length (header + body)
    pub error:  i32,    // 0 on success, -errno on error
    pub unique: u64,    // Echoed from request
}
// Size: 16 bytes
```

### 5.2 Required Operations

Minimum set for file I/O:

| Opcode | Name              | Request Body          | Response Body          | Used By                |
|--------|-------------------|-----------------------|------------------------|------------------------|
| 1      | `FUSE_LOOKUP`     | filename (null-term)  | `FuseEntryOut`         | open, stat, access     |
| 2      | `FUSE_FORGET`     | `FuseForgetIn`        | (no response)          | close, path cleanup    |
| 3      | `FUSE_GETATTR`    | `FuseGetattrIn`       | `FuseAttrOut`          | stat, fstat            |
| 14     | `FUSE_OPEN`       | `FuseOpenIn`          | `FuseOpenOut`          | open, openat           |
| 15     | `FUSE_READ`       | `FuseReadIn`          | data bytes             | read, pread64          |
| 16     | `FUSE_WRITE`      | `FuseWriteIn` + data  | `FuseWriteOut`         | write, pwrite64        |
| 18     | `FUSE_RELEASE`    | `FuseReleaseIn`       | (empty)                | close                  |
| 26     | `FUSE_INIT`       | `FuseInitIn`          | `FuseInitOut`          | mount / device init    |
| 28     | `FUSE_OPENDIR`    | `FuseOpenIn`          | `FuseOpenOut`          | getdents               |
| 29     | `FUSE_READDIR`    | `FuseReadIn`          | `FuseDirent` stream    | getdents               |
| 30     | `FUSE_RELEASEDIR` | `FuseReleaseIn`       | (empty)                | close dir              |

Extended set (Phase 2):

| Opcode | Name              | Used By                          |
|--------|-------------------|----------------------------------|
| 4      | `FUSE_SETATTR`    | chmod, chown, truncate, utimes   |
| 6      | `FUSE_SYMLINK`    | symlink                          |
| 9      | `FUSE_LINK`       | link                             |
| 10     | `FUSE_UNLINK`     | unlink, unlinkat                 |
| 11     | `FUSE_RMDIR`      | rmdir                            |
| 12     | `FUSE_RENAME`     | rename                           |
| 14     | `FUSE_MKDIR`      | mkdir                            |
| 22     | `FUSE_READLINK`   | readlink                         |
| 34     | `FUSE_ACCESS`     | access                           |
| 35     | `FUSE_CREATE`     | open(O_CREAT), creat             |
| 44     | `FUSE_LSEEK`      | lseek (SEEK_DATA/SEEK_HOLE)      |

### 5.3 Key FUSE Structures

```rust
#[repr(C)]
pub struct FuseAttr {
    pub ino:       u64,
    pub size:      u64,
    pub blocks:    u64,
    pub atime:     u64,
    pub mtime:     u64,
    pub ctime:     u64,
    pub atimensec: u32,
    pub mtimensec: u32,
    pub ctimensec: u32,
    pub mode:      u32,
    pub nlink:     u32,
    pub uid:       u32,
    pub gid:       u32,
    pub rdev:      u32,
    pub blksize:   u32,
    pub flags:     u32,
}

#[repr(C)]
pub struct FuseEntryOut {
    pub nodeid:         u64,   // Assigned inode for this entry
    pub generation:     u64,
    pub entry_valid:    u64,   // Cache timeout (seconds)
    pub attr_valid:     u64,
    pub entry_valid_nsec: u32,
    pub attr_valid_nsec:  u32,
    pub attr:           FuseAttr,
}

#[repr(C)]
pub struct FuseOpenIn {
    pub flags:   u32,   // O_RDONLY, O_WRONLY, O_RDWR, etc.
    pub open_flags: u32,
}

#[repr(C)]
pub struct FuseOpenOut {
    pub fh:         u64,   // File handle (opaque, assigned by VM)
    pub open_flags: u32,
    pub padding:    u32,
}

#[repr(C)]
pub struct FuseReadIn {
    pub fh:      u64,   // File handle from FUSE_OPEN
    pub offset:  u64,   // Byte offset in file
    pub size:    u32,   // Bytes to read
    pub read_flags: u32,
    pub lock_owner: u64,
    pub flags:   u32,
    pub padding: u32,
}

#[repr(C)]
pub struct FuseWriteIn {
    pub fh:      u64,
    pub offset:  u64,
    pub size:    u32,
    pub write_flags: u32,
    pub lock_owner: u64,
    pub flags:   u32,
    pub padding: u32,
}
// Followed by `size` bytes of write data.

#[repr(C)]
pub struct FuseWriteOut {
    pub size:    u32,   // Bytes actually written
    pub padding: u32,
}

#[repr(C)]
pub struct FuseInitIn {
    pub major:        u32,   // FUSE_KERNEL_VERSION (7)
    pub minor:        u32,   // FUSE_KERNEL_MINOR_VERSION (31)
    pub max_readahead: u32,
    pub flags:        u32,
}

#[repr(C)]
pub struct FuseInitOut {
    pub major:         u32,
    pub minor:         u32,
    pub max_readahead:  u32,
    pub flags:         u32,
    pub max_background: u16,
    pub congestion_threshold: u16,
    pub max_write:     u32,
    // ... additional fields (padded to 64 bytes)
}

#[repr(C)]
pub struct FuseForgetIn {
    pub nlookup: u64,   // Number of lookups to forget
}
```

### 5.4 Session Initialization

After virtio device init, the kernel sends `FUSE_INIT`:

```
Kernel → VM:  FuseInHeader { opcode: FUSE_INIT, nodeid: 0 }
              FuseInitIn { major: 7, minor: 31, max_readahead: 0, flags: 0 }

VM → Kernel:  FuseOutHeader { error: 0 }
              FuseInitOut { major: 7, minor: 31, max_write: 1048576, ... }
```

`max_write` tells the kernel the maximum bytes per `FUSE_WRITE` request. We set this to
1 MB (matching typical virtio-fs implementations). `max_read` is implicitly the same.

### 5.5 Path Resolution

FUSE resolves paths component-by-component from the root node (`nodeid = 1`).

To open `/data/input.txt`:
```
FUSE_LOOKUP(parent=1, name="data")     → nodeid=2, attr={mode=S_IFDIR, ...}
FUSE_LOOKUP(parent=2, name="input.txt") → nodeid=3, attr={mode=S_IFREG, size=4096, ...}
FUSE_OPEN(nodeid=3, flags=O_RDONLY)     → fh=1
```

Each `FUSE_LOOKUP` is one virtqueue round-trip. Since each round-trip is a synchronous
MMIO exit (no context switch, no interrupt — just a function call in the VM process),
per-component resolution is fast (~microseconds per component).

The kernel caches `nodeid` assignments in the FD table entries and issues `FUSE_FORGET`
when all references to a node are released.

---

## 6. File Descriptor Table

### 6.1 Design

```rust
// sumi-kernel/src/fs/fd.rs

pub const MAX_FDS: usize = 256;

#[derive(Clone, Copy)]
pub enum FdKind {
    /// Console: debugcon port for output, no input yet.
    Console,
    /// Host file accessed via virtio-fs FUSE.
    File {
        fuse_fh: u64,       // FUSE file handle from FUSE_OPEN
        fuse_nodeid: u64,   // FUSE node ID for this file
        offset: u64,        // Current file position (updated by read/write/lseek)
    },
    /// Host directory accessed via virtio-fs FUSE.
    Directory {
        fuse_fh: u64,
        fuse_nodeid: u64,
        offset: u64,
    },
}

pub struct FileDescriptor {
    pub kind: FdKind,
    pub flags: u32,       // O_RDONLY, O_WRONLY, O_RDWR, O_APPEND, etc.
}

pub struct FdTable {
    fds: [Option<FileDescriptor>; MAX_FDS],
}
```

### 6.2 Pre-allocated Descriptors

At boot, before any user code runs:

| FD | Kind    | Purpose                              |
|----|---------|--------------------------------------|
| 0  | Console | stdin (returns 0 bytes / EOF for now) |
| 1  | Console | stdout → debugcon port 0xE9          |
| 2  | Console | stderr → debugcon port 0xE9          |

### 6.3 FD Allocation

`open()` / `openat()` scan from index 3 upward for the first `None` slot, matching
Linux's "lowest available fd" guarantee. Returns `-EMFILE` if the table is full.

`dup()` / `dup2()` copy the `FileDescriptor` (including offset and fh) to the target slot.
Both FDs share the same underlying FUSE file handle — `FUSE_RELEASE` is sent only when the
last FD referencing a given `fuse_fh` is closed. A simple reference count on `fuse_fh`
handles this.

### 6.4 Integration with KernelState

```rust
// sumi-kernel/src/lib.rs (KernelState)

pub struct KernelState<'a, DM: DirectMap> {
    pub page_alloc: &'a PageAllocator,
    pub kernel_alloc: &'a KernelAllocator<DM>,
    pub page_table: &'a RootPageTable<DM>,
    pub fd_table: spin::Mutex<FdTable>,           // NEW
    pub virtio_fs: Option<VirtioFsClient<'a, DM>>, // NEW
}
```

`fd_table` is behind a `spin::Mutex` because syscall handlers from multiple vCPUs need
mutable access. With up to 256 vCPUs doing concurrent I/O, the lock is held only for
the brief FD lookup/update (not during the actual I/O operation). Each handler copies
the `FileDescriptor` fields it needs under the lock, then releases before submitting
the FUSE request.

---

## 7. Syscall Handlers

### 7.1 io.rs — File I/O through virtio-fs

**`sys_read(fd, buf, count)` → nr 0**

```
if fd.kind == Console:
    return 0  (EOF — no stdin yet)
if fd.kind == File:
    send FUSE_READ { fh, offset: fd.offset, size: count }
    descriptor chain:
      [0] FuseInHeader + FuseReadIn          (device-readable)
      [1] FuseOutHeader (16 bytes)           (device-writable)
      [2] buf[0..count]                      (device-writable, ZERO-COPY)
    kick virtqueue
    on return: fd.offset += bytes_read
    return bytes_read (or -errno)
```

The data buffer points directly to the caller's `buf` pointer. The VM writes read data
directly into the guest's buffer through the virtqueue descriptor. No intermediate copy.

If `count > max_read` (1 MB), the kernel splits the read into multiple FUSE_READ
requests, each of at most `max_read` bytes, advancing offset between rounds. The total
bytes read is accumulated and returned to the caller. A short read (fewer bytes than
requested) terminates the loop early.

**`sys_write(fd, buf, count)` → nr 1**

```
if fd.kind == Console:
    for each byte in buf[0..count]:
        outb(0xE9, byte)  // debugcon
    return count
if fd.kind == File:
    send FUSE_WRITE { fh, offset: fd.offset, size: count }
    descriptor chain:
      [0] FuseInHeader + FuseWriteIn         (device-readable)
      [1] buf[0..count]                      (device-readable, ZERO-COPY)
      [2] FuseOutHeader + FuseWriteOut       (device-writable)
    kick virtqueue
    on return: fd.offset += bytes_written
    return bytes_written (or -errno)
```

If `count > max_write` (1 MB), the kernel splits the write into multiple FUSE_WRITE
requests, each of at most `max_write` bytes. A short write terminates the loop early.

**`sys_open(path, flags, mode)` → nr 2**

```
resolve path component-by-component via FUSE_LOOKUP
  (starting from root nodeid=1)
if O_CREAT and file not found:
    send FUSE_CREATE { parent_nodeid, name, flags, mode }
else:
    send FUSE_OPEN { nodeid, flags }
allocate fd with FdKind::File { fuse_fh, fuse_nodeid, offset: 0 }
return fd
```

**`sys_close(fd)` → nr 3**

```
match fd.kind:
    Console → just free the FD slot
    File    → send FUSE_RELEASE { fh }
              send FUSE_FORGET { nodeid, nlookup }
              free the FD slot
    Directory → send FUSE_RELEASEDIR { fh }
                send FUSE_FORGET { nodeid, nlookup }
                free the FD slot
return 0
```

**`sys_lseek(fd, offset, whence)` → nr 8**

Handled entirely in the kernel by updating `fd.offset`:

```
match whence:
    SEEK_SET → fd.offset = offset
    SEEK_CUR → fd.offset += offset
    SEEK_END → send FUSE_GETATTR to get file size
               fd.offset = size + offset
return fd.offset
```

**`sys_pread64(fd, buf, count, offset)` → nr 17**

Same as `sys_read` but uses the explicit `offset` argument instead of `fd.offset`.
Does not update `fd.offset`.

**`sys_pwrite64(fd, buf, count, offset)` → nr 18**

Same as `sys_write` but uses the explicit `offset` argument. Does not update `fd.offset`.

**`sys_readv(fd, iov, iovcnt)` → nr 19**

Iterates over the iovec array, calling the read path for each buffer. Could be optimized
later with multi-descriptor chains, but serial read is correct and simple for Phase 1.

**`sys_writev(fd, iov, iovcnt)` → nr 20**

Same approach as readv — iterate iovecs, write each buffer.

### 7.2 fs.rs — Filesystem Metadata

**`sys_stat(path, statbuf)` → nr 4**

```
resolve path via FUSE_LOOKUP chain → nodeid
send FUSE_GETATTR { nodeid }
fill statbuf from FuseAttr
send FUSE_FORGET { nodeid, nlookup }
return 0
```

**`sys_fstat(fd, statbuf)` → nr 5**

```
send FUSE_GETATTR { nodeid: fd.fuse_nodeid }
fill statbuf from FuseAttr
return 0
```

**`sys_openat(dirfd, path, flags, mode)` → nr 257**

```
if dirfd == AT_FDCWD:
    start_nodeid = cwd_nodeid  (tracked in KernelState)
else:
    start_nodeid = fd_table[dirfd].fuse_nodeid
resolve path from start_nodeid via FUSE_LOOKUP chain
FUSE_OPEN or FUSE_CREATE
allocate fd
return fd
```

**`sys_getdents(fd, dirent, count)` → nr 78**

```
send FUSE_READDIR { fh: fd.fuse_fh, offset: fd.offset, size: count }
parse FuseDirent stream from response
convert to Linux struct linux_dirent64 into dirent buffer
update fd.offset from last dirent.off
return bytes written to dirent buffer
```

**`sys_access(path, mode)` → nr 21**

```
resolve path via FUSE_LOOKUP → nodeid, attr
check attr.mode against requested mode (F_OK, R_OK, W_OK, X_OK)
FUSE_FORGET
return 0 or -EACCES
```

**`sys_getcwd(buf, size)` → nr 79**

Return the current working directory path stored in `KernelState`. Initially `"/"`.

**`sys_chdir(path)` → nr 80**

Resolve path via `FUSE_LOOKUP`, verify it's a directory, update `KernelState.cwd`.

### 7.3 Linux stat Structure

The kernel must fill the Linux `struct stat` from `FuseAttr`:

```rust
#[repr(C)]
pub struct LinuxStat {
    pub st_dev:     u64,
    pub st_ino:     u64,
    pub st_nlink:   u64,
    pub st_mode:    u32,
    pub st_uid:     u32,
    pub st_gid:     u32,
    pub __pad0:     u32,
    pub st_rdev:    u64,
    pub st_size:    i64,
    pub st_blksize: i64,
    pub st_blocks:  i64,
    pub st_atime:   i64,
    pub st_atime_nsec: i64,
    pub st_mtime:   i64,
    pub st_mtime_nsec: i64,
    pub st_ctime:   i64,
    pub st_ctime_nsec: i64,
    pub __unused:   [i64; 3],
}
```

---

## 8. VM-Side Backend

### 8.1 VirtIO MMIO Device Emulation

The VM process handles `KVM_EXIT_MMIO` by dispatching to a device model:

```rust
// sumi-vm/src/devices/virtio_mmio.rs

pub struct VirtioMmioDevice {
    // Standard virtio state
    status: u32,
    device_features: u64,
    driver_features: u64,
    queue_sel: u32,
    queues: [VirtqueueState; 2],  // hiprio + request

    // Device-specific backend
    backend: Box<dyn VirtioDevice>,
}

pub trait VirtioDevice {
    fn device_id(&self) -> u32;
    fn device_features(&self) -> u64;
    fn config_read(&self, offset: u64) -> u32;
    fn process_queue(&mut self, queue: &mut VirtqueueState, mem: &GuestMemoryMmap<()>);
}
```

### 8.2 MMIO Exit Handling

Each vCPU runs on its own host thread. MMIO exits are handled per-thread:

```rust
// sumi-vm/src/arch/x86_64/kvm/mod.rs — updated VCpu::run()
// `devices` is shared across vCPU threads via Arc<Mutex<DeviceRegistry>>

loop {
    match self.fd.run()? {
        VcpuExit::IoOut(0xE9, data) => { /* debugcon */ }

        VcpuExit::MmioRead(addr, data) => {
            let mut devs = devices.lock();
            if let Some(dev) = devs.find_device(addr) {
                let val = dev.mmio_read(addr - dev.base, data.len());
                data.copy_from_slice(&val.to_le_bytes()[..data.len()]);
            }
        }

        VcpuExit::MmioWrite(addr, data) => {
            let mut devs = devices.lock();
            if let Some(dev) = devs.find_device(addr) {
                let offset = addr - dev.base;
                dev.mmio_write(offset, data);

                // If this was a QueueNotify write, process pending requests NOW
                if offset == 0x050 {
                    dev.backend.process_queue(&mut dev.queues[queue_idx], &mem);
                }
            }
        }

        VcpuExit::Hlt | VcpuExit::Shutdown => return Ok(()),
        other => return Err(...),
    }
}
```

The critical insight: when `QueueNotify` is written, the VM processes all pending requests
**before returning to the guest**. The guest resumes only after responses are in the used
ring.

### 8.3 FUSE Server

```rust
// sumi-vm/src/devices/virtio_fs.rs

pub struct VirtioFs {
    share_root: PathBuf,              // Host directory shared with guest
    nodes: Vec<FuseNode>,             // nodeid → host state
    file_handles: Vec<Option<File>>,  // fh → host File
    next_nodeid: u64,
    next_fh: u64,
}

struct FuseNode {
    host_path: PathBuf,       // Absolute path on host
    parent: u64,              // Parent nodeid
    lookup_count: u64,        // Incremented by LOOKUP, decremented by FORGET
}
```

**Security**: All host paths are resolved relative to `share_root`. The server uses
`openat2(RESOLVE_BENEATH)` to prevent symlink escapes.

**FUSE_LOOKUP** → `fstatat(parent_host_fd, name)`, assign nodeid, return attributes.

**FUSE_OPEN** → `open(node.host_path, flags)`, assign fh, return handle.

**FUSE_READ** → `pread(host_fd, guest_buf_ptr, count, offset)`. The VM reads the
guest buffer address from the virtqueue descriptor and writes data directly into guest
memory via `GuestMemoryMmap::write_slice()`. Zero-copy from VM's perspective — one
host syscall, one memcpy into guest RAM.

**FUSE_WRITE** → `pwrite(host_fd, data, count, offset)`. The VM reads write data
directly from guest memory via `GuestMemoryMmap::read_slice()`.

**FUSE_GETATTR** → `fstat(host_fd)` or `stat(host_path)`, convert to `FuseAttr`.

**FUSE_READDIR** → `getdents64(host_fd)`, convert to FUSE dirent stream.

### 8.4 CLI Integration

```bash
sumi-vm run --kernel path/to/kernel --share /host/directory --mem 256M
```

The `--share` flag specifies the host directory mounted as `/` in the guest.

---

## 9. Memory Layout Changes

### 9.1 New Constants in sumi-abi

```rust
// sumi-abi/src/arch/x86_64/layout.rs

/// VirtIO MMIO device region: 127 TB, within 128 TB direct map.
pub const VIRTIO_MMIO_BASE: PhysicalAddr = PhysicalAddr::new(0x7F00_0000_0000);
pub const VIRTIO_MMIO_STRIDE: usize = 0x1000; // 4KB per device
```

### 9.2 Virtqueue Allocation

Virtqueue memory is allocated at boot from `PageAllocator`. For `QUEUE_SIZE = 256`:

| Structure        | Size                  | Allocation         |
|------------------|-----------------------|--------------------|
| Descriptor table | 256 * 16 = 4096 B    | 1 page (2 MB)      |
| Available ring   | 4 + 256*2 = 516 B    | (shares page)      |
| Used ring        | 4 + 256*8 = 2052 B   | (shares page)      |

All three structures total ~6.6 KB per queue. Allocated via `KernelAllocator` (kmalloc)
from sub-page memory. The descriptor table is placed first, available ring after
descriptors, used ring after available ring, all properly aligned.

### 9.3 FUSE Request Buffers

FUSE headers are small (40 + body bytes for request, 16 + body for response). These are
allocated from `KernelAllocator` (sub-page allocation) as needed.

For read/write data, the virtqueue descriptor points directly to the caller's buffer —
no additional allocation.

---

## 10. Module Layout

### Kernel (sumi-kernel)

```
sumi-kernel/src/
├── fs/
│   ├── mod.rs              FdTable, FileDescriptor, FdKind
│   └── virtio_fs.rs        VirtioFsClient: FUSE protocol, path resolution
├── drivers/
│   └── virtio/
│       ├── mod.rs           Common virtio types
│       ├── mmio.rs          MMIO register access (read/write device regs)
│       └── virtqueue.rs     Split virtqueue: alloc, submit, complete
├── syscall/
│   └── handlers/
│       ├── io.rs            sys_read/write/open/close/lseek/pread/pwrite (UPDATED)
│       └── fs.rs            sys_stat/fstat/openat/getdents/access/... (UPDATED)
└── kernel_main.rs           Init virtio-fs device, open FD 0/1/2 (UPDATED)
```

### VM (sumi-vm)

```
sumi-vm/src/
├── devices/
│   ├── mod.rs               Device registry, find_device()
│   ├── virtio_mmio.rs       VirtIO MMIO register emulation
│   └── virtio_fs.rs         FUSE server, host syscall translation
├── arch/x86_64/kvm/
│   └── mod.rs               Handle MmioRead/MmioWrite exits (UPDATED)
└── cmd/
    └── run.rs               --share flag (UPDATED)
```

### ABI (sumi-abi)

```
sumi-abi/src/
├── arch/x86_64/
│   └── layout.rs            VIRTIO_MMIO_BASE constant (UPDATED)
├── fuse.rs                  FUSE protocol types (NEW)
└── virtio.rs                VirtqDesc, VirtqAvail, VirtqUsed (NEW)
```

Shared types (`VirtqDesc`, FUSE structs) go in `sumi-abi` because both kernel and VM
reference them. The kernel writes `VirtqDesc` entries; the VM reads them. The kernel
writes `FuseInHeader`; the VM parses it.

---

## 11. Implementation Plan

### Phase 1: Core Read/Write Path

1. **sumi-abi**: Add `VIRTIO_MMIO_BASE` to layout, add `virtio.rs` (virtqueue types),
   add `fuse.rs` (FUSE types for INIT, LOOKUP, OPEN, READ, WRITE, RELEASE, GETATTR,
   FORGET).

2. **sumi-kernel/drivers/virtio**: Implement MMIO register read/write helpers and
   split virtqueue (alloc, submit descriptor chain, read used ring).

3. **sumi-kernel/fs**: Implement `FdTable` with console FDs pre-allocated. Implement
   `VirtioFsClient` with FUSE session init, lookup, open, read, write, release, forget.

4. **sumi-kernel/syscall/handlers/io.rs**: Implement `sys_read`, `sys_write`, `sys_open`,
   `sys_close`, `sys_lseek` dispatching through FD table and VirtioFsClient.

5. **sumi-vm/devices**: Implement `VirtioMmioDevice` register emulation. Implement
   `VirtioFs` FUSE server with INIT, LOOKUP, OPEN, READ, WRITE, RELEASE, GETATTR, FORGET
   backed by host syscalls.

6. **sumi-vm/kvm**: Handle `MmioRead`/`MmioWrite` exits, dispatch to device registry.

7. **sumi-vm/cmd/run.rs**: Add `--share` CLI flag.

8. **sumi-kernel/kernel_main.rs**: Initialize virtio-fs device at boot (probe, negotiate,
   FUSE_INIT), set up console FDs.

### Phase 2: Full Filesystem

9. `sys_stat`, `sys_fstat`, `sys_lstat`, `sys_access`, `sys_openat`, `sys_newfstatat`.
10. `sys_getdents` via FUSE_OPENDIR / FUSE_READDIR / FUSE_RELEASEDIR.
11. `sys_getcwd`, `sys_chdir` — kernel-side CWD tracking.
12. `sys_pread64`, `sys_pwrite64`, `sys_readv`, `sys_writev`.
13. Mutation operations: `sys_mkdir`, `sys_rmdir`, `sys_unlink`, `sys_rename`,
    `sys_symlink`, `sys_link`, `sys_readlink`, `sys_creat`.

### Phase 3: Performance

14. DAX window: map host file pages directly into guest physical memory. Requires
    a dedicated physical memory region and KVM memslot management per mapping.
    Eliminates virtqueue round-trip for read/write on mapped regions.
15. Dirent caching: cache FUSE_LOOKUP results to avoid repeated lookups for the same path.
16. Read-ahead: pre-fetch file data into a kernel buffer for sequential reads.

---

## 12. Open Questions

1. **FUSE_FORGET batching** — FUSE_FORGET has no response, so it doesn't need a
   synchronous round-trip. We could batch forgets and send them on the hiprio queue,
   or just send them inline on the request queue. Inline is simpler; batch if profiling
   shows overhead.

2. **dup() semantics** — When two FDs share a FUSE file handle, FUSE_RELEASE must only
   be sent when the last FD is closed. Need a refcount on `(fuse_fh, fuse_nodeid)` pairs.
   Simplest: a small array of `{ fuse_fh, refcount }` in VirtioFsClient.

3. **stdin** — Currently returns EOF. A virtio-console device would provide real stdin
   in the future. Not needed for file I/O.

4. **O_APPEND** — Requires the kernel to seek to end-of-file before each write.
   Can be done with FUSE_GETATTR + write at size, or by passing O_APPEND to the host
   open() and ignoring the kernel-side offset for writes.

5. **Error mapping** — FUSE returns `-errno` in `FuseOutHeader.error`. These match Linux
   errno values, so the kernel can return them directly to the caller as `SyscallResult`.
