# Debugging And Profiling

Status: current implementation notes, synced with the codebase on 2026-07-04.

`sumi-vm` provides a host-side GDB remote stub and always emits a perf symbol
map. The guest kernel does not contain a debugger.

## GDB Mode

Run with:

```bash
cargo run -p sumi-vm -- run target/x86_64-unknown-none/debug/sumi-kernel \
  --share / \
  --run /path/to/program \
  --gdb 1234
```

`--gdb` forces `--vcpus 1`. The current stub controls only vCPU 0 and does not
present multiple vCPUs as debugger threads.

Flow:

1. `sumi-vm` starts one vCPU in debug mode.
2. A GDB server thread listens on the requested TCP port.
3. `sumi-vm` launches GDB with the kernel ELF and user/interpreter symbol info
   when available.
4. Commands are sent over channels to the vCPU thread.
5. KVM guest debug handles software breakpoints and single-step exits.

## Supported RSP Features

The stub in `sumi-vm/src/debug/` supports the basic packet set needed for kernel
and userspace inspection:

- stop reason query;
- read registers and selected registers;
- read/write memory;
- continue;
- single step;
- insert/remove software breakpoint;
- detach/kill;
- minimal thread identity replies for the single-vCPU target.

Register serialization covers general-purpose registers, RIP, EFLAGS, segment
selectors, and zero-filled FP/SSE state.

## Memory Access

Debugger memory reads and writes go through helper code in
`sumi-vm/src/debug/breakpoints.rs`. The helper translates guest virtual
addresses by walking the guest page tables through the current CR3, then reads
or writes guest physical memory in the KVM memory map.

## perf

At VM startup, `sumi-vm` writes:

```text
/tmp/perf-<host-pid>.map
```

The map includes function symbols from:

- the kernel ELF;
- the user binary, when `--run` is set;
- the interpreter named by `PT_INTERP`, when present.

Example:

```bash
perf record -p <sumi-vm-pid> -g
perf script
```

## Limits

- GDB mode is single-vCPU only.
- Hardware breakpoints/watchpoints are not exposed.
- The GDB stub is intentionally small and not a full Linux remote target.
- Debugging shares the same trust model as the rest of `sumi`: all guest code is
  in one address space, so memory writes can corrupt kernel/user state.
