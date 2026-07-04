# Syscall Subsystem

Status: current implementation notes, synced with the codebase on 2026-07-04.

`sumi` uses the Linux x86_64 syscall ABI so libc and raw Linux-style test
programs can run unchanged where the syscall subset is implemented.

## Entry Path

Code:

- `sumi-kernel/src/arch/x86_64/syscall.rs`: MSR setup and assembly entry.
- `sumi-kernel/src/syscall/mod.rs`: `SyscallArgs` and dispatcher.
- `sumi-kernel/src/syscall/handlers/`: syscall families.

The guest runs everything in ring 0, but the syscall instruction is still used
as an ABI boundary. Entry saves user registers on a per-CPU syscall stack, builds
`SyscallArgs`, calls `syscall_dispatch`, then returns to the saved user RIP.

`SyscallArgs` stores:

- syscall number;
- six Linux ABI arguments;
- caller RIP from `rcx`;
- caller RFLAGS from `r11`.

The extra RIP/RFLAGS fields are needed by `clone` so the child can return from
the same logical syscall site.

## Dispatch

`syscall_dispatch` matches Linux syscall numbers and routes to handler modules:

- `io.rs`: file descriptors, console, read/write, vectors, dup/fcntl.
- `fs.rs`: metadata, directory, path, and host filesystem syscalls.
- `memory/`: `mmap`, `munmap`, `mprotect`, `brk`, advisory stubs.
- `process.rs`: IDs, exit, uname, TLS, prctl, tid address.
- `thread.rs`: futex, sched_yield, robust-list storage.
- `clone.rs`: `clone` and `clone3`.
- `signal.rs`: compatibility stubs; no real signal delivery.
- `time.rs`: clocks, sleep, limits.
- `random.rs`: `getrandom`.
- `net.rs`: networking stubs.

Unhandled syscalls print a log line and return `-ENOSYS`.

## Return-Side Hooks

After a handler returns, the dispatcher performs two scheduler-related checks:

1. `reload_tlb_if_stale()`: reload CR3 if this CPU has not observed the latest
   global TLB generation after `munmap` or `mprotect`.
2. `need_resched`: if a wakeup or `clone` requested rescheduling, push the
   current non-idle thread back to the runqueue and call `schedule()` before
   returning to user code.

## Error Convention

Handlers return `i64`. Success values are non-negative. Errors are negative
Linux errno values from `sumi-kernel/src/syscall/errno.rs`.

## Pointer Model

Kernel and user code share one address space. Many handlers directly dereference
user pointers after simple null/alignment checks. A bad pointer may fault the
kernel; this is accepted by the current unikernel trust model.

## Current Compatibility Notes

- The syscall surface is shaped around dynamic glibc, pthreads, and the
  integration tests.
- Signal handlers can be registered but are not delivered.
- Network syscalls are stubs.
- `rseq` returns `ENOSYS`, which glibc tolerates.
- Page permissions are only partially modeled due to 2 MB pages.
