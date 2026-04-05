# Host Filesystem via virtio-fs — Design Document

## 1. Background

sumi runs Linux ELF binaries that expect a POSIX filesystem. Since the unikernel has no
block device or local filesystem, all file I/O is forwarded to the host. We use virtio
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

### Non-goals

- DAX / shared memory mapping.
- mmap of files (requires DAX or page-fault forwarding).
- File locking (flock/fcntl).
- Extended attributes, ACLs, inotify.
- Mutation operations (mkdir, rmdir, unlink, rename) — stubs return ENOSYS.

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

VirtIO MMIO transport (virtio spec v1.2, section 4.2). Each virtio device occupies a 4KB
MMIO register region at a fixed physical address known at compile time.

### 3.1 MMIO Register Layout

All registers are 32-bit. Offset from device base address:

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

### 3.2 MMIO Address

```
VIRTIO_MMIO_BASE   = 0x10_0000_0000  (64 GB)
VIRTIO_MMIO_STRIDE = 0x1000          (4 KB per device)
VIRTIO_FS_MMIO     = VIRTIO_MMIO_BASE  (device 0)
```

Placed above reasonable guest RAM, within the 128 TB direct-map range. The kernel
accesses via `VirtualAddr = DIRECT_MAP_OFFSET + VIRTIO_FS_MMIO`. No KVM memory region
is registered — all accesses trigger `KVM_EXIT_MMIO`.

---

## 4. Split Virtqueue

Split virtqueue format (virtio spec section 2.7). Three physically-contiguous regions
allocated by the kernel from `KernelAllocator`.

- **Descriptor Table**: 256 entries × 16 bytes. Chains form FUSE requests (2-3 descriptors).
- **Available Ring**: Kernel publishes descriptor chain heads for the VM.
- **Used Ring**: VM writes completed chain heads with bytes written.

`QUEUE_SIZE = 256`. Only queue 1 (request queue) is used.

### 4.1 Synchronous Completion Model

Each vCPU independently submits descriptors and triggers KVM exits. The guest kernel
protects the virtqueue with a `spin::Mutex`, held only during descriptor submission.
The MMIO exit (I/O) happens outside the lock.

The VM processes all pending requests in `process_queue()` before returning to the guest.
`avail.flags = VIRTQ_AVAIL_F_NO_INTERRUPT` — no host→guest notifications needed.

---

## 5. FUSE Protocol

FUSE 7.31. Every message starts with `FuseInHeader` (40 bytes) / `FuseOutHeader` (16 bytes).

### 5.1 Implemented Operations

| Opcode | Name              | Descriptor Chain                        | Used By                      |
|--------|-------------------|-----------------------------------------|------------------------------|
| 26     | `FUSE_INIT`       | [hdr+body] → [out_hdr+body]            | Device init                  |
| 1      | `FUSE_LOOKUP`     | [hdr+name\0] → [out_hdr+entry]         | open, stat, access, chdir    |
| 3      | `FUSE_GETATTR`    | [hdr+body] → [out_hdr+attr]            | fstat, stat, lseek(SEEK_END) |
| 14     | `FUSE_OPEN`       | [hdr+body] → [out_hdr+open]            | open, openat                 |
| 28     | `FUSE_OPENDIR`    | [hdr+body] → [out_hdr+open]            | openat(O_DIRECTORY)          |
| 35     | `FUSE_CREATE`     | [hdr+body+name\0] → [out_hdr+entry+open]| creat                       |
| 15     | `FUSE_READ`       | [hdr+body] → [out_hdr] [data_buf]      | read, pread64, readv         |
| 16     | `FUSE_WRITE`      | [hdr+body] [data_buf] → [out_hdr+write] | write, pwrite64, writev     |
| 29     | `FUSE_READDIR`    | [hdr+body] → [out_hdr] [dirent_buf]    | getdents64                   |
| 18     | `FUSE_RELEASE`    | [hdr+body] → [out_hdr]                 | close (file)                 |
| 30     | `FUSE_RELEASEDIR` | [hdr+body] → [out_hdr]                 | close (directory)            |
| 2      | `FUSE_FORGET`     | [hdr+body] (no response)               | close, path cleanup          |

### 5.2 Path Resolution

FUSE resolves paths component-by-component from root (`nodeid = 1`):

```
open("/data/input.txt"):
  FUSE_LOOKUP(parent=1, "data")      → nodeid=2
  FUSE_LOOKUP(parent=2, "input.txt") → nodeid=3
  FUSE_OPEN(nodeid=3, O_RDONLY)      → fh=1
```

**Intermediate nodeids are forgotten immediately** — `resolve_path()` calls
`FUSE_FORGET` on each intermediate nodeid (except root) as it walks. Only the final
nodeid is retained with its lookup reference.

### 5.3 READDIR Format

FUSE_READDIR returns a packed stream of `FuseDirent` entries (8-byte aligned):

```
struct FuseDirent {
    ino: u64,      // Inode number
    off: u64,      // Offset for next readdir call
    namelen: u32,  // Length of name
    typ: u32,      // File type (DT_REG=8, DT_DIR=4, etc.)
    // name[namelen] follows, padded to 8-byte alignment
}
```

The VM-side server synthesizes `.` and `..` entries, then iterates the host directory.
Offset is a 1-based entry index. The kernel converts FUSE dirents to `linux_dirent64`
format (19-byte header: d_ino u64, d_off i64, d_reclen u16, d_type u8, then name).

---

## 6. File Descriptor Table

### 6.1 Design

```rust
pub const MAX_FDS: usize = 256;

pub enum FdKind {
    Console,
    File { fuse_fh: u64, fuse_nodeid: u64, offset: u64 },
    Directory { fuse_fh: u64, fuse_nodeid: u64, offset: u64 },
}

pub struct FileDescriptor { pub kind: FdKind, pub flags: u32 }
pub struct FdTable { fds: [Option<FileDescriptor>; MAX_FDS] }
```

Pre-allocated: fd 0 (stdin/Console), fd 1 (stdout/Console), fd 2 (stderr/Console).

### 6.2 Allocation

`alloc()` scans from index 0 for the first `None` slot (Linux "lowest fd" guarantee).
`put(fd, desc)` places a descriptor at a specific slot (for dup2), returning any evicted
descriptor for cleanup. `free(fd)` removes and returns the old descriptor.

### 6.3 dup/dup2 and Reference Counting

`dup`/`dup2` copy the `FileDescriptor` including `fuse_fh`. Multiple fds may reference
the same FUSE file handle. `FUSE_RELEASE` is sent only when the last fd referencing a
given `fuse_fh` is closed.

Implemented via `count_fh_refs(fh)` — scans the fd table for remaining references.
`sys_close` and `sys_dup2` check this count after removing/replacing the fd slot:
- If `remaining_refs == 0`: call `release(fh)` (or `releasedir(fh)`) and `forget(nodeid, 1)`.
- Otherwise: just remove the fd slot, leave the FUSE handle open.

### 6.4 Global State

```rust
// sumi-kernel/src/lib.rs
pub static FD_TABLE: spin::Mutex<FdTable> = spin::Mutex::new(FdTable::new());
pub static VIRTIO_FS: spin::Once<VirtioFsClient> = spin::Once::new();
```

Handlers copy the fields they need under the lock, then release before FUSE I/O.

---

## 7. Syscall Handlers

### 7.1 io.rs — File I/O

| Syscall     | Nr  | Implementation                                              |
|-------------|-----|-------------------------------------------------------------|
| `read`      | 0   | Console→0, File→FUSE_READ via `fs_transfer_chunked`, updates offset |
| `write`     | 1   | Console→debugcon, File→FUSE_WRITE chunked, updates offset   |
| `open`      | 2   | `resolve_path` + FUSE_OPEN, alloc fd                        |
| `close`     | 3   | Free fd, release/forget if last reference                    |
| `lseek`     | 8   | SEEK_SET/CUR in-kernel, SEEK_END via FUSE_GETATTR           |
| `pread64`   | 17  | FUSE_READ at explicit offset, no fd offset update            |
| `pwrite64`  | 18  | FUSE_WRITE at explicit offset, no fd offset update           |
| `readv`     | 19  | Iterate iovecs, chunked read per buffer                      |
| `writev`    | 20  | Console→debugcon per byte, File→chunked write per iovec      |
| `dup`       | 32  | Copy descriptor, alloc lowest fd                             |
| `dup2`      | 33  | Copy descriptor to target fd, evict + cleanup old occupant   |

**Chunked transfer**: `fs_transfer_chunked()` splits I/O at 2MB page boundaries for
correct physical address translation. Each chunk is a separate FUSE request. Transfer
counts are clamped to prevent underflow from unexpected device responses.

**iovec handling**: `iov_len` is clamped to `u32::MAX` before casting. `read`/`write`
counts are similarly clamped.

### 7.2 fs.rs — Filesystem Metadata

| Syscall       | Nr  | Implementation                                            |
|---------------|-----|-----------------------------------------------------------|
| `stat`        | 4   | `resolve_path` + FUSE_GETATTR → Linux `Stat`, forget     |
| `fstat`       | 5   | FUSE_GETATTR by nodeid, Console→synthetic char device stat |
| `lstat`       | 6   | Same as stat (no symlink distinction)                     |
| `access`      | 21  | Resolve path (existence check only), forget               |
| `getdents`    | 78  | Returns ENOSYS (old 32-bit format)                        |
| `getcwd`      | 79  | Returns `"/\0"` (unikernel cwd is always root)           |
| `chdir`       | 80  | Validates path exists, returns 0                          |
| `fchdir`      | 81  | Returns 0                                                 |
| `creat`       | 85  | Split path → FUSE_CREATE on parent, alloc fd              |
| `readlink`    | 89  | Returns EINVAL (no symlinks)                              |
| `getdents64`  | 217 | FUSE_READDIR → parse FuseDirent → write linux_dirent64    |
| `openat`      | 257 | AT_FDCWD + O_DIRECTORY support, FUSE_OPEN or FUSE_OPENDIR |
| `newfstatat`  | 262 | AT_EMPTY_PATH→fstat, otherwise stat by path               |

**getdents64 details**: Reads FUSE dirents into a 4KB kernel buffer, converts to
`linux_dirent64` format (19-byte header written at raw offsets to match Linux ABI),
updates directory fd offset from last `dirent.off`. Returns EINVAL if the user buffer
is too small for even one entry. Guards against `d_reclen` u16 overflow.

**Resource management**: All paths call `forget_if_not_root()` — a helper that only
sends FUSE_FORGET for non-root nodeids. Error paths in `open`/`openat`/`creat` clean
up both the FUSE file handle (release) and nodeid (forget) on failure.

### 7.3 Stubbed Syscalls (ENOSYS)

`poll`, `ioctl`, `pipe`, `select`, `rename`, `mkdir`, `rmdir`, `link`, `unlink`,
`symlink`, `unlinkat`.

---

## 8. VM-Side FUSE Server

### 8.1 Data Structures

```rust
pub struct VirtioFs {
    nodes: Vec<Option<FuseNode>>,       // nodeid → host path
    file_handles: Vec<Option<File>>,    // fh → host File
    last_avail_idx: u16,
}

struct FuseNode {
    host_path: PathBuf,
    _lookup_count: u64,
}
```

- `nodes[0]` = None (FUSE convention), `nodes[1]` = root directory.
- `alloc_nodeid()` appends to the vector, returns index.
- `alloc_fh()` reuses freed slots or appends.

### 8.2 FUSE Handlers

**FUSE_INIT**: Returns version 7.31, `max_write = 1MB`.

**FUSE_LOOKUP**: Resolves name under parent's host path via `std::fs::metadata`.
Allocates a new nodeid. Returns `FuseEntryOut` with attributes.

**FUSE_GETATTR**: Reads host metadata for the node's path.

**FUSE_OPEN / FUSE_OPENDIR**: Opens the host file with translated flags
(`O_RDONLY`/`O_WRONLY`/`O_RDWR`, `O_CREAT`, `O_TRUNC`, `O_APPEND`). Allocates fh.

**FUSE_CREATE**: Creates and opens in one step. Returns `FuseEntryOut + FuseOpenOut`.

**FUSE_READ**: Seeks to offset, reads in a loop until buffer is full or EOF.
Retries on `EINTR`. Uses split descriptors: header in buf[0], data in buf[1].

**FUSE_WRITE**: Reads write data from second readable descriptor. Seeks and writes.

**FUSE_READDIR**: Reads host directory via `std::fs::read_dir()`. Synthesizes `.` and
`..` entries. Packs `FuseDirent` entries with 8-byte alignment. Uses `OsStrExt::as_bytes()`
to preserve non-UTF-8 filenames. Offset is a 1-based entry index.

**FUSE_RELEASE / FUSE_RELEASEDIR**: Drops the host `File` handle.

**FUSE_FORGET**: Clears the node entry (sets `nodes[nodeid] = None`).

### 8.3 Response Helpers

- `write_response(unique, body, writable_bufs, mem)` — writes `FuseOutHeader` + body,
  handles cross-buffer splits (header in buf[0], overflow into buf[1]).
- `write_error(unique, errno, writable_bufs, mem)` — writes error-only header.

---

## 9. Module Layout

### Kernel (sumi-kernel)

```
sumi-kernel/src/
├── fs/
│   ├── mod.rs              FdTable, FileDescriptor, FdKind, count_fh_refs
│   └── virtio_fs.rs        VirtioFsClient: 12 FUSE operations, path resolution
├── drivers/
│   └── virtio/
│       ├── mmio.rs          MMIO register access (read32/write32)
│       └── virtqueue.rs     Split virtqueue: alloc, submit, complete, free_chain
├── syscall/
│   ├── mod.rs               Dispatch table (100+ syscalls)
│   ├── errno.rs             EIO, EBADF, ENOMEM, EFAULT, ENOTDIR, EINVAL, EMFILE, ENOSYS
│   └── handlers/
│       ├── io.rs            read/write/open/close/lseek/pread/pwrite/readv/writev/dup/dup2
│       └── fs.rs            stat/fstat/lstat/access/getdents64/getcwd/chdir/creat/openat/newfstatat
├── selftest/
│   ├── virtio/fs.rs         init, create_write_read, read_print
│   └── syscalls/
│       ├── io/              write, read, close
│       └── fs/              open, pread, lseek, fstat, stat, openat, dup
└── lib.rs                   FD_TABLE, VIRTIO_FS globals
```

### VM (sumi-vm)

```
sumi-vm/src/
├── devices/
│   ├── mod.rs               Device registry, MMIO address routing
│   ├── virtio_mmio.rs       VirtIO MMIO register emulation, queue state
│   └── virtio_fs.rs         FUSE server: 12 handlers, host FS translation
└── ...
```

### ABI (sumi-abi)

```
sumi-abi/src/
├── fuse.rs                  FUSE 7.31 types: headers, attrs, FuseDirent, etc.
├── virtio.rs                VirtqDesc, VirtqAvail, VirtqUsed, MMIO constants
├── stat.rs                  Linux Stat (144 bytes), linux_dirent64, AT_FDCWD, DT_* constants
└── arch/x86_64/layout.rs    VIRTIO_MMIO_BASE, VIRTIO_FS_MMIO
```

---

## 10. Selftests

17 tests run under KVM via `make self-test`:

| Suite       | Test               | What it verifies                              |
|-------------|--------------------|-----------------------------------------------|
| fd_table    | console_fds        | fds 0-2 are Console                           |
| fd_table    | alloc_free_lowest  | Lowest-fd allocation and reuse                |
| syscall_io  | write_console      | write(1, "hi", 2) → 2                        |
| syscall_io  | read_console_eof   | read(0) → 0                                   |
| syscall_io  | close_bad_fd       | close(999) → -EBADF                           |
| virtio_fs   | init               | VIRTIO_FS device probed                       |
| virtio_fs   | create_write_read  | FUSE create/write/read round-trip             |
| virtio_fs   | read_print         | Write+read+print via debugcon                 |
| syscall_fs  | open_read_close    | Full open→read→close via syscalls             |
| syscall_fs  | write_pread        | pread64 reads at offset without fd update     |
| syscall_fs  | lseek              | SEEK_SET, SEEK_CUR, SEEK_END                  |
| syscall_fs  | open_enoent        | open nonexistent → -ENOENT                    |
| syscall_fs  | fstat_file_size    | fstat returns correct st_size                  |
| syscall_fs  | stat_file          | stat by path returns correct st_size           |
| syscall_fs  | stat_enoent        | stat nonexistent → -ENOENT                    |
| syscall_fs  | openat_fdcwd       | openat(AT_FDCWD, path) works                  |
| syscall_fs  | dup2_read          | dup2 then read from duped fd                  |

---

## 11. Known Limitations

1. **readdir ordering**: The VM re-reads the host directory on every FUSE_READDIR call.
   If the directory is modified between calls, entries may be skipped or duplicated.

2. **access() ignores mode**: Only checks file existence, not permission bits.

3. **No mutation ops**: mkdir, rmdir, unlink, rename, symlink, link all return ENOSYS.

4. **No mmap of files**: Dynamic linking (loading .so via mmap) is not supported.
   Only statically-linked ELF binaries work.

5. **No fcntl/ioctl**: Some libc runtimes call these at startup.

6. **stdin returns EOF**: Console fd 0 always returns 0 bytes.

7. **Single-threaded readdir**: Directory listing is not cached per open handle.
