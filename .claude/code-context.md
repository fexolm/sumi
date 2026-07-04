# sumi Code Context

This file is the compact code map for agents. Read it before scanning the tree.
Use the linked source files as the source of truth when changing behavior.

## Project Shape

sumi is a Rust workspace for a KVM-hosted unikernel that runs Linux x86_64 ELF
binaries. The guest has one shared address space: there is no process isolation.
Threads are scheduled M:N over a fixed set of KVM vCPUs.

Workspace crates:

- `sumi-vm`: host-side KVM hypervisor, loader, devices, debug support. Target:
  `x86_64-unknown-linux-gnu`.
- `sumi-kernel`: bare-metal unikernel. Target: `x86_64-unknown-none`.
- `sumi-abi`: shared `no_std` ABI types and layout constants used by host and
  kernel.
- `sumi-integration-tests`: end-to-end harness. `build.rs` compiles one user
  test binary per file under `data/`.

## Build And Run

Common commands:

```bash
make build
make clippy
make test
make integration-test
make all
```

Focused commands:

```bash
cargo build -p sumi-kernel --target x86_64-unknown-none
cargo build -p sumi-vm
cargo test -p sumi-abi
cargo test -p sumi-vm
cargo test -p sumi-kernel
cargo test -p sumi-integration-tests
```

Run a guest program:

```bash
cargo run -p sumi-vm -- run target/x86_64-unknown-none/debug/sumi-kernel --share / --run /host/path/to/program
```

`sumi-vm run` arguments live in `sumi-vm/src/cmd/run.rs`:

- Positional `KERNEL`: kernel ELF path.
- `--share DIR`: host directory exposed as guest root. Default is `/`.
- `--run PATH`: user program path as seen through the share root.
- `--vcpus N`: 1..64, defaults to `num_cpus::get().clamp(1, 64)`.
- `--gdb PORT`: starts the GDB stub and forces one vCPU.
- Memory size is currently 2 GiB in `RunCommand::execute`.

Integration tests require `/dev/kvm`; C/glibc tests require `gcc`; Rust std
tests require the `x86_64-unknown-linux-musl` target.

## Runtime Flow

Host VM flow:

1. `sumi-vm/src/main.rs` parses CLI and calls `cmd::Command::execute`.
2. `sumi-vm/src/vm.rs` creates guest memory, initializes KVM, devices, vCPUs,
   loads the kernel ELF, writes boot info, and runs vCPU threads.
3. Devices are registered in `sumi-vm/src/devices/mod.rs`: virtio-fs is present
   when `share_dir` exists, virtio-console is always present.
4. Hypercalls are MMIO writes decoded by `HypercallContext` in
   `sumi-vm/src/vm.rs`. Shutdown stores the exit code, sets the shutdown flag,
   and signals peer vCPUs with `SIGUSR1`.

Kernel boot flow:

1. `_start` is in `sumi-kernel/src/kernel_main.rs`.
2. BSP initializes per-CPU state and syscall MSRs, then TSS, IDT, LAPIC.
3. FD defaults, virtio-fs, and virtio-console are initialized.
4. `exec::read_boot_info` initializes time/RNG and reads the optional run path.
5. `sched::init_phase3_bsp` registers the BSP main thread and idle thread.
6. `sched::KERNEL_READY.store(true, Release)` releases APs.
7. If `--run` was supplied, `exec::exec_user_program` loads and jumps to it.
   Otherwise the kernel halts.

AP flow:

- Entry is `sumi-kernel/src/arch/x86_64/smp.rs::ap_main_rust`.
- APs initialize per-CPU state, syscall MSRs, TSS/IDT/LAPIC, print
  `[ap] cpu N online`, wait on `KERNEL_READY`, then enter scheduler idle.

User program flow:

- `sumi-kernel/src/exec.rs` reads the file via virtio-fs, parses ELF with
  `goblin`, supports x86_64 `ET_EXEC` and `ET_DYN`.
- Main PIE binaries load at `PIE_LOAD_BASE`.
- Dynamic linkers from `PT_INTERP` load at `INTERP_LOAD_BASE`.
- PT_LOAD segments are mapped with 2 MiB pages in `KERNEL_PAGE_TABLE`.
- Initial stack includes argv, envp terminator, and auxv entries.
- `brk` state is initialized in `MEMORY_STATE`.
- `jump_to_user_asm` enables interrupts and jumps to the user entry.

Syscall flow:

- Entry assembly is `sumi-kernel/src/arch/x86_64/syscall.rs`.
- `SyscallArgs` layout is hard-coded by assembly; do not reorder fields in
  `sumi-kernel/src/syscall/mod.rs`.
- `syscall_dispatch` routes Linux syscall numbers to
  `sumi-kernel/src/syscall/handlers/*`.
- Return-side hooks reload stale TLBs and schedule if `need_resched` was set.
- Syscall errors are negative Linux errno values from `syscall/errno.rs`.

## Important Layouts

Core constants:

- `sumi-abi/src/layout.rs`
- `sumi-abi/src/arch/x86_64/layout.rs`

Key values:

- `PAGE_SIZE` is 2 MiB.
- Kernel physical base: `KERNEL_CODE_PHYS = 0`.
- Kernel virtual base: `KERNEL_CODE_VIRT = 0xFFFF_FFFF_8000_0000`.
- Kernel code region size: `KERNEL_CODE_SIZE = 2 GiB`.
- Direct map base: `DIRECT_MAP_OFFSET = 0xFFFF_8880_0000_0000`.
- Max tracked guest RAM: `MAX_GUEST_MEMORY = 2 TiB`.
- User stack top: `USER_STACK_TOP = 0x0000_7FFF_FFFF_F000`.
- User stack size: 8 MiB.
- User mmap base: `USER_MMAP_BASE = 0x0000_7FFF_0000_0000`.
- PIE load base: `0x0040_0000`.
- Interpreter load base: `0x7f00_0000_0000`.
- LAPIC physical frame: `0xFEE0_0000`, reserved from the page allocator.
- Virtio MMIO base: `0x10_0000_0000`, stride 4 KiB.
- DAX window base: `0x20_0000_0000`, size 128 GiB, 2 MiB slots.
- Hypercall MMIO base: `0x1_0000_2000`, intentionally outside guest RAM and
  below virtio MMIO.

If guest memory size grows beyond the current 2 GiB, re-check
`HYPERCALL_MMIO_BASE`, KVM memslot ranges, and the compile-time assertions in
the layout module.

## Source Map

Shared ABI:

- `sumi-abi/src/address.rs`: `PhysicalAddr`, `VirtualAddr`, `DirectMap`.
- `sumi-abi/src/boot_info.rs`: host-to-kernel boot info.
- `sumi-abi/src/hypercall.rs`: hypercall MMIO offsets and size.
- `sumi-abi/src/virtio.rs`, `fuse.rs`, `stat.rs`: device and filesystem ABI.

Host VM:

- `sumi-vm/src/vm.rs`: core VM lifecycle, ELF loading, vCPU run loop,
  hypercall decode/apply, GDB metadata.
- `sumi-vm/src/arch/x86_64/kvm/`: KVM backend, vCPU setup, CPUID masking.
- `sumi-vm/src/devices/virtio_mmio.rs`: generic virtio-mmio transport.
- `sumi-vm/src/devices/virtio_fs.rs`: host-side virtio-fs/FUSE backend and DAX.
- `sumi-vm/src/devices/virtio_console.rs`: host-side console backend.
- `sumi-vm/src/debug/`: GDB remote protocol and breakpoints.

Kernel globals:

- `sumi-kernel/src/lib.rs`: global allocators, page table, FD table, virtio
  singletons, RNG seed, TLB generation, memory state, VMA table, DAX allocator.
- `sumi-kernel/src/kernel_main.rs`: production `_start`, global allocator,
  panic handler.
- `sumi-kernel/src/main.rs`: `no_std`/`no_main` gate and host-test main.

Kernel arch:

- `sumi-kernel/src/arch/x86_64/pagetable.rs`: page-table mapping and direct
  map helpers.
- `sumi-kernel/src/arch/x86_64/syscall.rs`: syscall MSRs and entry assembly.
- `sumi-kernel/src/arch/x86_64/switch.rs`: context switch assembly.
- `sumi-kernel/src/arch/x86_64/smp.rs`: AP Rust entry.
- `sumi-kernel/src/arch/x86_64/idt.rs`, `interrupt.rs`, `lapic.rs`, `tss.rs`:
  interrupts, timer, and IST/TSS setup.
- `sumi-kernel/src/arch/x86_64/hypercall.rs`: guest-side hypercall MMIO writes.
- `sumi-kernel/src/arch/x86_64/debugcon.rs`: debug output path used by
  `kprintln!`.

Memory:

- `sumi-kernel/src/memory/alloc/palloc.rs`: bitmap 2 MiB page allocator;
  reserves all pre-kernel-heap pages and the LAPIC hole.
- `sumi-kernel/src/memory/alloc/kmalloc.rs`: freelist allocator backed by
  `PageAllocator`; max single allocation is 16 MiB.
- `sumi-kernel/src/memory/vma.rs`: simple VMA table for anonymous, DAX, and
  private file mappings.
- `sumi-kernel/src/memory/errors.rs`: memory error types.

Filesystem and devices:

- `sumi-kernel/src/fs/mod.rs`: FD table; fd 0..2 are console by default.
- `sumi-kernel/src/fs/virtio_fs.rs`: guest-side virtio-fs client.
- `sumi-kernel/src/fs/dax.rs`: bitmap allocator for DAX slots.
- `sumi-kernel/src/drivers/virtio/`: guest-side virtio MMIO, virtqueue,
  console.

Scheduler:

- `sumi-kernel/src/sched/mod.rs`: scheduler entry points and invariants.
- `sumi-kernel/src/sched/percpu.rs`: per-vCPU state and GS offsets.
- `sumi-kernel/src/sched/thread.rs`: thread structs, states, contexts.
- `sumi-kernel/src/sched/runqueue.rs`: runnable queues.
- `sumi-kernel/src/sched/registry.rs`: global thread registry and live thread
  counters.
- `sumi-kernel/src/sched/kthread.rs`: main and idle thread construction.
- `sumi-kernel/src/sched/clone.rs`: clone thread setup.
- `sumi-kernel/src/sched/futex.rs`: futex wait/wake.
- `sumi-kernel/src/sched/irq.rs`: interrupt/preemption helpers.
- `sumi-kernel/src/sched/reaper.rs`: zombie thread cleanup.

Syscall handlers:

- `handlers/io.rs`: read/write/open/close/lseek/poll/ioctl/fcntl/vectored IO,
  fd duplication, pipe/select stubs.
- `handlers/fs.rs`: stat, access, getdents, cwd, directory and path syscalls.
- `handlers/memory/`: mmap, mprotect, munmap, brk, mremap/msync/mincore/madvise.
- `handlers/process.rs`: ids, exit/exit_group, uname, prctl, arch_prctl.
- `handlers/clone.rs`: clone/clone3.
- `handlers/thread.rs`: sched_yield, futex, robust-list stub.
- `handlers/signal.rs`: signal compatibility stubs.
- `handlers/time.rs`: nanosleep, clock/gettimeofday/rlimit.
- `handlers/random.rs`: getrandom.
- `handlers/net.rs`: socket API stubs.

## Testing Patterns

Host unit tests:

- Kernel tests run on the host under `#[cfg(test)]`; `std` is available there.
- Many subsystem tests live in side files via `#[cfg(test)] #[path = "..."]`.
- Memory tests use direct-map test doubles; follow `kmalloc_test.rs` and
  `vma_test.rs` before adding allocator or VMA tests.

Integration tests:

- Harness crate: `sumi-integration-tests`.
- Build script: `sumi-integration-tests/build.rs`.
- Runtime helpers: `sumi-integration-tests/src/lib.rs`.
- Launcher: `sumi-integration-tests/tests/test_launcher.rs`.

Test categories:

- `data/syscalls/*.rs`: single-file `no_std` Rust programs. They include
  `../common.rs`, define `sumi_main`, and exit through `pass!()` or `check!()`.
- `data/glibc/*.c`: host glibc-linked C programs, compiled with
  `gcc -O2 -march=x86-64-v2 -lm -lpthread`.
- `data/rust_std/*.rs`: Rust std programs, compiled for static musl
  `x86_64-unknown-linux-musl` with `-no-pie`.

`build.rs` auto-generates one `#[test]` per test program unless the file is
listed in a manual-test array. Use manual tests in `test_launcher.rs` for
non-zero exit codes or special vCPU counts.

The harness runs `sumi-vm run <kernel> --share / --run <compiled-test>` and
scans stdout for the last `[exit] code=N` emitted by the kernel. SMP helpers
also require every AP to print `[ap] cpu N online`.

## Design Docs

- `docs/user-program-design.md`: boot info, ELF loading, stack setup.
- `docs/syscall-design.md`: syscall entry, dispatch, return hooks.
- `docs/multithreading.md`: current scheduler/threading model.
- `docs/glibc-support-design.md`: glibc and pthread compatibility.
- `docs/dynamic-linking-design.md`: PT_INTERP and dynamic linker behavior.
- `docs/virtio-fs-design.md`: virtio-fs/FUSE contract.
- `docs/virtio-console-design.md`: console device.
- `docs/dax-mmap-design.md`: DAX-backed file mmap.
- `docs/debugging-profiling-design.md`: GDB stub and perf notes.

## Invariants And Gotchas

- `PAGE_SIZE` means 2 MiB in this project. Do not assume 4 KiB pages in kernel
  memory-management code.
- `SyscallArgs` field offsets are asserted because assembly depends on them.
- `KERNEL_READY` is the BSP-to-AP publication barrier. APs must not observe
  partially initialized global kernel state.
- The AP online print happens before `KERNEL_READY` and intentionally uses
  debugcon, not virtio-console.
- `KERNEL_PAGE_TABLE` is global. User mappings, DAX mappings, brk, mmap, and
  TLB generation changes must remain coherent across vCPUs.
- `TLB_GENERATION` is checked on syscall return and timer/preemption paths.
- The LAPIC physical frame is reserved from `PageAllocator` because KVM must
  trap it as LAPIC MMIO, not back it with guest RAM.
- Hypercall MMIO must stay outside all KVM memory slots and outside virtio MMIO.
- `--gdb` is BSP-only and forces one vCPU.
- glibc C tests are capped to `x86-64-v2`; CPUID masking keeps higher features
  such as AVX unavailable.
- `FdTable::alloc` returns the lowest available fd, matching Linux.
- `PageAllocator::free` frees one 2 MiB page at a time.
- `KernelAllocator::calloc` zeroes the requested byte count, not necessarily the
  whole backing block.
- Do not introduce production kernel panics for ordinary guest/user errors; use
  negative errno or subsystem `Result` values.
