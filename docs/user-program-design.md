# User Program Execution

Status: current implementation notes, synced with the codebase on 2026-07-04.

`sumi-vm run` loads the kernel and optionally passes one user program path to
the guest. The guest reads that program through virtio-fs, maps it into the
shared address space, builds an initial Linux-like stack, and jumps to it.

## Host Interface

```bash
cargo run -p sumi-vm -- run <kernel-elf> \
  --share /path/to/root \
  --run /path/inside/share \
  --vcpus 4
```

Flags:

- `--share DIR`: host directory exposed as guest root; defaults to `/`.
- `--run PATH`: user program path as seen by the guest.
- `--vcpus N`: fixed vCPU count, default host CPU count clamped to `1..=64`.
- `--gdb PORT`: start the GDB stub and force one vCPU.

## Boot Info

The host writes `BootInfo` at `BOOT_INFO_ADDR`. Version 3 contains:

- memory size;
- optional run path offset/length;
- TSC frequency;
- wall-clock time;
- 32-byte RNG seed;
- total vCPU count.

The kernel reads this before releasing APs with `KERNEL_READY`, because time,
RNG, and scheduler state are global data APs may observe.

## Guest Boot Flow

`sumi-kernel/src/kernel_main.rs`:

1. Initializes per-CPU state for CPU 0 and syscall MSRs.
2. Loads TSS, IDT, and LAPIC timer.
3. Initializes FD defaults, virtio-fs, and virtio-console.
4. Reads `BootInfo`.
5. Registers the BSP main thread and idle thread.
6. Publishes `KERNEL_READY` so APs can enter their idle loops.
7. If `--run` was provided, calls `exec::exec_user_program`.

If no run path is provided, the kernel halts after init.

## ELF Loading

`sumi-kernel/src/exec.rs`:

- reads the file from virtio-fs;
- parses ELF with `goblin`;
- accepts x86_64 `ET_EXEC` and `ET_DYN`;
- loads main `PT_LOAD` segments;
- loads `PT_INTERP` when present;
- builds auxv and the initial stack;
- sets global `brk` state;
- jumps to the selected entry point with interrupts enabled.

The loader uses the same page table as the kernel. User mappings live in the
lower canonical half; kernel code and the direct map live in the upper half.

## Initial Stack

Current stack shape:

- `argc = 1`;
- `argv[0] = run path`;
- empty `envp`;
- auxv entries for program headers, page size, interpreter base, entry point,
  IDs, `AT_RANDOM`, `AT_SECURE`, and `AT_HWCAP`.

The user stack is 8 MB ending at `USER_STACK_TOP`.

## Memory Layout Notes

- Page allocation and page-table mappings are 2 MB-granular.
- `mmap` grows downward from `USER_MMAP_BASE`.
- `brk` starts at the aligned end of the loaded main program.
- PIE main binaries use `PIE_LOAD_BASE`.
- The dynamic linker uses `INTERP_LOAD_BASE`.

## Limits

- There is one shared address space; there is no user/kernel privilege boundary.
- There is no `execve`; the initial program is selected by boot info.
- Host environment variables are not passed to the guest.
- Page protections are coarse because mappings are 2 MB pages.
- A bad user pointer can still fault kernel code under the unikernel trust model.

## Tests

Integration tests under `sumi-integration-tests/data/syscalls/`,
`data/glibc/`, and `data/rust_std/` build user programs and run them under KVM.
