# DAX Window And File mmap

Status: current implementation notes, synced with the codebase on 2026-07-04.

This document describes the current file-backed `mmap` and DAX path. The code is
split between:

- `sumi-vm/src/arch/x86_64/kvm/mod.rs`: host DAX memslot setup.
- `sumi-vm/src/devices/virtio_fs.rs`: FUSE setup/removal mapping handlers.
- `sumi-kernel/src/fs/dax.rs`: guest DAX slot allocator.
- `sumi-kernel/src/syscall/handlers/memory/mod.rs`: `mmap`, `munmap`,
  `mprotect`, `brk`.
- `sumi-kernel/src/memory/vma.rs`: VMA metadata.

## DAX Window

The host allocates a 128 GB anonymous `MAP_NORESERVE` region and registers it as
a KVM memslot at `DAX_WINDOW_BASE`. The guest treats it as a physical window of
2 MB slots.

The guest `DaxAllocator` tracks contiguous 2 MB slot ranges with a bitmap. A
file mapping requests host setup with `FUSE_SETUPMAPPING`; teardown uses
`FUSE_REMOVEMAPPING`.

## mmap Dispatch

`sys_mmap` rejects zero length with `EINVAL`, rounds internal mappings to 2 MB,
and returns the requested user virtual address.

Current paths:

| Request | Behavior |
|---|---|
| `MAP_ANONYMOUS` | Allocate zeroed 2 MB pages and track an anonymous VMA. |
| File `MAP_PRIVATE | PROT_WRITE` | Allocate private pages and copy file bytes through FUSE reads. |
| File read-only `MAP_PRIVATE` | Try DAX if the mapping is fully covered by file contents; otherwise private copy. |
| File `MAP_SHARED` | Same DAX-first path, with write setup flag. |
| File `MAP_FIXED` | Ensure pages exist and read bytes into the exact requested address. |
| Anonymous `MAP_FIXED` | Zero the exact requested range inside existing or newly allocated 2 MB pages. |

DAX is skipped for partial EOF pages so the host never takes SIGBUS from an
underlying mmap beyond the file length.

## MAP_FIXED And DAX Replacement

The dynamic linker often reserves a range and later maps file segments into
pieces of that range. If a target page is currently DAX-backed, the kernel first
replaces it with a private page, copies the DAX contents, then writes the file
bytes. This prevents a partial segment write from modifying the host file
mapping.

## munmap And mprotect

`munmap` removes whole VMAs when the request covers the full range. Partial
unmaps clear only the requested 2 MB pages and leave surrounding VMA metadata in
place, matching loader gap-teardown patterns.

`mprotect(PROT_NONE)` clears the present bit for covered 2 MB pages. Other
protections restore presence. After `munmap` or `mprotect`, the kernel bumps
`TLB_GENERATION`; each CPU reloads CR3 lazily before returning to user code if
its local generation is stale.

## Limits

- All guest page management is 2 MB-granular, even though user ABI reports
  4 KB pages to libc.
- There is no demand paging or page cache.
- DAX mappings are not deduplicated across independent `mmap` calls.
- `msync`, `mremap`, and `mincore` return `ENOSYS`.
- File truncation while mapped is not modeled.
- Protection bits other than present/not-present are not enforced.

## Tests

Coverage is in host unit tests for the DAX allocator and integration tests such
as `mmap_file_private.rs`, `mmap_munmap_anon.rs`, and glibc dynamic-linking
tests.
