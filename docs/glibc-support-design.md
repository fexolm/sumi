# glibc Binary Support — Design Document

## 1. Goal

Run dynamically-linked Linux ELF binaries compiled against **glibc** under sumi.
Target: a `gcc`-compiled C `hello world` that links against the host distro's
`libc.so.6` / `ld-linux-x86-64.so.2` should boot, load all required shared
objects, and run to completion.

This is the follow-up to [dynamic-linking-design.md](dynamic-linking-design.md),
which established musl support and explicitly deferred glibc to "Phase 3".

### 1.1 Non-goals

- `dlopen` / `dlmopen` (runtime library loading).
- NSS modules, locale machinery, iconv — the test binary must not depend on them.
- Real multi-threading (`clone` / `clone3` will not be implemented). glibc's
  single-threaded fast paths must work.
- `/proc` / `/sys` / `/dev` beyond the minimum glibc reads during startup.
- `mprotect` actually enforcing page protections — stays a no-op.
- 4 KB page support — stays 2 MB huge pages only.
- Real signal delivery — stubs only.
- `ld.so.cache` — we require explicit library placement or RPATH.
- ASLR.

### 1.2 Success criterion

```bash
cat > /tmp/hello.c <<'EOF'
#include <stdio.h>
int main(void) { printf("Hello from glibc!\n"); return 0; }
EOF
gcc -O2 -march=x86-64-v2 -o /tmp/hello /tmp/hello.c

cargo run -p sumi-vm -- run --run /tmp/hello \
    target/x86_64-unknown-none/debug/sumi-kernel
# expected output: "Hello from glibc!" followed by [exit] code=0
```

`--share` defaults to `/`, so the guest sees the host filesystem natively
and resolves `PT_INTERP=/lib64/ld-linux-x86-64.so.2`, `libc.so.6`, etc.
against the real host paths. No staging step. See §7.

---

## 2. Background: why glibc is harder than musl

The kernel's role in running a dynamic binary is small — it loads the main ELF
and the interpreter, builds the initial stack, and jumps to `ld.so`. Everything
after that is driven by the dynamic linker in userspace. We already do this for
musl. The question is **what does glibc's `ld.so` / `libc.so.6` touch that
musl does not**.

### 2.1 What musl already exercises (works today)

The [`dynamic_hello_musl`](../sumi-integration-tests/tests/user_programs.rs)
integration test already runs a C program linked against musl, which means the
following end-to-end path works:

- ELF loader for `ET_DYN` main + `PT_INTERP`
  ([exec.rs:104-220](../sumi-kernel/src/exec.rs#L104-L220))
- Full auxv: `AT_PHDR`, `AT_PHENT`, `AT_PHNUM`, `AT_PAGESZ`, `AT_BASE`,
  `AT_ENTRY`, `AT_RANDOM`, `AT_{U,EU,G,EG}ID`, `AT_SECURE`
  ([exec.rs:406-423](../sumi-kernel/src/exec.rs#L406-L423))
- `mmap` (anonymous, file-backed, MAP_FIXED at 4 KB granularity, DAX fast path)
- `mprotect` no-op, `munmap`, `brk`
- `arch_prctl(ARCH_SET_FS, addr)` writes `MSR_FS_BASE` (IA32_FS_BASE, 0xC000_0100)
- `futex` non-blocking fast path, `set_tid_address`, `rt_sigprocmask` stub
- virtio-fs-backed `openat` / `read` / `fstat` / `close`
- `getrandom` backed by RDRAND / BOOT_INFO seed
- `rseq` → `ENOSYS` (syscall dispatcher default,
  [syscall/mod.rs:95](../sumi-kernel/src/syscall/mod.rs#L95))

### 2.2 What's different about glibc

1. **More libraries, transitively.** Even a trivial hello world pulls in
   `ld-linux-x86-64.so.2` and `libc.so.6`. A program calling `sin()` also
   pulls `libm.so.6`. Each library is mmap'd file-backed through the FUSE path,
   relocated, optionally RELRO'd. The musl test loads only one file (musl's
   ld.so and libc are the same binary); glibc's ld.so loads libc.so.6 as a
   separate DSO.
2. **Dynamic-linker search path.** glibc's ld.so does *not* read any
   `/etc/ld-musl-*.path` file — it walks `DT_RPATH` / `DT_RUNPATH`, `LD_LIBRARY_PATH`,
   `/etc/ld.so.cache`, and the hard-coded defaults (`/lib64`, `/usr/lib64`, `/lib`,
   `/usr/lib`). Since we do not ship a cache, we must place libraries in one of
   the default search directories or embed an `RPATH` at link time.
3. **IFUNC resolvers.** glibc ships multi-versioned `memcpy` / `memset` / `strlen`
   etc., and the loader calls `STT_GNU_IFUNC` resolvers during relocation. Each
   resolver reads CPUID (directly, no syscall) and picks the best variant. If the
   resolver selects AVX/AVX2 and we run the instruction, the guest `#UD`s because
   we never enabled `CR4.OSXSAVE` / `XCR0`. See §5.
4. **TLS startup is more elaborate.** glibc's `_dl_allocate_tls` allocates the
   TCB plus dtv plus initial TLS blocks, then calls `arch_prctl(ARCH_SET_FS, tcb)`.
   Between allocation and `arch_prctl`, TLS-relative accesses (`fs:...`) must
   not happen — glibc is careful, we do not need to do anything special, but
   the kernel must not clobber `MSR_FS_BASE` across the syscall boundary.
5. **Startup syscall sequence is richer.** In addition to what musl calls,
   glibc ≥ 2.35 startup typically issues:
   - `set_tid_address` — already stubbed
   - `set_robust_list` (273) — **not in the dispatch table**; hits the fall-through
     `ENOSYS` path plus an `[syscall] unhandled nr=273` kprintln spam. glibc
     tolerates `ENOSYS` here.
   - `rseq` (334) — already returns `ENOSYS` explicitly. glibc tolerates this
     but also respects `GLIBC_TUNABLES=glibc.pthread.rseq=0`.
   - `prlimit64(RLIMIT_STACK)` — stubbed as `ENOSYS`
     ([time.rs:191](../sumi-kernel/src/syscall/handlers/time.rs#L191)).
     glibc uses this to compute the stack-guard; on `ENOSYS` it falls back to
     `getrlimit` (97) which is also stubbed `ENOSYS`. This is the first hard
     blocker — glibc *does* need a plausible stack-size answer to compute guard
     pages.
   - `readlink("/proc/self/exe")` — used by `__get_nprocs` and some audit paths;
     current readlink implementation forwards to FUSE, so it will `ENOENT`.
     glibc usually tolerates this.
   - `openat(AT_FDCWD, "/etc/ld.so.cache", O_RDONLY)` — `ENOENT` is fine; ld.so
     just skips the cache.
   - `openat(..., "/etc/ld.so.preload", O_RDONLY)` — `ENOENT` is fine.
   - `brk(0)` then `brk(addr)` — already implemented.
   - `mmap` / `mprotect` / `munmap` as per musl path.
6. **`mprotect` semantics for RELRO.** glibc calls `mprotect(PROT_READ)` on the
   RELRO window after applying relocations. Our `mprotect` is a no-op, which is
   behaviorally correct (we lose the hardening but the program runs).
7. **Stack canary.** glibc's `__stack_chk_guard` is read from TLS, which is
   initialized from `AT_RANDOM`. We already provide a 16-byte `AT_RANDOM` block,
   so this works unmodified.
8. **vDSO.** glibc prefers vDSO for `clock_gettime` / `gettimeofday` / `getcpu`
   / `time`. If `AT_SYSINFO_EHDR` is absent, glibc falls back to the syscall path,
   which is what we already implement. **No vDSO needed for correctness.**
9. **CPU feature assumptions.** `libc.so.6` built for `x86-64-v2` or `v3`
   baselines (common on modern distros) contains unconditional SSE4.2 / AVX /
   AVX2 instructions outside IFUNCs. Debian/Ubuntu still ship baseline `x86-64-v1`
   glibc, so SSE2 is the floor. We already enable SSE2 via
   `CR4.OSFXSR | CR4.OSXMMEXCPT` and `!CR0.EM / !CR0.TS`
   ([kvm/mod.rs:282,311-312](../sumi-vm/src/arch/x86_64/kvm/mod.rs#L282)).
   AVX is a problem — see §5.

---

## 3. Current state: what already works

Relative to the gap list in §2, here is what needs *no* change:

| Capability | Where | Status |
|---|---|---|
| ELF loader for `ET_DYN` + `PT_INTERP` | [exec.rs:104-220](../sumi-kernel/src/exec.rs#L104-L220) | Works |
| auxv with `AT_BASE`, `AT_RANDOM`, ID entries | [exec.rs:406-423](../sumi-kernel/src/exec.rs#L406-L423) | Works |
| `mmap` file-backed + DAX + MAP_FIXED 4 KB | [memory.rs](../sumi-kernel/src/syscall/handlers/memory.rs) | Works |
| `arch_prctl(ARCH_SET_FS)` via wrmsr | [process.rs](../sumi-kernel/src/syscall/handlers/process.rs) | Works |
| `futex` fast path (single-threaded) | [thread.rs](../sumi-kernel/src/syscall/handlers/thread.rs) | Works |
| `getrandom` | [random.rs](../sumi-kernel/src/syscall/handlers/random.rs) | Works |
| `clock_gettime`, `gettimeofday` | [time.rs](../sumi-kernel/src/syscall/handlers/time.rs) | Works |
| virtio-fs openat / read / fstat / close | [fs.rs](../sumi-kernel/src/syscall/handlers/fs.rs) | Works |
| `SSE` enablement in vCPU | [kvm/mod.rs:282](../sumi-vm/src/arch/x86_64/kvm/mod.rs#L282) | Works |

Everything in §4 below is what actually needs to change.

---

## 4. Required changes: kernel & syscalls

### 4.1 `set_robust_list` (nr 273) — add a stub

**File:** [sumi-kernel/src/syscall/mod.rs](../sumi-kernel/src/syscall/mod.rs),
[handlers/thread.rs](../sumi-kernel/src/syscall/handlers/thread.rs)

glibc calls this unconditionally during thread setup. Returning `ENOSYS` via
the fall-through works but fires an `[syscall] unhandled nr=273` kprintln on
every boot. Add an explicit stub that returns `0` (pretend we registered the
list but never walk it — correct for a single-threaded unikernel with no
cancellation).

```rust
// handlers/thread.rs
pub fn sys_set_robust_list(_args: &SyscallArgs) -> SyscallResult {
    // Single-threaded unikernel: pretend the list is installed. We never
    // walk it because we never exit a thread other than the main one.
    0
}
```

```rust
// syscall/mod.rs dispatch
273 => handlers::thread::sys_set_robust_list(args),
```

Matching `get_robust_list` (274) is not called by glibc startup — leave it on
the ENOSYS path.

### 4.2 `prlimit64` / `getrlimit` — return plausible RLIMIT_STACK

**File:** [sumi-kernel/src/syscall/handlers/time.rs:191](../sumi-kernel/src/syscall/handlers/time.rs#L191)

glibc uses `RLIMIT_STACK` to compute the stack guard page size and to decide
whether to call `mprotect` on the bottom of the stack. On `ENOSYS` it may fall
back to the default `8 * 1024 * 1024`, but some glibc versions abort if the
value is implausible. We already know our stack size: `USER_STACK_SIZE` from
[layout.rs](../sumi-abi/src/arch/x86_64/layout.rs).

Implement the minimum set of resources, return `EINVAL` for everything else:

| resource | rlim_cur | rlim_max |
|---|---|---|
| `RLIMIT_STACK` (3) | `USER_STACK_SIZE` | `USER_STACK_SIZE` |
| `RLIMIT_NOFILE` (7) | `1024` | `1024` |
| `RLIMIT_AS` (9) | `RLIM_INFINITY` | `RLIM_INFINITY` |
| `RLIMIT_DATA` (2) | `RLIM_INFINITY` | `RLIM_INFINITY` |
| `RLIMIT_CORE` (4) | `0` | `0` |

`prlimit64(pid, res, new, old)`: if `new != NULL`, silently ignore; if
`old != NULL`, copy the entry from the table. Only accept `pid == 0`.
`getrlimit(res, old)` reuses the same table.

Move the table to a shared helper to avoid duplication. Place it next to
`sys_prlimit64` since `time.rs` already owns these syscalls.

### 4.3 `uname` — make glibc happy

**File:** [sumi-kernel/src/syscall/handlers/process.rs](../sumi-kernel/src/syscall/handlers/process.rs)

glibc's `gnu_get_libc_release` calls `uname` and parses `release`. It rejects
strings that do not look like a kernel version (e.g. it wants `X.Y.Z-...`).
The current stub returns sysname `"sumi"`, release `"0.1.0"` — musl tolerates
this but glibc's `__init_misc` parses `release` as a `major.minor.patch`
string and some call sites (e.g. `tunables`) check for `major >= 3`.

Return a Linux-compatible release string so glibc's version checks pass:

```rust
release:  b"6.6.0-sumi\0..."    // 6.6.0 is the minimum glibc 2.38+ accepts
version:  b"#1 SMP sumi\0..."
sysname:  b"Linux\0..."         // glibc only checks sysname for "Linux" in a few places
machine:  b"x86_64\0..."
nodename: b"sumi\0..."
domainname: b"(none)\0..."
```

Sysname `"Linux"` is mandatory: the glibc `_dl_discover_osversion`
implementation requires it; any other sysname is treated as a non-Linux system
and may trip `abort` in tunables.

### 4.4 `ioctl(TIOCGWINSZ)` — return a plausible size

**File:** [sumi-kernel/src/syscall/handlers/io.rs](../sumi-kernel/src/syscall/handlers/io.rs)

glibc's `printf` / `setvbuf` calls `ioctl(1, TIOCGWINSZ, &ws)` to decide line vs
full buffering for stdout. Current stub returns `ENOTTY`, which is actually the
correct answer for "not a TTY" and causes glibc to switch to full buffering —
which means `printf("Hello\n")` buffered until `exit_group`, where it *is* flushed
via `_IO_cleanup`. So `ENOTTY` is fine as long as glibc flushes on exit. No
change required, but document this here because it's easy to misdiagnose if
the hello world goes silent.

### 4.5 `mmap` — verify large file handling

**File:** [sumi-kernel/src/syscall/handlers/memory.rs](../sumi-kernel/src/syscall/handlers/memory.rs)

`libc.so.6` is ~2 MB on modern glibc. `ld-linux-x86-64.so.2` is ~220 KB. The
musl test exercises a single ~800 KB file. We need to verify:

1. File-backed `mmap` of a 2 MB file — does the DAX reservation and per-segment
   `MAP_FIXED` chain work when `libc.so.6` actually spans two 2 MB pages?
2. VMA table capacity: glibc may create ~5-10 VMAs per library × 3 libraries =
   ~30 VMAs. Current `MAX_VMAS` is 256, so fine.
3. `mmap(NULL, size, PROT_NONE, MAP_PRIVATE|MAP_ANONYMOUS, ...)` — glibc uses
   this to reserve the TLS static area. The current mmap path allocates pages
   eagerly regardless of `PROT_NONE`, which is correct but wasteful. No change.

No code change expected, but §8 adds integration tests covering `libc.so.6`'s
exact layout.

### 4.6 `/proc/self/exe` readlink

**File:** [sumi-kernel/src/syscall/handlers/fs.rs](../sumi-kernel/src/syscall/handlers/fs.rs)

glibc's `dl_main` calls `readlink("/proc/self/exe", ...)` to set the
`__progname_full` global. On `ENOENT` glibc falls back to `argv[0]`. We
currently forward `readlink` to FUSE which returns `ENOENT` for `/proc/*`, so
glibc handles it gracefully. **No change needed for hello world.**

If a later test program depends on `__progname_full`, add a special-case in
`sys_readlink`:

```rust
if path == b"/proc/self/exe" {
    // Copy RUN_PATH (stored at exec time) into the user buffer.
    return copy_run_path(buf, bufsize);
}
```

Store the run path in a new `RUN_PATH: Once<&'static str>` global in `lib.rs`
and set it from `exec_user_program`. Defer this until a real failure forces it.

### 4.7 Optional: AVX / XSAVE enablement

**File:** [sumi-vm/src/arch/x86_64/kvm/mod.rs:282](../sumi-vm/src/arch/x86_64/kvm/mod.rs#L282)

glibc's IFUNC resolvers read CPUID features and select AVX paths on CPUs that
advertise them. On x86, `AVX` is usable only if `CR4.OSXSAVE = 1` **and**
`XCR0[1] (SSE) = 1`, `XCR0[2] (AVX) = 1`. When `OSXSAVE = 0`, CPUID `1:ECX.OSXSAVE`
reports 0, and a correctly-written IFUNC resolver (e.g. glibc's
`init-arch.c`) will *not* select an AVX variant — it checks both the vendor
CPUID bit and the OS-enabled bits via `xgetbv`.

**Default plan: rely on the IFUNC guard.** Do not enable XSAVE. Observe what
`init-arch.c` selects via KVM's CPUID and the host CPU exposure. If the guest
IFUNCs select SSE2 paths only, no change needed.

**Fallback plan: enable XSAVE + XCR0.** If IFUNCs do select AVX, add:

```rust
const CR4_OSXSAVE: u64 = 1 << 18;
sregs.cr4 |= CR4_OSXSAVE;
// After vcpu init, issue `xsetbv` to set XCR0 = 0x7 (x87 + SSE + AVX).
```

`xsetbv` is a ring-0 instruction, so run it from the kernel's `_start` before
any user code:

```rust
unsafe {
    asm!(
        "xor ecx, ecx",     // XCR0
        "mov eax, 0x7",     // x87 + SSE + AVX
        "xor edx, edx",
        "xsetbv",
    );
}
```

Additionally, filter the KVM `KVM_SET_CPUID2` list to **clear** `AVX` and
`AVX2` bits if we don't want to enable XSAVE — this is the simplest way to
force glibc IFUNCs onto the SSE2 path without touching control registers.
KVM already exposes a `cpuid.set_entries` API.

**Recommendation:** start by filtering CPUID to mask `AVX`/`AVX2`/`AVX512*`
via `KVM_SET_CPUID2`. This is strictly less code than OSXSAVE + XCR0 and
avoids a class of "we enabled AVX but not OSXSAVE correctly" bugs. If a
future workload needs AVX for performance, enable XSAVE properly at that time.

### 4.8 Syscall surface summary

| Syscall | nr | Current | Change |
|---|---|---|---|
| `set_robust_list` | 273 | default ENOSYS (unhandled log) | explicit 0-stub |
| `prlimit64` | 302 | ENOSYS | return table from §4.2 |
| `getrlimit` | 97 | ENOSYS | return table from §4.2 |
| `uname` | 63 | sysname="sumi" | sysname="Linux", release="6.6.0-sumi" |
| `ioctl(TIOCGWINSZ)` | 16 | ENOTTY | keep — §4.4 |
| `rseq` | 334 | ENOSYS | keep |
| `readlink("/proc/self/exe")` | 89 | ENOENT via FUSE | keep — §4.6 |

Nothing else is added. In particular, we do **not** add `clone`, `clone3`,
vDSO, signal delivery, or `/proc` population.

---

## 5. CPU feature handling (detail)

The plan from §4.7: **mask AVX family bits in the CPUID leaves we return to the
guest.** Implementation:

1. `sumi-vm/src/arch/x86_64/kvm/mod.rs`: after
   `kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)`, walk the entries and:
   - For function `1`: clear `ECX[28]` (AVX), `ECX[12]` (FMA), `ECX[27]`
     (OSXSAVE), `ECX[26]` (XSAVE).
   - For function `7`, subleaf `0`: clear `EBX[5]` (AVX2), `EBX[16]`
     (AVX512F), `EBX[17]` (AVX512DQ), `EBX[28]` (AVX512CD), `EBX[30]`
     (AVX512BW), `EBX[31]` (AVX512VL), `ECX[1]` (AVX512VBMI), `ECX[6]`
     (AVX512VBMI2), etc. (Pull the full list from Intel SDM vol 2 chapter 3.)
   - Leave SSE2 / SSE3 / SSSE3 / SSE4.1 / SSE4.2 / POPCNT / CX16 set.
2. Call `vcpu.set_cpuid2(&filtered)` before `init`.

This matches what `qemu -cpu ...,-avx,-avx2` does and is a well-understood
knob. Document the rationale inline so future maintainers understand why we
advertise SSE4.2 but not AVX.

**Verified empirically (Ubuntu 25.x, glibc 2.41, gcc 15.2):**

- All AVX/AVX2 paths in `libc.so.6` are gated behind IFUNC resolvers, so the
  CPUID mask is sufficient to keep glibc on the SSE2 baseline at runtime.
- `gcc -O3 -march=x86-64-v2` (Nehalem baseline: SSE4.2, no AVX) for the
  user binary: works.
- `gcc -O3 -march=x86-64-v3` (Haswell baseline: AVX, AVX2, FMA, BMI2) for
  the user binary: **crashes with `#UD` → triple fault**. The compiler
  emits unconditional VEX-encoded instructions in main() itself
  (`vmovdqa`, `vbroadcasti128`, etc.) which the guest cannot legally execute
  without `CR4.OSXSAVE`. The CPUID mask only reroutes glibc IFUNC resolvers
  — it cannot rewrite already-compiled user code.

**Therefore the supported user-binary baseline is `x86-64-v2`** (SSE2/SSE3/
SSSE3/SSE4.1/SSE4.2/POPCNT/CX16). The integration test pins
`-march=x86-64-v2` explicitly. If a future workload needs AVX, that requires
a separate change: enabling XSAVE/XCR0 in CR4, setting `XCR0[2..0] = 7` via
`xsetbv`, and removing the OSXSAVE/AVX bits from the CPUID mask.

---

## 6. User-space: address space and TLS

No changes. The existing layout from
[layout.rs](../sumi-abi/src/arch/x86_64/layout.rs) already provides what glibc
needs:

```
0x0000_0000_0040_0000  PIE_LOAD_BASE      — main binary
0x0000_0000_00??_????  brk heap
...
0x0000_7f00_0000_0000  INTERP_LOAD_BASE   — ld-linux-x86-64.so.2
0x0000_7FFF_0000_0000  USER_MMAP_BASE     — libc.so.6, libm.so.6, TLS blocks
0x0000_7FFF_FF80_0000  stack bottom
0x0000_7FFF_FFFF_F000  USER_STACK_TOP
```

glibc's TLS block is allocated via anonymous `mmap` at some address in the
USER_MMAP_BASE range, then installed via `arch_prctl(ARCH_SET_FS, tcb)`. Our
syscall entry path does **not** touch FS/GS
([arch/x86_64/syscall.rs:84-154](../sumi-kernel/src/arch/x86_64/syscall.rs#L84-L154))
so the user's FS base survives across syscalls. The kernel itself does not
emit any `fs:`-relative accesses (verified by reading the generated asm for
the syscall dispatch and `kprintln` paths during previous work on
`dynamic_hello_musl`).

One subtle concern: the Rust kernel may grow a stack protector in the future,
which LLVM implements as `fs:0x28`. If that happens, glibc's FS base (a
pointer to its TCB with canary at offset 0x28) would be read by kernel code
expecting its own canary and we'd get nondeterministic crashes. Mitigation:
build the kernel with `-C no-stack-check -Z stack-protector=none`. This is
already implicit in `x86_64-unknown-none` + Cargo defaults; add an explicit
check in `build.rs` if paranoid. **Not doing this now** — document as an
assumption and add a test that builds a Rust fn with stack-protector and
verifies it's not emitted.

---

## 7. Filesystem: share the host root, no staging

The canonical way to run a glibc-linked binary under sumi is to expose the
host filesystem directly:

```bash
sumi-vm run --share / --run /tmp/hello target/x86_64-unknown-none/debug/sumi-kernel
```

Equivalently, since `--share /` is the default in
[sumi-vm/src/cmd/run.rs](../sumi-vm/src/cmd/run.rs):

```bash
sumi-vm run --run /tmp/hello target/x86_64-unknown-none/debug/sumi-kernel
```

What this gives you:

- The guest sees the host filesystem one-to-one. Every absolute path
  resolves to the same file as on the host.
- `PT_INTERP=/lib64/ld-linux-x86-64.so.2` resolves through virtio-fs to
  the actual host `/lib64/ld-linux-x86-64.so.2`. No copy.
- glibc's library search (`/lib64`, `/lib`, `/lib/x86_64-linux-gnu`,
  `/usr/lib`, etc.) hits the real host directories. `libc.so.6`,
  `libm.so.6`, and any transitively-needed DSOs are loaded zero-copy.
- `/etc/ld.so.cache` is also visible, so glibc's regular cache fast path
  works without us shipping a stub. The fallback search path also works
  without it.
- No script. No staging step. No drift between "what the test sees" and
  "what the user sees".

### 7.1 Why this works

[VirtioFs::new](../sumi-vm/src/devices/virtio_fs.rs) treats `share_dir` as
the FUSE root nodeid 1 and resolves every guest path against it via
`share_dir.join(rel)`. There is no chroot. With `share_dir = "/"`, the
host's `realpath` of any guest absolute path equals the path itself.

### 7.2 What about hermetic runs / CI containers without /lib64?

If you need a hermetic share directory for some reason (e.g. running
sumi-vm inside a minimal container that does not have a host glibc at the
expected paths), you can still build one by hand: copy the binary plus
`ld-linux-x86-64.so.2`, `libc.so.6`, etc. into `/lib64` and
`/lib/x86_64-linux-gnu` under that directory and pass `--share <dir>`. The
kernel does not care which mode is in use — both resolve through the same
FUSE path.

Both `dynamic_hello_glibc` and `dynamic_hello_musl` integration tests use
the host-share approach. There is no separate staging helper.

---

## 8. Implementation plan

### Phase 0: reproduction test

Before touching kernel code, add an integration test that compiles and
**attempts** to run a glibc hello world. It should fail in a known way
(probably a crash during ld.so startup, or a syscall ENOSYS log), so we have
a concrete baseline.

- [ ] Add `tests/fixtures/dynamic_hello_glibc.c` (identical source to
      `dynamic_hello.c`).
- [ ] Add `compile_glibc_dynamic()` helper to
      [user_programs.rs](../sumi-integration-tests/tests/user_programs.rs),
      mirroring `compile_musl_dynamic()`.
- [ ] Add `dynamic_hello_glibc` integration test that skips if `gcc` is not
      available, builds the binary into a tmp dir, runs it under sumi-vm
      via `--share /` (so `PT_INTERP` and `DT_NEEDED` resolve against the
      host filesystem natively), and expects `"Hello from glibc!"` +
      `[exit] code=0`.

### Phase 1: syscall surface

- [ ] `sys_set_robust_list` stub (§4.1). Unit test: returns 0 regardless of
      arguments.
- [ ] `sys_prlimit64` / `sys_getrlimit` table (§4.2). Unit tests:
      `RLIMIT_STACK` returns `USER_STACK_SIZE`; unknown resource returns
      `EINVAL`; `new != NULL` is silently ignored.
- [ ] `sys_uname` Linux-compatible strings (§4.3). Unit test: `release` matches
      `/^[0-9]+\.[0-9]+\.[0-9]+/`, sysname is exactly `Linux`.

### Phase 2: CPU feature masking

- [ ] Filter AVX/AVX2/AVX512/FMA/XSAVE/OSXSAVE out of the guest CPUID in
      `sumi-vm/src/arch/x86_64/kvm/mod.rs`.
- [ ] Host-side test: spawn a vCPU, query CPUID, assert masked bits are 0.
- [ ] Verify on the dev box that
      `objdump -d $(ldconfig -p | awk '/libc.so.6/{print $NF; exit}')` does
      not emit AVX instructions outside of IFUNC resolver functions. If it
      does, fall back to the XSAVE + XCR0 path from §4.7.

### Phase 3: run it

- [ ] Unignore `dynamic_hello_glibc`. Expect: passes.
- [ ] Review the new syscall log lines — every `[syscall] unhandled nr=N`
      that fires during glibc startup is a bug in this plan and should be
      added as an explicit (possibly ENOSYS) dispatch entry so the log is
      clean.

### Phase 4: harden

- [ ] Add a test program that calls `malloc` / `free` — exercises glibc's
      `mmap`-backed heap and verifies our 2 MB VMA handling plays with
      glibc's arena allocator.
- [ ] Add a test program that calls `sin(1.0)` — pulls `libm.so.6` and
      exercises a second library.
- [ ] Add a test program that `printf`s from many threads — **expected to
      fail** because we do not implement `clone`. Document the failure as a
      known limitation.

### Phase 5: (optional) vDSO

Deferred. glibc runs fine without vDSO; time syscalls are cheap in a
unikernel because there is no ring transition. Revisit only if profiling
shows `clock_gettime` as a hot path.

---

## 9. Testing strategy

### 9.1 Unit tests (`cargo test`)

| Test | Verifies |
|---|---|
| `set_robust_list_returns_zero` | §4.1 stub shape |
| `prlimit64_stack_returns_user_stack_size` | §4.2 table |
| `prlimit64_unknown_resource_einval` | §4.2 error path |
| `getrlimit_stack_matches_prlimit64` | §4.2 consistency |
| `uname_release_is_linux_versioned` | §4.3 parseable release |
| `cpuid_mask_clears_avx_bits` | §5 filter logic (host side) |

### 9.2 KVM integration (`make self-test` + `cargo test -p sumi-integration-tests`)

| Test | Verifies |
|---|---|
| `dynamic_hello_musl` (regression) | musl path still works |
| `dynamic_hello_glibc` (new) | glibc hello world runs to completion |
| `glibc_malloc_free` (new) | glibc arena allocator works over our mmap |
| `glibc_libm_sin` (new) | second DSO loads and links |

All new tests skip if `gcc` or `/dev/kvm` is unavailable, matching the musl
tests' skip-on-missing-toolchain convention.

### 9.3 Manual verification

```bash
make build
gcc -O2 -march=x86-64-v2 -o /tmp/hello tests/fixtures/dynamic_hello_glibc.c
cargo run -p sumi-vm -- run --run /tmp/hello \
    target/x86_64-unknown-none/debug/sumi-kernel
```

Expected: `Hello from glibc!` followed by `[exit] code=0`. `--share` is
omitted because `/` is the default — the guest uses the host filesystem
directly, and `PT_INTERP=/lib64/ld-linux-x86-64.so.2` resolves against the
real host file.

---

## 10. Risks and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| Host glibc has unconditional AVX/AVX2 outside IFUNCs (x86-64-v3 build) | guest `#UD` during ld.so startup | §4.7 fallback: enable XSAVE + XCR0. Or ship a v1-compiled glibc with the test. |
| glibc version bumps break assumptions (e.g. new required syscall) | boot fails after dev-box upgrade | CI pins to a specific Debian/Ubuntu container image so the glibc under test is stable |
| `set_robust_list` stub returning 0 masks a real bug later (if we ever add threading) | silent data corruption | only applies once `clone` exists — gate with `debug_assert!(num_threads == 1)` inside the stub |
| 2 MB pages waste ~4-8 MB for ld.so + libc | higher memory floor | acceptable for unikernel; same tradeoff as musl path |
| glibc parses `uname().release` more strictly in the future | boot fails on upgrade | version string `6.6.0-sumi` chosen because glibc's strictest check accepts it; document the constraint inline |
| Stack protector emitted in kernel code reads user FS base | nondeterministic crash | build flags exclude `-Z stack-protector`; add a compile-time assertion |
| `libc.so.6` spans more 2 MB pages than expected, breaking MAP_FIXED fast path | ld.so crashes | Phase 4 integration tests cover real glibc file sizes |
| Host environment affects glibc search behavior (`LD_*` envvars) | flaky tests | `run_program` helper explicitly clears `LD_*` from the guest envp (already empty — envp is not propagated) |
| `ld.so` tries to `openat` dozens of unknown paths, spamming kprintln | log noise obscures real errors | demote FUSE `ENOENT` on `/etc/*` and `/proc/*` to a silent code path |

---

## 11. Open questions

1. **Which glibc version do we test against?** Debian 12 (glibc 2.36),
   Ubuntu 24.04 (glibc 2.39), Fedora 40 (glibc 2.39)? Pin to Ubuntu 24.04 in
   CI; document locally for devs.
2. **Do we ship a glibc in-tree?** No — use whatever is on the host. Tests
   skip if `gcc` is unavailable.
3. **Is `rseq` a real concern?** glibc ≥ 2.35 calls `rseq` at startup and
   stores the result in `__rseq_offset`. On `ENOSYS` glibc sets
   `__rseq_offset = 0` and continues. **No change needed**, but document
   that `GLIBC_TUNABLES=glibc.pthread.rseq=0` is a known workaround if a
   specific glibc version regresses this.
4. **Does glibc call `clone` during single-threaded startup?** No —
   `clone` is only called from `pthread_create`. A purely single-threaded
   binary (no `pthread_create`) never calls it. Verified against glibc 2.36
   source.
5. **Should we also mask `PCID` / `INVPCID` / `RDRAND` / `RDSEED` CPUID
   bits?** No — these are individually-usable features and glibc's `getentropy`
   path uses `getrandom(2)`, not `rdrand` directly.
6. **Do we need `AT_SYSINFO_EHDR`?** No — glibc's `_dl_aux_init` tolerates
   missing `AT_SYSINFO_EHDR` and sets the vDSO function table to null, which
   makes `clock_gettime` fall back to `syscall(SYS_clock_gettime, ...)`.
7. **Do we need `AT_HWCAP` / `AT_HWCAP2`?** glibc reads these to set
   `__x86_string_control` and similar tunables. Missing them means glibc
   falls back to defaults, which is correct but possibly slower.
   **Recommendation:** add `AT_HWCAP = 0` explicitly in Phase 1 to avoid
   subtle misdetection of CPU features. The existing auxv push order in
   [exec.rs:406-423](../sumi-kernel/src/exec.rs#L406-L423) just needs one
   more line.

---

## 12. Summary: change footprint

| Area | Files | Lines changed (estimate) |
|---|---|---|
| New syscall stub | `syscall/mod.rs`, `handlers/thread.rs` | +15 |
| rlimit table | `handlers/time.rs` | +40 |
| uname strings | `handlers/process.rs` | +10 |
| CPUID AVX mask | `sumi-vm/src/arch/x86_64/kvm/mod.rs` | +30 |
| Integration test + fixture | `sumi-integration-tests/tests/user_programs.rs`, `tests/fixtures/dynamic_hello_glibc.c` | +60 |
| Unit tests | various | +80 |

Total: ~300 lines across 6 files. No architectural changes. No new crates,
no new traits, no new globals. The entire feature lives on top of the
existing musl dynamic-linking path.
