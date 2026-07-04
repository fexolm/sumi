# virtio-fs

Status: current implementation notes, synced with the codebase on 2026-07-04.

`sumi` exposes a host directory to the guest through a small virtio-mmio
virtio-fs/FUSE implementation.

## Code Map

- Host backend: `sumi-vm/src/devices/virtio_fs.rs`.
- Generic virtio-mmio device wrapper: `sumi-vm/src/devices/virtio_mmio.rs`.
- Guest driver: `sumi-kernel/src/fs/virtio_fs.rs`.
- FUSE ABI structs/constants: `sumi-abi/src/fuse.rs`.
- Virtio ABI structs/constants: `sumi-abi/src/virtio.rs`.
- FD table and descriptor kinds: `sumi-kernel/src/fs/mod.rs`.

## Device Shape

- Device 0 in the MMIO device band is virtio-fs.
- Queue 1 is the request queue used by the guest driver.
- Queue submission is synchronous: guest kicks, the VM handles the FUSE request
  during the MMIO exit, writes the used ring, and returns to the guest.
- The whole device registry is currently protected by one host mutex.

## Supported FUSE Operations

Implemented operations include:

- `INIT`;
- `LOOKUP` / `FORGET`;
- `GETATTR`;
- `OPEN` / `OPENDIR`;
- `READ` / `READDIR`;
- `WRITE`;
- `CREATE`;
- `RELEASE` / `RELEASEDIR`;
- `SETUPMAPPING` / `REMOVEMAPPING` for DAX-backed file mappings.

Unsupported FUSE opcodes return `ENOSYS`.

## Guest Syscall Usage

The guest syscall layer uses virtio-fs for:

- path lookup and metadata;
- regular file read/write;
- directory iteration;
- file creation through supported open/create paths;
- loading ELF binaries and interpreters;
- file-backed `mmap` private-copy and DAX paths.

`VirtioFsClient::v2p` converts kernel, direct-map, and user virtual addresses to
guest physical addresses before descriptors are submitted.

## Limits

- No asynchronous virtqueue processing.
- No permission model beyond host filesystem results.
- Many mutation operations beyond the currently tested create/write paths are
  absent or stubs.
- File handles and node IDs are VM-local bookkeeping, not a complete FUSE cache.
- Host path behavior depends on the selected `--share` root.

## Tests

Coverage comes from syscall integration tests for file I/O, directories,
metadata, dynamic linking, and glibc file APIs.
