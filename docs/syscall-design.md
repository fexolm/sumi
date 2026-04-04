# Syscall Subsystem — Design Document

## Background

sumi runs Linux ELF binaries in ring-0 kernel space. These binaries make Linux syscalls
via the `syscall` instruction. We must intercept these calls, dispatch to handlers, and
return results using the standard Linux ABI — without any privilege transition.

## Mechanism

### The `syscall` Instruction in a Ring-0 Unikernel

In x86-64, the `syscall` instruction does not require a privilege change. It:

1. Saves `RIP` → `RCX`, `RFLAGS` → `R11`
2. Masks `RFLAGS` with `SFMASK`
3. Loads `LSTAR` → `RIP`, `STAR[47:32]` → `CS`/`SS`

Because sumi runs everything in ring 0, `syscall` works as a fast indirect call: the
program jumps into our handler, and we return by restoring `RFLAGS` from `R11` and
jumping to `RCX`.

**We must NOT use `sysret`** — it unconditionally sets `CPL=3`, which would fault.

### Linux x86-64 Syscall ABI

| Register | Role on entry       | Role on exit  |
|----------|---------------------|---------------|
| `RAX`    | Syscall number      | Return value  |
| `RDI`    | arg0                | (clobbered)   |
| `RSI`    | arg1                | (clobbered)   |
| `RDX`    | arg2                | (clobbered)   |
| `R10`    | arg3 (not `RCX`!)  | (clobbered)   |
| `R8`     | arg4                | (clobbered)   |
| `R9`     | arg5                | (clobbered)   |
| `RCX`    | Saved `RIP`         | Must preserve |
| `R11`    | Saved `RFLAGS`      | Must preserve |

### MSR Setup

| MSR         | Address        | Value                          | Purpose                        |
|-------------|----------------|--------------------------------|--------------------------------|
| `EFER`      | `0xC000_0080`  | set bit 0 (`SCE`)              | Enable `syscall`/`sysret`      |
| `LSTAR`     | `0xC000_0082`  | `&syscall_entry`               | 64-bit handler entry point     |
| `STAR`      | `0xC000_0081`  | `0x0008 << 32`                 | `CS=0x0008`, `SS=0x0010`       |
| `SFMASK`    | `0xC000_0084`  | `IF \| DF` (`0x600`)           | Clear interrupts and direction |

`STAR[47:32] = 0x0008` sets `CS = 0x0008` (kernel code) and `SS = 0x0010` (kernel data)
on syscall entry — consistent with the existing GDT layout from `KvmVCpu::init`.

---

## Module Layout

```
sumi-kernel/src/
├── arch/
│   ├── mod.rs          (modified) pub mod syscall; re-exported from x86_64
│   └── x86_64/
│       ├── mod.rs      (unmodified)
│       └── syscall.rs  MSR setup + assembly trampoline
├── syscall/
│   ├── mod.rs          SyscallArgs, SyscallResult, dispatch()
│   └── handlers/
│       ├── mod.rs      re-exports all handlers
│       ├── io.rs       read, write, open, close, ioctl, pipe, ...
│       ├── fs.rs       stat, fstat, lstat, openat, getdents, ...
│       ├── memory.rs   mmap, munmap, brk, mprotect, mremap, ...
│       ├── process.rs  exit, exit_group, getpid, getuid, uname, ...
│       ├── signal.rs   rt_sigaction, rt_sigprocmask, rt_sigreturn, kill, ...
│       ├── time.rs     clock_gettime, clock_getres, nanosleep, gettimeofday, ...
│       └── net.rs      socket, bind, connect, accept, send, recv, ...
├── kernel_main.rs      (modified) call arch::x86_64::syscall::init()
└── lib.rs              (modified) pub mod syscall
```

---

## Core Types

```rust
// src/syscall/mod.rs

/// Raw syscall arguments, populated by the assembly trampoline.
/// `repr(C)` because it is constructed by assembly.
#[repr(C)]
pub struct SyscallArgs {
    pub nr:   u64,  // RAX
    pub arg0: u64,  // RDI
    pub arg1: u64,  // RSI
    pub arg2: u64,  // RDX
    pub arg3: u64,  // R10
    pub arg4: u64,  // R8
    pub arg5: u64,  // R9
}

/// Linux return value convention: 0..i64::MAX = success, -(errno) = error.
pub type SyscallResult = i64;

/// Returned for unimplemented syscalls.
pub const ENOSYS: SyscallResult = -38;
```

---

## Assembly Trampoline

File: `src/arch/x86_64/syscall.rs`

The trampoline builds a `SyscallArgs` on the stack, saves all callee-saved registers,
calls the Rust dispatcher, restores everything, and returns via `jmp rcx`.

### Stack Layout (after all saves)

```
RSP+ 0   padding (alignment)
RSP+ 8   R11    (saved RFLAGS)
RSP+16   RCX    (return RIP)
RSP+24   RBX
RSP+32   RBP
RSP+40   R12
RSP+48   R13
RSP+56   R14
RSP+64   R15
RSP+72   RAX    ← SyscallArgs.nr    (RDI for Rust call)
RSP+80   RDI    ← SyscallArgs.arg0
RSP+88   RSI    ← SyscallArgs.arg1
RSP+96   RDX    ← SyscallArgs.arg2
RSP+104  R10    ← SyscallArgs.arg3
RSP+112  R8     ← SyscallArgs.arg4
RSP+120  R9     ← SyscallArgs.arg5
```

Total: 16 pushes × 8 = 128 bytes. Before the `call`, `RSP % 16 == 0` (SysV requirement).

### Pseudo-assembly

```asm
.global syscall_entry
syscall_entry:
    // Push SyscallArgs (arg5 first so nr is closest to callee-saved saves)
    push r9     // arg5
    push r8     // arg4
    push r10    // arg3
    push rdx    // arg2
    push rsi    // arg1
    push rdi    // arg0
    push rax    // nr

    // Save callee-saved registers
    push r15
    push r14
    push r13
    push r12
    push rbp
    push rbx

    // Save syscall-clobbered return state
    push rcx    // return RIP
    push r11    // saved RFLAGS

    // Alignment padding  (makes RSP 16-byte aligned before call)
    push 0

    // Call Rust dispatcher with pointer to SyscallArgs
    lea  rdi, [rsp + 72]    // &SyscallArgs (past padding+r11+rcx+callee_saved = 9*8=72)
    call syscall_dispatch    // extern "C" fn(&SyscallArgs) -> SyscallResult
    // RAX = SyscallResult

    // Remove padding
    add rsp, 8

    // Restore state
    pop r11    // saved RFLAGS
    pop rcx    // return RIP
    pop rbx
    pop rbp
    pop r12
    pop r13
    pop r14
    pop r15

    // Discard SyscallArgs (7 * 8 = 56 bytes)
    add rsp, 56

    // Restore RFLAGS and return to caller
    push r11
    popfq
    jmp rcx
```

---

## Dispatcher

```rust
// src/syscall/mod.rs

#[unsafe(no_mangle)]
pub extern "C" fn syscall_dispatch(args: &SyscallArgs) -> SyscallResult {
    match args.nr {
        // I/O
        0  => handlers::io::sys_read(args),
        1  => handlers::io::sys_write(args),
        2  => handlers::io::sys_open(args),
        3  => handlers::io::sys_close(args),
        // ... (see Syscall Table below)
        _  => ENOSYS,
    }
}
```

The dispatcher retrieves `KernelState` from the global and passes it to each handler:

```rust
#[unsafe(no_mangle)]
pub extern "C" fn syscall_dispatch(args: &SyscallArgs) -> SyscallResult {
    let kernel = KERNEL_STATE.get();  // &'static KernelState, set once at boot
    match args.nr {
        0  => handlers::io::sys_read(kernel, args),
        1  => handlers::io::sys_write(kernel, args),
        // ...
        _  => ENOSYS,
    }
}
```

Each handler has the signature:

```rust
pub fn sys_foo<DM: DirectMap>(kernel: &KernelState<DM>, args: &SyscallArgs) -> SyscallResult
```

`KernelState` is passed explicitly so tests can inject a mock `DirectMap` without touching
the global. Handlers extract typed arguments from `args.arg0..arg5` using `VirtualAddr` /
`PhysicalAddr` from `sumi-abi` — never raw `usize`.

```rust
fn sys_read<DM: DirectMap>(kernel: &KernelState<DM>, args: &SyscallArgs) -> SyscallResult {
    let _fd    = args.arg0 as i32;
    let _buf   = VirtualAddr::new(args.arg1 as usize);
    let _count = args.arg2 as usize;
    todo!()
}
```

---

## Handler Organization

### `handlers/io.rs`

| Nr | Name          | Args                              |
|----|---------------|-----------------------------------|
| 0  | `read`        | fd, buf_ptr, count                |
| 1  | `write`       | fd, buf_ptr, count                |
| 2  | `open`        | path_ptr, flags, mode             |
| 3  | `close`       | fd                                |
| 7  | `poll`        | fds_ptr, nfds, timeout_ms         |
| 8  | `lseek`       | fd, offset, whence                |
| 16 | `ioctl`       | fd, request, arg                  |
| 17 | `pread64`     | fd, buf_ptr, count, offset        |
| 18 | `pwrite64`    | fd, buf_ptr, count, offset        |
| 19 | `readv`       | fd, iov_ptr, iovcnt               |
| 20 | `writev`      | fd, iov_ptr, iovcnt               |
| 22 | `pipe`        | pipefd_ptr                        |
| 23 | `select`      | nfds, r, w, e, timeout            |
| 32 | `dup`         | oldfd                             |
| 33 | `dup2`        | oldfd, newfd                      |

### `handlers/fs.rs`

| Nr  | Name          | Args                              |
|-----|---------------|-----------------------------------|
| 4   | `stat`        | path_ptr, statbuf_ptr             |
| 5   | `fstat`       | fd, statbuf_ptr                   |
| 6   | `lstat`       | path_ptr, statbuf_ptr             |
| 21  | `access`      | path_ptr, mode                    |
| 78  | `getdents`    | fd, dirent_ptr, count             |
| 79  | `getcwd`      | buf_ptr, size                     |
| 80  | `chdir`       | path_ptr                          |
| 81  | `fchdir`      | fd                                |
| 82  | `rename`      | old_ptr, new_ptr                  |
| 83  | `mkdir`       | path_ptr, mode                    |
| 84  | `rmdir`       | path_ptr                          |
| 85  | `creat`       | path_ptr, mode                    |
| 86  | `link`        | old_ptr, new_ptr                  |
| 87  | `unlink`      | path_ptr                          |
| 88  | `symlink`     | target_ptr, link_ptr              |
| 89  | `readlink`    | path_ptr, buf_ptr, bufsiz         |
| 257 | `openat`      | dirfd, path_ptr, flags, mode      |
| 262 | `newfstatat`  | dirfd, path_ptr, statbuf_ptr, fl  |
| 263 | `unlinkat`    | dirfd, path_ptr, flags            |

### `handlers/memory.rs`

| Nr  | Name          | Args                              |
|-----|---------------|-----------------------------------|
| 9   | `mmap`        | addr, len, prot, flags, fd, off   |
| 10  | `mprotect`    | addr, len, prot                   |
| 11  | `munmap`      | addr, len                         |
| 12  | `brk`         | addr                              |
| 25  | `mremap`      | old_addr, old_sz, new_sz, fl, new |
| 26  | `msync`       | addr, len, flags                  |
| 27  | `mincore`     | addr, len, vec_ptr                |
| 28  | `madvise`     | addr, len, advice                 |

### `handlers/process.rs`

| Nr  | Name          | Args                              |
|-----|---------------|-----------------------------------|
| 39  | `getpid`      | —                                 |
| 60  | `exit`        | status                            |
| 102 | `getuid`      | —                                 |
| 104 | `getgid`      | —                                 |
| 107 | `geteuid`     | —                                 |
| 108 | `getegid`     | —                                 |
| 110 | `getppid`     | —                                 |
| 158 | `arch_prctl`  | code, addr                        |
| 186 | `gettid`      | —                                 |
| 231 | `exit_group`  | status                            |

### `handlers/signal.rs`

| Nr  | Name              | Args                              |
|-----|-------------------|-----------------------------------|
| 13  | `rt_sigaction`    | signum, act_ptr, old_ptr, sigsetsize |
| 14  | `rt_sigprocmask`  | how, set_ptr, old_ptr, sigsetsize |
| 15  | `rt_sigreturn`    | —                                 |
| 34  | `pause`           | —                                 |
| 62  | `kill`            | pid, sig                          |
| 129 | `rt_sigsuspend`   | mask_ptr, sigsetsize              |
| 130 | `rt_sigpending`   | set_ptr, sigsetsize               |

### `handlers/time.rs`

| Nr  | Name              | Args                              |
|-----|-------------------|-----------------------------------|
| 35  | `nanosleep`       | req_ptr, rem_ptr                  |
| 96  | `gettimeofday`    | tv_ptr, tz_ptr                    |
| 97  | `getrlimit`       | resource, rlim_ptr                |
| 228 | `clock_gettime`   | clk_id, tp_ptr                    |
| 229 | `clock_getres`    | clk_id, res_ptr                   |
| 230 | `clock_nanosleep` | clk_id, flags, req_ptr, rem_ptr   |

### `handlers/net.rs`

| Nr  | Name          | Args                              |
|-----|---------------|-----------------------------------|
| 41  | `socket`      | domain, type, protocol            |
| 42  | `connect`     | fd, addr_ptr, addrlen             |
| 43  | `accept`      | fd, addr_ptr, addrlen_ptr         |
| 44  | `sendto`      | fd, buf_ptr, len, flags, addr, al |
| 45  | `recvfrom`    | fd, buf_ptr, len, flags, addr, al |
| 46  | `sendmsg`     | fd, msg_ptr, flags                |
| 47  | `recvmsg`     | fd, msg_ptr, flags                |
| 48  | `shutdown`    | fd, how                           |
| 49  | `bind`        | fd, addr_ptr, addrlen             |
| 50  | `listen`      | fd, backlog                       |
| 51  | `getsockname` | fd, addr_ptr, addrlen_ptr         |
| 52  | `getpeername` | fd, addr_ptr, addrlen_ptr         |
| 54  | `setsockopt`  | fd, level, optname, val, optlen   |
| 55  | `getsockopt`  | fd, level, optname, val, len_ptr  |

---

## Integration

### `kernel_main.rs` Changes

```rust
pub extern "C" fn _start() -> ! {
    let _kernel = KernelState::new(...);

    // NEW: initialise syscall handling before running any user code
    arch::syscall::init();

    // ... eventually: load and jump to Linux ELF binary

    halt_forever()
}
```

### `arch/mod.rs` Changes

```rust
pub mod syscall;  // NEW — re-exports arch::x86_64::syscall
```

### `lib.rs` Changes

```rust
pub mod syscall;  // NEW
```

---

## Error Convention

Linux requires: on error, return `-(errno as i64)` in `RAX`. When handlers are implemented,
a thin helper will convert `Result<u64, i32>` to `SyscallResult` — deferred to
implementation phase.

---

## Open Questions

1. **File descriptors** — Need a FD table. Likely a fixed-size array in `KernelState`.
   Deferred until handlers are implemented.

2. **Pointer validation** — Handlers use `VirtualAddr` for all pointer arguments. Debug
   builds may add bounds checks inside `VirtualAddr` itself; handlers do not validate
   explicitly. A crash in the guest program means the kernel should crash too.

3. **`exit` / `exit_group`** — Must halt the vCPU cleanly. Likely calls `halt_forever()`
   after signalling the hypervisor via the debugcon port or a dedicated hypercall.
