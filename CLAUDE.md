# sumi

Unikernel that runs Linux ELF binaries. No context switching, no process isolation — everything runs in kernel space as a single process.

## Architecture

| Crate | Role | Target |
|-------|------|--------|
| `sumi-vm` | KVM hypervisor / loader | `x86_64-unknown-linux-gnu` |
| `sumi-kernel` | The unikernel itself | `x86_64-unknown-none` (bare-metal) |
| `sumi-abi` | Shared types between loader and kernel | `no_std`, both targets |
| `sumi-integration-tests` | End-to-end test runner; one test program per file in `data/` | host (build.rs cross-compiles each test) |

### Build & Test

```bash
# Build kernel + VM
make build

# Host-side unit tests for every crate
make test

# End-to-end tests: each binary in sumi-integration-tests/data/{syscalls,glibc}/
# is built and executed inside sumi under KVM (requires /dev/kvm and gcc).
make integration-test
```

The integration test framework lives in `sumi-integration-tests/`. Each
`data/syscalls/<name>.rs` is a single-file `no_std` Rust program that uses
raw syscalls (via `include!("../common.rs")`) to exercise one kernel feature.
Each `data/glibc/<name>.c` is a glibc-linked C program that exercises the
dynamic linker / libc surface. `build.rs` compiles each file into a binary
and emits one `#[test]` per binary; the harness in `tests/test_launcher.rs`
runs each binary inside `sumi-vm` and asserts that the kernel printed
`[exit] code=0`. To add a new test, drop a new file into `data/syscalls/`
or `data/glibc/`.

## Coding Standards

This is a bare-metal systems project. Every decision matters.

### Performance First
- Minimize allocations. Prefer static/stack when possible.
- Avoid unnecessary abstractions — a direct function call is better than a trait object.
- Think about cache lines, alignment, and memory layout.
- Use `#[inline(always)]` only for genuinely hot paths in arch-specific code.
- Prefer `spin::Mutex` over complex lock-free structures unless profiling proves otherwise.

### Simplicity
- No frameworks, no macros where a function suffices.
- Files under 500 lines. Split when logic is distinct, not for aesthetics.
- No premature abstraction — write the concrete case first, generalize only when there are 3+ users.
- If a comment explains *what* the code does, the code is too complex. Comments explain *why*.

### Safety
- Minimize `unsafe` surface. Every `unsafe` block must have a `// SAFETY:` comment.
- Validate at system boundaries (ELF parsing, hypercalls, hardware registers).
- Internal invariants are enforced by types, not runtime checks.
- No panics in kernel code paths — return `Result` or use `debug_assert!`.

### Rust Specifics
- Edition 2024, resolver 3.
- `#![no_std]` for kernel and abi crates; `#[cfg(test)]` enables std for tests.
- Use `PhysicalAddr` / `VirtualAddr` — never raw `usize` for addresses in public APIs.
- Page table operations go through `DirectMap` trait for testability.

### Testing
- Kernel code is testable on the host via `#[cfg(test)]` with mock `DirectMap`.
- Every allocator/memory subsystem change must have unit tests.
- Tests must verify edge cases: zero-size, max-size, alignment, double-free, concurrent access.
- Use `TestDirectMap` pattern from `kmalloc.rs` for memory subsystem tests.

### Delivery Checklist
Every implementation MUST pass before being presented as complete:
1. `cargo test` — all unit tests pass.
2. `cargo build -p sumi-kernel --target x86_64-unknown-none` — bare-metal kernel links.
3. `cargo build -p sumi-vm` — VM host binary builds.
4. KVM smoke test: `sumi-vm run <kernel>` — kernel boots and exits cleanly.
5. KVM integration test: if a new device/subsystem was added, run `sumi-vm run --share <dir> <kernel>` (or equivalent) and verify the init path succeeds.
If any step fails, fix it before reporting done. Never present a broken build.

## Multi-Agent Development Workflow

This project uses a 4-agent workflow to ensure quality. Use `/develop` to run the full pipeline, or individual commands for specific phases.

### Agents

| Agent | Model | Role |
|-------|-------|------|
| Architect | opus | Designs approach, defines interfaces, considers trade-offs |
| Implementor | sonnet | Writes the actual code following architect's plan |
| Reviewer | opus | Critically reviews all decisions and code, must be convinced |
| Tester | sonnet | Writes unit tests, runs them, verifies correctness |

### Commands

- `/develop <task>` — Full pipeline: architect -> implement -> review -> fix -> test
- `/architect <task>` — Architecture/design analysis only
- `/review` — Critical review of current changes
- `/test` — Write tests and verify current changes

### Workflow Rules
1. Architect plans before code is written.
2. Implementor follows the plan precisely.
3. Reviewer challenges everything — assumptions, performance, safety, edge cases.
4. If reviewer finds issues, implementor must fix them.
5. Tester writes tests that cover the implementation AND the reviewer's concerns.
6. No code is considered done until tests pass.

## Memory Layout (x86_64)

```
0x0000_0000_0000_0000  Kernel code (physical, loaded by ELF loader)
                        ... page tables, stack ...
KERNEL_STACK            First allocatable page (palloc starts here)
                        ... physical memory ...
0x0000_7FFF_FFFF_FFFF  MAX_PHYSICAL_ADDR (128 TB)

0xFFFF_8880_0000_0000  DIRECT_MAP_OFFSET (virtual, identity map of all physical memory)
0xFFFF_FFFF_8000_0000  KERNEL_CODE_VIRT (virtual, 2GB kernel code region)
```

## Key Abstractions

- `DirectMap` trait — translates physical <-> virtual addresses; real impl uses offset, tests use buffer
- `PageAllocator` — bitmap-based page allocator, 2MB pages
- `KernelAllocator` — freelist-based sub-page allocator built on top of PageAllocator
- `RootPageTable` — 3-level page table (PML4 -> PDPT -> PD) with 2MB huge pages
- `VirtBackend` / `VCpu` traits — hypervisor abstraction (KVM impl)
