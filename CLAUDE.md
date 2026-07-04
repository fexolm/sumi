# sumi

sumi is a Rust unikernel that runs Linux x86_64 ELF binaries under KVM in one
shared address space. There is no process isolation; user programs, kernel
services, and threads share the kernel page table.

## First Read

For codebase orientation, file maps, runtime flow, layout constants, and test
recipes, read:

- `.claude/code-context.md`

Then read only the source files relevant to the task.

## Build And Test

The common entry points are in the `Makefile`:

```bash
make build
make clippy
make test
make integration-test
make all
```

Use `.claude/code-context.md` for the exact crate-level commands and
integration-test details.

## Coding Standards

This is a bare-metal systems project. Every decision matters.

### Performance

- Minimize allocations. Prefer static or stack storage when possible.
- Avoid unnecessary abstraction. Use concrete code until a shared abstraction
  removes real complexity.
- Think about cache lines, alignment, memory layout, and hot-path instruction
  count.
- Use `#[inline(always)]` only for genuinely hot arch-specific paths.
- Prefer simple locked data structures unless profiling proves otherwise.

### Simplicity

- No frameworks.
- No macro when a function is enough.
- Keep files under about 500 lines. Split when logic is distinct.
- Comments explain why, not what.

### Safety

- Minimize `unsafe`.
- Every `unsafe` block needs a `// SAFETY:` comment naming the upheld
  precondition.
- Validate at system boundaries: ELF parsing, hypercalls, MMIO, user pointers,
  hardware registers.
- Do not add panics in production kernel paths. Return `Result` or use
  `debug_assert!` for internal invariants.

### Rust

- Workspace edition is 2024, resolver 3.
- `sumi-kernel` and `sumi-abi` are `#![no_std]` outside tests.
- Use `PhysicalAddr` and `VirtualAddr` in public APIs instead of raw `usize`
  addresses.
- Page-table and allocator code should remain host-testable through direct-map
  test doubles.

## Delivery Checklist

Before presenting code as complete, run the checks that match the blast radius:

1. `cargo test` or the focused `cargo test -p <crate>`.
2. `cargo build -p sumi-kernel --target x86_64-unknown-none`.
3. `cargo build -p sumi-vm`.
4. `make clippy` for broad kernel or VM changes.
5. `make integration-test` for syscall, ELF loading, scheduler, virtio, or
   user-program behavior changes when `/dev/kvm` is available.

If a required check cannot run, report that explicitly.

## Multi-Agent Workflow

This repo has Claude command files under `.claude/commands/` and agent prompts
under `.claude/agents/`.

- `/develop <task>`: architect, implement, review, fix loop, test, final verify.
- `/architect <task>`: design only.
- `/review`: review current uncommitted changes.
- `/test`: add and run tests for current changes or a specified area.

For significant kernel work, prefer architect -> implement -> review -> test.
