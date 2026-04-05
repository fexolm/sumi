# Virtio Console — Design Document

## 1. Background

Today all console output (stdout, stderr) and `kprintln!` use the QEMU debugcon port
(0xE9). Each byte triggers a separate `KVM_EXIT_IO`, the VM catches it, and writes to
the host's stdout. This works but has problems:

1. **One byte per VM exit.** A 100-byte `write(1, buf, 100)` causes 100 VM exits.
   Each exit is a context switch through KVM — tens of microseconds of overhead per byte.
2. **No stdin.** `read(0, ...)` returns 0 immediately. User programs cannot read input.
3. **No separation between kernel debug output and user program output.** `kprintln!`
   and `write(1, ...)` both go to the same debugcon port. There is no way for the host
   to distinguish them or redirect them independently.
4. **Not a real device.** Debugcon is a QEMU-ism. The virtio transport is already in
   place for virtio-fs — reusing it for console gives us a standard device model.

### Goals

- Replace per-byte debugcon I/O with batched virtio transfers for `write(1/2, ...)`.
- Support `read(0, ...)` — host can feed stdin to the guest.
- Keep `kprintln!` on debugcon for early-boot and panic output (before virtio is ready).
- One VM exit per `write()` call instead of one per byte.

### Non-goals

- Terminal emulation (ANSI escape codes, line editing) — the host terminal handles this.
- Multiple console ports (multiport feature).
- Console resize notifications (`VIRTIO_CONSOLE_F_SIZE`).
- Emergency write (`VIRTIO_CONSOLE_F_EMERG_WRITE`).

---

## 2. Architecture Overview

```
  Guest (sumi-kernel)                        Host (sumi-vm)
 +---------------------------+             +-----------------------------+
 | User program              |             |                             |
 |   write(1, buf, n)        |             |                             |
 +-----------+---------------+             |                             |
             | syscall                     |                             |
 +-----------v---------------+             |                             |
 | sys_write()               |             |                             |
 |   fd 1/2 → Console        |             |                             |
 +-----------+---------------+             |                             |
             |                             |                             |
 +-----------v---------------+             |                             |
 | VirtioConsole              |             |                             |
 |   copy buf → tx desc      |             |                             |
 |   submit chain            |             |                             |
 |   kick transmitq          |  MMIO exit  |                             |
 +-----------+---------------+------------>| VirtioMmioDevice (ID=3)    |
             |                             |   read desc from guest RAM  |
             |                             |   write buf → host stdout   |
             |                             |   post used ring entry      |
             |               MMIO return   |                             |
             |<----------------------------+                             |
 +-----------v---------------+             |                             |
 | return bytes written      |             |                             |
 +---------------------------+             +-----------------------------+
```

For stdin (receiveq):

```
  Guest                                      Host
 +---------------------------+             +-----------------------------+
 | sys_read(fd=0)            |             |                             |
 |   post empty desc on rxq  |             |                             |
 |   kick receiveq           |  MMIO exit  |                             |
 +-----------+---------------+------------>| read stdin into guest desc  |
             |               MMIO return   |   post used ring with len   |
             |<----------------------------+                             |
 | return bytes_read         |             |                             |
 +---------------------------+             +-----------------------------+
```

---

## 3. Virtio Console Device (virtio spec v1.2, section 5.3)

Device ID: **3** (`VIRTIO_DEVICE_CONSOLE`).

### 3.1 Queues

| Queue | Name       | Direction      | Purpose                    |
|-------|------------|----------------|----------------------------|
| 0     | receiveq   | device→driver  | Host stdin → guest buffer  |
| 1     | transmitq  | driver→device  | Guest buffer → host stdout |

Each queue uses the existing `Virtqueue` implementation (split virtqueue, 256 entries).

### 3.2 Feature Bits

None negotiated. We don't use `VIRTIO_CONSOLE_F_SIZE` or `VIRTIO_CONSOLE_F_MULTIPORT`.

### 3.3 MMIO Address

```
VIRTIO_CONSOLE_MMIO = VIRTIO_MMIO_BASE + VIRTIO_MMIO_STRIDE
                    = 0x10_0000_0000 + 0x1000
                    = 0x10_0000_1000
```

Device 1 in the MMIO device space (device 0 = virtio-fs).

---

## 4. Kernel-Side: VirtioConsole

### 4.1 Data Structures

```rust
// sumi-kernel/src/drivers/virtio/console.rs

pub struct VirtioConsole {
    transmitq: Virtqueue,   // queue 1 — guest→host writes
    receiveq: Virtqueue,    // queue 0 — host→guest reads
    mmio_base: VirtualAddr, // DIRECT_MAP_OFFSET + VIRTIO_CONSOLE_MMIO
    tx_buf: PhysicalAddr,   // bounce buffer for transmit (one page)
    rx_buf: PhysicalAddr,   // bounce buffer for receive (one page)
}
```

Bounce buffers are needed because `write()/read()` syscalls pass user virtual
addresses that may span multiple physical pages. Rather than building a multi-descriptor
scatter-gather chain per page boundary, we `memcpy` into a contiguous kernel buffer and
submit a single descriptor. The buffer is one 2 MB page (from `PageAllocator`) — more
than enough for any single write/read call.

### 4.2 Initialization

Called from `kernel_main()` after page allocator is ready, before user program exec.

```rust
pub fn init<DM: DirectMap>(
    kalloc: &KernelAllocator<DM>,
    palloc: &PageAllocator<DM>,
) -> Result<Self, MemoryError>
```

Initialization follows the standard virtio MMIO sequence:

1. Read `MagicValue` (0x74726976), `Version` (2), `DeviceID` (3) — validate.
2. Write `Status = ACKNOWLEDGE | DRIVER`.
3. Read/write feature bits (none for now).
4. Write `Status |= FEATURES_OK`, read back to confirm.
5. Allocate two `Virtqueue`s via `KernelAllocator`.
6. For each queue (0 and 1):
   - Write `QueueSel = idx`
   - Write `QueueNum = 256`
   - Write descriptor/avail/used physical addresses
   - Write `QueueReady = 1`
7. Allocate two pages as bounce buffers via `PageAllocator`.
8. Write `Status |= DRIVER_OK`.

### 4.3 Transmit (write)

```rust
pub fn write(&self, data: &[u8]) -> usize
```

1. Clamp `data.len()` to bounce buffer size.
2. Copy `data` into `tx_buf` (physical → virtual via direct map).
3. Allocate one descriptor: `addr = tx_buf_phys, len = data.len(), flags = 0` (device-readable).
4. Submit to transmitq, write `QueueNotify = 1`.
5. MMIO exit fires — host processes the descriptor synchronously.
6. Poll `complete()` for the used ring entry.
7. Free the descriptor chain.
8. Return bytes written.

For writes larger than the bounce buffer, loop in chunks.

### 4.4 Receive (read)

```rust
pub fn read(&self, buf: &mut [u8]) -> usize
```

1. Clamp to bounce buffer size.
2. Allocate one descriptor: `addr = rx_buf_phys, len = buf.len(), flags = VIRTQ_DESC_F_WRITE` (device-writable).
3. Submit to receiveq, write `QueueNotify = 0`.
4. MMIO exit — host reads from stdin, writes into guest buffer, posts used entry with actual byte count.
5. Poll `complete()` — `used_elem.len` = bytes actually read.
6. Copy `rx_buf[..bytes_read]` → `buf`.
7. Free the descriptor chain.
8. Return bytes read (0 = EOF).

### 4.5 Global State

```rust
// sumi-kernel/src/lib.rs
pub static VIRTIO_CONSOLE: spin::Once<VirtioConsole> = spin::Once::new();
```

---

## 5. VM-Side: Console Backend

### 5.1 Data Structures

```rust
// sumi-vm/src/devices/virtio_console.rs

pub struct VirtioConsoleBackend {
    stdin_buf: Vec<u8>,      // buffered stdin (read ahead)
    stdin_eof: bool,
    last_avail_idx: [u16; 2], // per-queue avail index tracking
}
```

### 5.2 Queue Processing

Called when the guest writes to `QueueNotify`:

**Transmitq (queue 1) — guest→host:**

```rust
fn process_transmit(&mut self, queue: &VirtqueueState, mem: &GuestMemoryMmap) {
    // Walk available ring from last_avail_idx[1]
    // For each descriptor chain:
    //   1. Read data from guest memory at desc.addr, desc.len
    //   2. Write to host stdout
    //   3. Post to used ring with len = bytes_written
    // Flush stdout after all descriptors processed
}
```

**Receiveq (queue 0) — host→guest:**

```rust
fn process_receive(&mut self, queue: &VirtqueueState, mem: &GuestMemoryMmap) {
    // Walk available ring from last_avail_idx[0]
    // For each descriptor (VIRTQ_DESC_F_WRITE):
    //   1. Read from host stdin (non-blocking) into local buffer
    //   2. Copy min(available_bytes, desc.len) into guest memory
    //   3. Post to used ring with len = bytes_read
    // If stdin is a pipe/file and returns 0 bytes, set stdin_eof
}
```

Host stdin reading uses `std::io::Read::read()` directly. If stdin is a TTY, it blocks
until data is available (acceptable for a unikernel — there's only one vCPU). If stdin
is a pipe/file, EOF is propagated as `len = 0` in the used ring.

### 5.3 VirtioMmioDevice Generalization

Currently `VirtioMmioDevice` is hardcoded to virtio-fs. We need to generalize it.

```rust
// sumi-vm/src/devices/virtio_mmio.rs

pub trait VirtioBackend {
    fn device_id(&self) -> u32;
    fn num_queues(&self) -> usize;
    fn process_queue(&mut self, queue_idx: usize, queue: &VirtqueueState, mem: &GuestMemoryMmap);
}

pub struct VirtioMmioDevice {
    status: u32,
    // ... same register state ...
    queues: Vec<VirtqueueState>,
    backend: Box<dyn VirtioBackend>,
}
```

`VirtioFs` and `VirtioConsoleBackend` both implement `VirtioBackend`. The MMIO register
handling stays identical — only `device_id()` and `process_queue()` differ.

---

## 6. Syscall Changes

### 6.1 Console Write Path

**Before (debugcon):**
```rust
FdKind::Console => {
    for i in 0..count {
        let byte = unsafe { read_volatile((buf_vaddr + i) as *const u8) };
        debugcon_write_byte(byte);
    }
    count as SyscallResult
}
```

**After (virtio console):**
```rust
FdKind::Console => {
    let console = crate::VIRTIO_CONSOLE.get().unwrap();
    // Copy from user buffer to kernel bounce buffer, then virtio write.
    // Handle page-boundary crossing via chunked copy.
    let mut total = 0;
    while total < count {
        let chunk = (count - total).min(PAGE_SIZE);
        // Copy user data into console bounce buffer
        // ... (page-aware copy) ...
        total += console.write(&data[..chunk]);
    }
    total as SyscallResult
}
```

Same pattern applies to `sys_writev()`.

### 6.2 Console Read Path

**Before:**
```rust
FdKind::Console => 0, // No stdin
```

**After:**
```rust
FdKind::Console => {
    let console = crate::VIRTIO_CONSOLE.get().unwrap();
    // Read from virtio console into user buffer
    // ... (page-aware copy from bounce buffer to user pages) ...
    console.read(&mut buf[..count]) as SyscallResult
}
```

### 6.3 kprintln! — No Change

`kprintln!` stays on debugcon (port 0xE9). It must work before virtio console is
initialized (early boot, page table setup) and during panics when virtio state may
be corrupted.

---

## 7. DeviceRegistry Changes

### 7.1 Layout Constants

```rust
// sumi-abi/src/arch/x86_64/layout.rs

pub const VIRTIO_CONSOLE_MMIO: PhysicalAddr =
    PhysicalAddr::new(VIRTIO_MMIO_BASE.as_u64() + VIRTIO_MMIO_STRIDE as u64);
```

### 7.2 Device Registration

```rust
// sumi-vm/src/devices/mod.rs

impl DeviceRegistry {
    pub fn new(share_dir: Option<&Path>, dax_host_ptr: *mut u8) -> Self {
        let mut devices = Vec::new();

        if let Some(dir) = share_dir {
            let fs_device = VirtioMmioDevice::new_fs(dir, dax_host_ptr);
            devices.push((VIRTIO_MMIO_BASE.as_u64(), fs_device));
        }

        // Console is always present
        let console_device = VirtioMmioDevice::new_console();
        devices.push((VIRTIO_CONSOLE_MMIO.as_u64(), console_device));

        Self { devices }
    }
}
```

---

## 8. Implementation Plan

### Phase 1: VirtioBackend Trait Refactor

Generalize `VirtioMmioDevice` to support multiple device types via a `VirtioBackend`
trait. `VirtioFs` implements the trait. No behavior change — pure refactor.

**Files:**
- `sumi-vm/src/devices/virtio_mmio.rs` — extract trait, make `backend` generic
- `sumi-vm/src/devices/virtio_fs.rs` — implement `VirtioBackend`
- `sumi-vm/src/devices/mod.rs` — adjust construction

**Verify:** `make self-test` passes, all existing virtio-fs tests green.

### Phase 2: Console Backend (VM-side)

Implement `VirtioConsoleBackend` and register it in `DeviceRegistry`.

**Files:**
- `sumi-abi/src/virtio.rs` — add `VIRTIO_DEVICE_CONSOLE = 3`
- `sumi-abi/src/arch/x86_64/layout.rs` — add `VIRTIO_CONSOLE_MMIO`
- `sumi-vm/src/devices/virtio_console.rs` — new: `VirtioConsoleBackend`
- `sumi-vm/src/devices/mod.rs` — register console device

**Verify:** `cargo build -p sumi-vm` succeeds. Console MMIO region responds to reads.

### Phase 3: Kernel Console Driver

Implement `VirtioConsole` with transmit and receive.

**Files:**
- `sumi-kernel/src/drivers/virtio/console.rs` — new: `VirtioConsole`
- `sumi-kernel/src/drivers/virtio/mod.rs` — add `pub mod console`
- `sumi-kernel/src/lib.rs` — add `VIRTIO_CONSOLE` global
- `sumi-kernel/src/kernel_main.rs` — call `VirtioConsole::init()`

**Verify:** `cargo build -p sumi-kernel --target x86_64-unknown-none` links.

### Phase 4: Rewire Syscalls

Replace debugcon path in `sys_write`, `sys_writev`, `sys_read`, `sys_readv` with
virtio console calls.

**Files:**
- `sumi-kernel/src/syscall/handlers/io.rs` — update Console arms

**Verify:** `make self-test` passes. `write(1, "hello", 5)` goes through virtio.

### Phase 5: Tests

- Selftest: `virtio_console_write` — write a string, verify it arrives on host stdout.
- Selftest: `virtio_console_read` — host sends bytes on stdin, guest reads them.
- Selftest: `write_syscall_console` — `write(1, ...)` returns correct byte count.
- Selftest: `read_syscall_stdin` — `read(0, ...)` returns host-provided data.

---

## 9. Performance

**Before (debugcon):** 100-byte write = 100 VM exits = ~100 * 5us = 500us.

**After (virtio console):** 100-byte write = 1 MMIO write (QueueNotify) = 1 VM exit = ~5us.
Plus memcpy into bounce buffer (~negligible for typical write sizes).

For a 4KB write: 1 VM exit instead of 4096. **~4000x fewer exits.**

The bounce buffer adds one memcpy but eliminates the per-byte MMIO overhead. For writes
smaller than a cache line this is net-zero; for larger writes the batching dominates.

---

## 10. Module Layout (Final State)

### Kernel

```
sumi-kernel/src/
├── drivers/
│   └── virtio/
│       ├── mod.rs
│       ├── mmio.rs           MMIO register access (unchanged)
│       ├── virtqueue.rs      Split virtqueue (unchanged)
│       └── console.rs        NEW: VirtioConsole — init, write, read
├── fs/
│   └── mod.rs                FdKind::Console (unchanged)
├── syscall/
│   └── handlers/
│       └── io.rs             Console arms → virtio console
├── arch/x86_64/
│   ├── debugcon.rs           kprintln! (unchanged, early-boot only)
│   └── mod.rs                debugcon_write_byte (unchanged)
└── lib.rs                    + VIRTIO_CONSOLE global
```

### VM

```
sumi-vm/src/
├── devices/
│   ├── mod.rs                DeviceRegistry + console registration
│   ├── virtio_mmio.rs        VirtioBackend trait + generic device
│   ├── virtio_fs.rs          impl VirtioBackend for VirtioFs
│   └── virtio_console.rs     NEW: impl VirtioBackend for VirtioConsoleBackend
```

### ABI

```
sumi-abi/src/
├── virtio.rs                 + VIRTIO_DEVICE_CONSOLE = 3
└── arch/x86_64/layout.rs     + VIRTIO_CONSOLE_MMIO
```

---

## 11. Open Questions

1. **Stdin blocking behavior.** When the guest calls `read(0, ...)` and stdin has no
   data, should the VM block the vCPU (waiting for host stdin) or return 0/EAGAIN?
   Current plan: block (simpler, single-threaded guest). If non-blocking is needed
   later, the guest can check `O_NONBLOCK` on fd 0.

2. **Bounce buffer size.** One 2 MB page is generous. Could use a smaller allocation
   (e.g., 64 KB from `KernelAllocator`) if memory pressure is a concern. 2 MB aligns
   with the page allocator's granularity and avoids fragmentation.

3. **stderr vs stdout.** Both fd 1 and fd 2 currently map to `FdKind::Console`.
   With virtio console, both go through the same transmitq → host stdout. To separate
   them, we'd need a second virtio-console device or a multiport setup. Out of scope
   for now — programs that need stderr separation can redirect on the host side.
