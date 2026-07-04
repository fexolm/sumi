# glibc Support

Status: current implementation notes, synced with the codebase on 2026-07-04.

`sumi` can run dynamically linked x86_64 glibc binaries through the guest ELF
loader, virtio-fs, and the Linux-compatible syscall subset in
`sumi-kernel/src/syscall/handlers/`.

## How To Run

```bash
make build
cargo run -p sumi-vm -- run \
  target/x86_64-unknown-none/debug/sumi-kernel \
  --share / \
  --run /absolute/or/share-relative/program
```

`--share` defaults to `/`, so normal host glibc paths such as
`/lib64/ld-linux-x86-64.so.2` are visible unless a narrower share root is
chosen.

## Loader Contract

The kernel loader in `sumi-kernel/src/exec.rs`:

- reads the requested ELF through virtio-fs;
- accepts `ET_EXEC` and `ET_DYN`;
- loads PIE main binaries at `PIE_LOAD_BASE`;
- loads `PT_INTERP` at `INTERP_LOAD_BASE`;
- maps `PT_LOAD` segments on 2 MB pages;
- prepares `argc = 1`, `argv[0] = run path`, empty `envp`;
- supplies auxv entries needed by glibc: `AT_PHDR`, `AT_PHENT`, `AT_PHNUM`,
  `AT_PAGESZ`, `AT_BASE`, `AT_ENTRY`, IDs, `AT_RANDOM`, `AT_SECURE`, and
  `AT_HWCAP = 0`.

`AT_HWCAP = 0` is deliberate. `sumi-vm` also masks CPUID so glibc stays on a
conservative SSE2-compatible path instead of selecting IFUNC variants that the
kernel has not validated.

## Syscall Surface glibc Relies On

Implemented or compatibility-stubbed paths include:

- File and directory I/O: `openat`, `read`, `write`, `pread64`, `pwrite64`,
  `readv`, `writev`, `close`, `fstat`, `newfstatat`, `getdents64`, `readlink`,
  `lseek`, `access`, `fcntl`, `dup`, `dup2`.
- Memory: `brk`, `mmap`, `munmap`, `mprotect`, `madvise`.
- Time/random: `clock_gettime`, `clock_getres`, `gettimeofday`, `time`,
  `nanosleep`, `getrandom`.
- Process/thread shape: `getpid`, `gettid`, `getuid`, `getgid`, `uname`,
  `prlimit64`, `set_tid_address`, `set_robust_list`, `arch_prctl`.
- pthreads: `clone`, `clone3`, `futex`, `sched_yield`, `exit`, `exit_group`.
- Compatibility stubs: signal registration/masks, `prctl(PR_SET_VMA)`, and
  `rseq` returning `ENOSYS`.

The syscall dispatcher is `sumi-kernel/src/syscall/mod.rs`.

## pthreads

glibc's `pthread_create` uses `clone` or `clone3` with shared-resource flags.
`sumi` implements that thread shape directly:

- all threads share the same address space and file table;
- each thread gets a unique TID and shares the thread-group PID;
- TLS is per-thread through `ARCH_SET_FS` / `CLONE_SETTLS`;
- `pthread_join` is supported through `CLONE_CHILD_CLEARTID` plus futex wake;
- mutexes and condition variables use the scheduler-integrated futex path.

See `docs/design/multithreading-v2.md` for scheduler invariants.

## Limits

- No process isolation: glibc code, user code, and kernel code all run in ring 0.
- `envp` is empty; host environment variables are not propagated.
- Real signal delivery is not implemented.
- `fork`, `vfork`, `execve`, sockets, and most networking syscalls are absent or
  stubs.
- `mremap`, `msync`, and `mincore` return `ENOSYS`.
- `mprotect` works at 2 MB granularity and only toggles page presence for
  `PROT_NONE` versus present.
- Robust-list pointers are stored but not walked on thread exit.

## Tests

glibc integration tests live in `sumi-integration-tests/data/glibc/`, including
dynamic linking smoke tests, libc functions, file I/O, pthread create/join,
mutexes, condvars, TLS keys, and stress cases.
