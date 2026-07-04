# Dynamic Linking

Status: current implementation notes, synced with the codebase on 2026-07-04.

Dynamic linking is handled by loading the program and its ELF interpreter, then
letting the interpreter run normally inside the guest. `sumi` does not implement
its own userspace linker.

## Loader Behavior

`sumi-kernel/src/exec.rs` is the entry point.

- `ET_EXEC` main binaries load at their linked virtual addresses.
- `ET_DYN` main binaries load at `PIE_LOAD_BASE`.
- If `PT_INTERP` is present, the interpreter is read from virtio-fs and loaded
  at `INTERP_LOAD_BASE`.
- Control transfers to the interpreter entry when present; otherwise to the
  main program entry.
- `AT_BASE` is the interpreter base, or 0 for static binaries.
- `AT_ENTRY` always points to the main binary entry.
- `AT_PHDR`, `AT_PHENT`, and `AT_PHNUM` describe the main binary.

Segments are mapped eagerly on 2 MB pages. BSS is zeroed by allocating zeroed
pages before copying file contents.

## mmap Support For ld.so

The dynamic linker uses `mmap` for library reservations and segment placement.
The kernel supports the patterns currently needed by glibc:

- anonymous mappings from the downward-growing mmap area;
- file-backed `MAP_PRIVATE`;
- file-backed `MAP_PRIVATE | PROT_WRITE` through private copy;
- file-backed read-only/private and `MAP_SHARED` through DAX when safe;
- file-backed `MAP_FIXED` into an exact address range;
- anonymous `MAP_FIXED` to zero BSS tails at 4 KB-aligned addresses;
- `munmap` for whole or partial VMA teardown;
- `mprotect(PROT_NONE)` / restore-present at 2 MB granularity.

See `docs/dax-mmap-design.md` for the DAX and VMA details.

## Filesystem Contract

All interpreter and shared-library paths resolve through the virtio-fs share
root. With `--share /`, normal absolute host paths work. With a narrower share
root, the interpreter path and libraries must exist under that root with the
same absolute-looking layout from the guest's perspective.

`envp` is currently empty, so environment-driven linker behavior such as
`LD_LIBRARY_PATH` is not available unless the loader's default search paths find
the libraries.

## Debug/Perf Metadata

`sumi-vm` parses the kernel, main user binary, and interpreter at startup and
writes `/tmp/perf-<pid>.map`. In `--gdb` mode it also resolves the user and
interpreter symbol files so GDB can load them.

## Limits

- No lazy demand paging; all segments are mapped eagerly.
- Page permissions are coarse because the guest page table uses 2 MB pages.
- `dlopen` may work for simple cases, but there is no dedicated test coverage
  or loader-specific state tracking beyond the syscall surface above.
- `mremap`, `msync`, and `mincore` are not implemented.
- `AT_RANDOM` comes from the host-provided boot seed; this is not a
  multi-tenant security boundary.

## Tests

Relevant tests live under `sumi-integration-tests/data/glibc/`, with
`dynamic_hello.c` as the basic dynamic-linking smoke test.
