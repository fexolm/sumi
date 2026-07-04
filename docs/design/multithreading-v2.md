# Multithreading in sumi — Design Document (v2: M:N scheduler)

> Status: implemented (phases 0–9); this revision synced to code 2026-07-03.
> Audience: sumi kernel developers.
> Supersedes: `docs/design/multithreading.md` (v1, which used a 1:1 vCPU-per-thread model).
> Related: `docs/syscall-design.md`, `docs/glibc-support-design.md`,
> `docs/user-program-design.md`, `docs/dynamic-linking-design.md`.

## 1. Goal and scope

### 1.1. What "multithreading" means in sumi

sumi is a unikernel: everything (kernel, glibc, user code) runs in ring 0 in a
single virtual address space, without process isolation and without a
user/kernel privilege boundary on syscall entry. A "thread" in sumi is
therefore not a Linux task with its own `mm`/`files`/`credentials` — it is
simply **an independent flow of execution over shared data structures**.
Isolation (process, thread group, namespace) is not needed; what we need is
**concurrent instruction delivery on multiple host CPUs** and a scheduler that
multiplexes many logical threads onto a fixed set of vCPUs.

The primary use case is **`pthread_create` / `std::thread::spawn` inside a
glibc-linked program**: glibc calls
`clone(CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD |
CLONE_SYSVSEM | CLONE_SETTLS | CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID,
child_stack, ptid, tls, ctid)`. After the call returns the new thread must
start executing `start_routine` passed to `pthread_create`. `std::thread`
in Rust uses the same path via libstd on top of glibc.

The secondary use case is `clone3` (new syscall with an extended `clone_args`
struct). glibc 2.34+ prefers `clone3` when available; both ABIs must be
supported.

In scope:
- `clone` (56), `clone3` (435) with the pthread flag set
- `futex` (202): `FUTEX_WAIT`, `FUTEX_WAKE`, `FUTEX_REQUEUE`,
  `FUTEX_WAIT_BITSET`, `FUTEX_WAKE_BITSET` (private and shared flags)
- `set_tid_address`, `set_robust_list` (real per-thread storage, not a no-op)
- `gettid`, `getpid` (per-thread TID, shared PID)
- `arch_prctl(ARCH_SET_FS / GET_FS)` (per-thread, not a global MSR)
- `sched_yield`
- `exit` (60) — exit a single thread, not the whole kernel
- `exit_group` (231) — exit all threads
- `tkill`, `tgkill` for the glibc exit handshake
- Multi-vCPU bring-up in `sumi-vm` with a **fixed** vCPU count
- **Preemptive M:N scheduler** in the kernel, driven by a periodic LAPIC
  timer from day one
- Per-CPU runqueues with work stealing
- Minimal IDT + LAPIC timer + `preempt_count`-based preemption discipline
- Protecting global kernel structures (allocator, page tables, FD table,
  VMA table), including correct lock discipline with preemption enabled

Out of scope (deferred to a separate document):
- Real signal delivery between threads
- Process group, session, ptrace
- Cgroups, namespaces
- `vfork`, `fork` (not needed for pthread; no MMU isolation)
- Real-time scheduling, affinity (`sched_setaffinity` / `sched_getaffinity`)
- CPU hot-plug (the vCPU count is fixed at boot)

### 1.2. Definition of done

By the end of all phases these tests must pass:
- `sumi-integration-tests/data/syscalls/clone_basic.rs` — raw `clone` with
  `CLONE_VM`
- `sumi-integration-tests/data/syscalls/futex_wait_wake.rs` — two threads,
  futex ping-pong
- `sumi-integration-tests/data/syscalls/sched_yield.rs` — two kernel threads
  yielding
- `sumi-integration-tests/data/glibc/pthread_create_join.c`
- `sumi-integration-tests/data/glibc/pthread_mutex.c`
- `sumi-integration-tests/data/glibc/pthread_cond.c`
- `sumi-integration-tests/data/rust_std/thread_spawn.rs` — `std::thread::spawn`
  + `join`

## 2. Current state

### 2.1. Codebase (single-threaded)

| Aspect | File | Current behaviour |
|---|---|---|
| Kernel entry | [sumi-kernel/src/kernel_main.rs](sumi-kernel/src/kernel_main.rs) | `_start` → init MSR → init FD → init virtio → `exec_user_program` → `halt_forever`. No AP boot, no idle loop. |
| Global state | [sumi-kernel/src/lib.rs](sumi-kernel/src/lib.rs) | `PAGE_ALLOCATOR`, `KERNEL_ALLOCATOR`, `KERNEL_PAGE_TABLE` (`spin::Mutex`), `FD_TABLE`, `VMA_TABLE`, `BRK_BASE`/`BRK_CURRENT`/`MMAP_NEXT`. Already behind mutexes but never contended. |
| Syscall entry | [sumi-kernel/src/arch/x86_64/syscall.rs](sumi-kernel/src/arch/x86_64/syscall.rs) | Single global 64 KB `SYSCALL_STACK`, single `SAVED_USER_RSP`. **Not multi-vCPU safe** — two simultaneous syscalls on different vCPUs would corrupt each other's stack. |
| `arch_prctl(ARCH_SET_FS)` | [sumi-kernel/src/syscall/handlers/process.rs](sumi-kernel/src/syscall/handlers/process.rs) | Writes MSR `IA32_FS_BASE` directly — globally. With multiple vCPUs each MSR is per-CPU, so the semantics become "FS of whatever CPU we ran on", which is wrong once a thread migrates. |
| `gettid` | [sumi-kernel/src/syscall/handlers/process.rs](sumi-kernel/src/syscall/handlers/process.rs) | Hardcoded `1`. |
| `set_tid_address` | [sumi-kernel/src/syscall/handlers/process.rs](sumi-kernel/src/syscall/handlers/process.rs) | Hardcoded `1`; pointer ignored → `CLONE_CHILD_CLEARTID` broken → `pthread_join` would hang forever. |
| `futex` | [sumi-kernel/src/syscall/handlers/thread.rs](sumi-kernel/src/syscall/handlers/thread.rs) | One function: WAIT on matching val → 0, WAKE → 0. No wait queues. |
| `clone` / `clone3` | [sumi-kernel/src/syscall/mod.rs](sumi-kernel/src/syscall/mod.rs) | **Not implemented** — fall through to `ENOSYS`. |
| `exit` | [sumi-kernel/src/syscall/handlers/process.rs](sumi-kernel/src/syscall/handlers/process.rs) | `kprintln!("[exit] code={}")` + `halt_forever()` — stops the only CPU. |
| Stack | [sumi-abi/src/arch/x86_64/layout.rs](sumi-abi/src/arch/x86_64/layout.rs) | A single 32 KB kernel stack at a fixed address `KERNEL_STACK`. |
| Multi-vCPU in sumi-vm | [sumi-vm/src/vm.rs](sumi-vm/src/vm.rs), [sumi-vm/src/cmd/run.rs](sumi-vm/src/cmd/run.rs) | The code **already** knows how to run multiple vCPUs as a `Vec<KvmVCpu>`, each on its own `std::thread`. `vcpu_count` is currently hard-coded to `1`. KVM `create_vm` + `create_vcpu` support multiple vCPUs; CR3 (the shared PML4), guest memory, and devices are already shared via `Arc<Mutex<...>>`. |

### 2.2. What already works in our favour

1. **The address space is single.** `clone(CLONE_VM)` is a no-op for memory:
   the new thread immediately sees the same memory. There is no `mm_struct`,
   no `dup_mm`, no `copy_page_range`.
2. **`spin::Mutex` is already in place** on every global structure. Today they
   never block, but the moment a second vCPU starts running they become real.
3. **`sumi-vm` already supports multi-vCPU** at the level of creation and
   `std::thread::spawn`.
4. **CR3 is shared** across all vCPUs out of the box (see `KvmVCpu::init`:
   `sregs.cr3 = DIRECT_MAP_PML4`).
5. **Devices are shared** through `Arc<Mutex<DeviceRegistry>>`.

### 2.3. What blocks a naive multi-vCPU path today

1. `SYSCALL_STACK` is global — two concurrent syscalls would corrupt each
   other.
2. `SAVED_USER_RSP` is a single global u64, same problem.
3. `ARCH_SET_FS` writes the MSR directly — the MSR is per-CPU but the
   **semantic** we want is "FS base of *this thread*", which requires
   per-thread storage that is restored on every context switch.
4. `exit` / `halt_forever` kills the only CPU and does not notify the others.
5. There is no `Thread` struct, no TID allocator, no thread registry.
6. `futex` has no wait queues.
7. There is no scheduler, no context switch, no idle loop.

## 3. Thread model

### 3.1. `Thread` struct

File: new `sumi-kernel/src/sched/thread.rs`.

```rust
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, AtomicPtr, Ordering};
use sumi_abi::address::{PhysicalAddr, VirtualAddr};
use alloc::sync::Arc;

/// Thread identifier (= Linux TID). 0 is reserved; the main thread gets 1.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(transparent)]
pub struct Tid(pub u32);

/// Scheduler state. Transitions are made with CAS; see §6.6.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum ThreadState {
    /// Just created, not yet in a runqueue.
    New = 0,
    /// In some CPU's runqueue (or currently running).
    Runnable = 1,
    /// Currently executing on a vCPU. `current_thread()` returns this.
    Running = 2,
    /// Not in any runqueue; owner parked itself via `sched::block()`.
    /// Will become Runnable again via `sched::wake_blocked()`.
    Blocked = 3,
    /// Thread ran `exit`; awaiting reaper.
    Exited = 4,
}

/// Callee-saved register context for cooperative in-kernel context switch.
/// Written/read only by `__switch_to_asm` — never by normal Rust code, hence
/// `UnsafeCell`. FPU state lives in a separately-allocated XSAVE area pointed
/// to by `xsave_area` (see §6.4).
#[repr(C)]
pub struct ThreadContext {
    pub rsp:    u64,
    pub rbp:    u64,
    pub rbx:    u64,
    pub r12:    u64,
    pub r13:    u64,
    pub r14:    u64,
    pub r15:    u64,
    pub rflags: u64,
    /// Pointer to the per-thread XSAVE area (64-byte aligned, sized per
    /// `cpuid.leaf(0xD).ecx`). Allocated once at thread creation.
    pub xsave_area: u64,
}

/// Intrusive link nodes. Each Thread can be in at most one runqueue or one
/// wait queue at a time, so one of each link is enough.
#[repr(C)]
pub struct RunLink {
    pub next: AtomicPtr<Thread>,
    pub prev: AtomicPtr<Thread>,
}

#[repr(C)]
pub struct WaitLink {
    pub next: AtomicPtr<Thread>,
    pub uaddr: AtomicU64,
    pub bitset: AtomicU32,
}

/// Per-thread control block. Owned through `Arc<Thread>`. ~512 bytes,
/// cache-line aligned.
#[repr(C, align(64))]
pub struct Thread {
    pub tid:  Tid,
    /// PID = TID of the thread group leader.
    pub tgid: Tid,
    pub state: AtomicU32,        // ThreadState
    pub exit_code: AtomicI32,

    /// Callee-saved context, touched only by `__switch_to_asm`.
    pub ctx: UnsafeCell<ThreadContext>,

    /// Per-thread kernel stack (top of stack, virtual). Page-allocated.
    pub kernel_stack_top:  VirtualAddr,
    pub kernel_stack_phys: PhysicalAddr,
    pub kernel_stack_size: usize,

    /// User stack region. For the main thread these come from exec; for
    /// children they come from the `child_stack` argument to `clone`.
    pub user_stack_base: VirtualAddr,
    pub user_stack_size: usize,

    /// FS base as set by `arch_prctl(ARCH_SET_FS)` or `CLONE_SETTLS`.
    /// Written to MSR `IA32_FS_BASE` by `__switch_to_asm` on every switch.
    pub fs_base: AtomicU64,

    /// `CLONE_CHILD_CLEARTID`: user address to zero + `FUTEX_WAKE` at exit.
    pub clear_child_tid:  AtomicU64,
    /// `set_robust_list` head; currently stored but not walked.
    pub robust_list_head: AtomicU64,

    /// Which CPU the thread is currently running on (or last ran on).
    /// Updated by the scheduler on context switch. `u32::MAX` = never ran.
    pub cpu: AtomicU32,

    /// Intrusive runqueue and wait-queue links.
    pub run_link:  RunLink,
    pub wait_link: WaitLink,

    /// Bucket pointer while blocked on a futex — used to remove self on
    /// cancellation. Null when not blocked.
    pub futex_bucket: AtomicPtr<FutexBucket>,
}

// SAFETY: every field is either atomic, immutable after creation, or accessed
// only through `__switch_to_asm` with interrupts disabled on this CPU.
unsafe impl Sync for Thread {}
unsafe impl Send for Thread {}
```

### 3.2. Lifecycle

```
 clone() /           schedule() picks it             futex_wait / block
 main thread spawn   ─────────────────────────►
 ──────────► [New] ─► [Runnable] ───────────► [Running] ──────────────► [Blocked]
                         ▲                        │                         │
                         │                        │ sched_yield / preempt   │
                         │◄───────────────────────┘                         │
                         │                                                  │
                         │               futex_wake / wake_blocked          │
                         └──────────────────────────────────────────────────┘
                                                  │
                                                  │ exit()
                                                  ▼
                                               [Exited] ──► reaper frees stack+Arc
```

- **New → Runnable**: `sys_clone` finishes building the kernel stack frame
  and calls `sched::wake_new(thread)` which enqueues on some CPU's runqueue.
- **Runnable → Running**: `schedule()` on a vCPU pops the thread from its
  runqueue, performs `__switch_to_asm(prev, next)`. `next.state = Running`.
- **Running → Runnable**: voluntary `sched_yield`, end of timeslice if
  preemption is enabled, or end of a syscall when a higher-priority wakeup
  set `need_resched`.
- **Running → Blocked**: the thread calls `sched::block()` after atomically
  transitioning its state and adding itself to a wait queue (futex, etc).
  Then `schedule()` picks the next runnable (or idle).
- **Blocked → Runnable**: another thread removes this one from the wait
  queue, CAS'es state `Blocked → Runnable`, pushes it onto a runqueue (its
  last-run CPU for cache warmth), and sends an IPI if that CPU is idle.
- **Running → Exited**: `sys_exit` runs `CLONE_CHILD_CLEARTID` handshake,
  marks itself `Exited`, pushes itself onto the zombie list, and calls
  `schedule()` for the last time. The reaper on another vCPU frees the
  kernel stack (we cannot free our own stack before context-switching off).

### 3.3. Storage — `ThreadRegistry`

File: new `sumi-kernel/src/sched/registry.rs`.

```rust
pub struct ThreadRegistry {
    by_tid: BTreeMap<u32, Arc<Thread>>,   // keyed by the raw TID, not `Tid`
    next_tid: u32,
}

impl ThreadRegistry {
    pub const fn new() -> Self {
        Self { by_tid: BTreeMap::new(), next_tid: 2 }   // 1 = main
    }
    pub fn alloc_tid(&mut self) -> Tid { /* ... */ }
    pub fn register(&mut self, t: Arc<Thread>) { /* ... */ }
    pub fn lookup(&self, tid: Tid) -> Option<Arc<Thread>> { /* ... */ }
    pub fn unregister(&mut self, tid: Tid) -> Option<Arc<Thread>> { /* ... */ }
    pub fn alive_count(&self) -> usize { self.by_tid.len() }
}

pub static THREAD_REGISTRY: spin::Mutex<ThreadRegistry> =
    spin::Mutex::new(ThreadRegistry::new());
```

`BTreeMap<u32, _>` (not `BTreeMap<Tid, _>` — `Tid` doesn't implement `Ord`)
is chosen over `HashMap` because (1) it is `no_std`-ready, (2) we
expect a few hundred live threads at most, (3) it avoids hash-flooding attack
surface (not that we have adversaries, but determinism is nice). TID 1 (the
BSP main thread) is inserted via a dedicated `register_main`, not
`register`, so that `alloc_tid` starting at 2 can never collide with it.

### 3.4. Per-CPU state

GS base on every vCPU points at a dedicated `PerCpu` struct:

```rust
// Offsets 0/8/16/24 are frozen by the syscall_entry asm (const-asserted
// in percpu.rs); fields after cpu_id can be reordered freely.
#[repr(C, align(64))]
pub struct PerCpu {
    /// Self pointer — lets `this_cpu()` do a single `mov reg, gs:[0]`.
    pub self_ptr: *const PerCpu,
    /// Top of this CPU's syscall stack. No longer read by syscall_entry
    /// since Phase 4 (per-thread stack switch replaced it); kept to avoid
    /// shifting the frozen offsets.
    pub syscall_stack_top: u64,
    /// Scratch used by syscall_entry to save the user RSP.
    pub saved_user_rsp: u64,
    pub cpu_id: u32,

    /// The thread currently running on this CPU. Always non-null after
    /// `init_phase3_bsp`/`init_phase3_ap` (the idle thread is used when
    /// nothing else is runnable). `AtomicPtr`, not a plain pointer — other
    /// CPUs read this (e.g. the reaper's `is_running_on_any_cpu`).
    pub current_thread: AtomicPtr<Thread>,
    /// Idle thread for this CPU. Also `AtomicPtr`: written once at init,
    /// read on every `schedule()`.
    pub idle_thread: AtomicPtr<Thread>,
    /// Set by a remote CPU via `wake_blocked` or by the preemption tick
    /// ISR to signal "call `schedule()` at the next preemption point".
    pub need_resched: AtomicBool,
    /// `true` iff the CPU is currently in the idle loop (in `hlt`).
    /// Checked by `wake_blocked` to decide whether to send an IPI.
    pub is_idle: AtomicBool,
    /// Per-CPU runqueue. Protected by its own spinlock.
    pub runqueue: RunQueue,
    /// Last TLB generation this CPU has flushed to (§8.3).
    pub tlb_generation: AtomicU64,
    /// Preemption disable counter. Incremented on every spinlock acquire
    /// and on IRQ entry; decremented on release/exit. `schedule()` from
    /// the timer ISR is only allowed when this is 0. See §6.7.
    /// `UnsafeCell<u32>` (not `AtomicU32`) because it is only touched by
    /// *this* CPU with interrupts disabled during the read-modify-write.
    pub preempt_count: UnsafeCell<u32>,
}

pub fn this_cpu() -> &'static PerCpu {
    // SAFETY: IA32_GS_BASE is set by `init_for_cpu` before any other
    // kernel code runs on this CPU, and never changes afterward.
    unsafe {
        let pc: *const PerCpu;
        core::arch::asm!("mov {}, gs:0", out(reg) pc,
                         options(nostack, preserves_flags, readonly));
        &*pc
    }
}

pub fn current_thread() -> &'static Thread {
    unsafe { &*this_cpu().current_thread.load(Ordering::Relaxed) }
}
```

The `PerCpu` array lives in a static `[PerCpu; MAX_VCPUS]` in `sched::percpu`
and is initialised by `_start` (CPU 0) and `ap_main` (CPUs 1..N).

## 4. vCPU model: fixed N

### 4.1. Decision: fixed vCPU count, M:N scheduling

The vCPU count is **fixed** at VM startup. sumi-vm takes a CLI flag
`--vcpus N` (default = number of host logical CPUs, capped at 64). All N
vCPUs are created with `KVM_CREATE_VCPU` before any `KVM_RUN`, and each runs
on its own `std::thread` for the lifetime of the VM.

Threads inside the guest are scheduled **M:N** onto those N vCPUs by an
in-kernel scheduler (§6). M is unbounded (a few hundred is a reasonable
working point); N is small (typically 1..16).

### 4.2. Why M:N, not 1:1 or a parked-vCPU pool

The previous design (v1) proposed 1:1 with a pool of pre-created, parked
vCPUs. That design was rejected because:

1. **KVM vCPU is an expensive resource.** Each vCPU holds a VMCS, an FPU
   save area, a LAPIC state page, a KVM_RUN mmap region, and pins a host
   pthread. A pool of 32 idle vCPUs wastes ~1–2 MB per vCPU and 32 host
   threads even when the program is single-threaded.
2. **Pool size is a hard cap.** A real program that spawns 200 short-lived
   pthreads would hit `EAGAIN` at `pthread_create` even though the host has
   plenty of CPU.
3. **Thread spawn becomes a hypercall round-trip.** Every `clone()` requires
   host coordination to pick a slot, set registers, and signal a condvar —
   this is orders of magnitude more expensive than an in-guest context
   switch.
4. **The symmetric argument for avoiding an in-kernel scheduler** (no IDT,
   no timer, no save/restore) is weaker than it looks: we already need an
   IDT for page faults / #GP debug anyway (today triple-faulting is the
   "handling"), FS base save/restore is just an MSR write, and the
   GP register set fits in 64 bytes.

With M:N the guest owns scheduling policy, `clone()` is cheap (it is a
kernel-side data structure op plus enqueue), and vCPUs are a fixed,
well-bounded resource.

### 4.3. Bring-up of the N vCPUs

CPU 0 is the bootstrap processor (BSP) and runs the existing `_start` path.
CPUs 1..N-1 (application processors, APs) enter at a new `ap_start`
stub, implemented as a `core::arch::global_asm!` block in `ap_start.rs`
rather than a separate `.S` file, ending in `call ap_main_rust`.

Files:
- New `sumi-kernel/src/arch/x86_64/ap_start.rs` (`global_asm!`: load GDT,
  CR3, GS base, boot stack from `AP_BOOT_STACKS[cpu_id]`, `call
  ap_main_rust`).
- New `sumi-kernel/src/arch/x86_64/smp.rs` with `ap_main_rust(cpu_id: u32) -> !`.
- [sumi-vm/src/vm.rs](sumi-vm/src/vm.rs) — already spawns per-vCPU host
  threads; extend to set the initial RIP/RSP/GS base for APs from a
  `BootInfo.ap_entries: [ApEntry; MAX_VCPUS]` table prepared by the loader.

`ap_main_rust` (`sumi-kernel/src/arch/x86_64/smp.rs`):

```rust
#[unsafe(no_mangle)]
pub extern "C" fn ap_main_rust(cpu_id: u32) -> ! {
    sched::init_for_cpu(cpu_id);               // writes IA32_GS_BASE first
    crate::arch::x86_64::syscall::init();      // per-CPU LSTAR/STAR/SFMASK
    crate::arch::x86_64::tss::init_and_load(cpu_id);
    crate::arch::x86_64::idt::load();          // shared IDT, per-CPU IST1
    crate::arch::x86_64::lapic::init();        // this AP's periodic timer
    kprintln!("[ap] cpu {cpu_id} online");
    while !KERNEL_READY.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    crate::sched::init_phase3_ap(cpu_id)       // builds idle thread, -> idle_loop()
}
```

No AP is allowed to run user code until the BSP publishes
`KERNEL_READY: AtomicBool = true`. The BSP publishes this **after** it has
finished virtio, FD, VMA, allocator init and loaded the user program image.
APs spin on `KERNEL_READY` in `ap_main` before calling `idle_loop`, so no
racy mid-init user thread can land on an unready AP.

### 4.4. sumi-vm changes for parallel vCPUs

`sumi-vm` today constructs `Vec<KvmVCpu>` and `std::thread::spawn`s one
host thread per vCPU. That structure is kept; changes are:

1. [sumi-vm/src/cmd/run.rs](sumi-vm/src/cmd/run.rs): `--vcpus N` flag
   (default `num_cpus::get()`, clamped to 64).
2. [sumi-vm/src/vm.rs](sumi-vm/src/vm.rs): write `BootInfo.num_cpus = N` and
   the `ap_entries` table into guest memory before any `KVM_RUN`.
3. The BSP's host thread runs `KVM_RUN` starting at `_start`. AP host
   threads run `KVM_RUN` starting at `ap_start`. All vCPU host threads run
   concurrently.
4. **Global state that is still serialised per-VM** (TAP devices, virtio
   queue heads, `DeviceRegistry`) must be examined for lock granularity:
   `Arc<Mutex<DeviceRegistry>>` is fine for correctness but becomes a
   contention point under MMIO storms. Per-device locks are a follow-up,
   not a blocker.
5. **Hypercalls are now concurrent.** sumi-vm must handle
   `VcpuExit::Hypercall` on any host thread and can no longer assume the
   caller is vCPU 0.

### 4.5. Hypercall interface (shrunken)

With M:N most scheduling happens inside the guest, so the hypercall list
gets shorter than v1. The mechanism is **MMIO-only**, not
`KVM_CAP_EXIT_HYPERCALL`/`vmcall`: each hypercall is a single 8-byte
little-endian MMIO write to `HYPERCALL_MMIO_BASE + offset`, where the
offset (not a small opcode number) is the selector. Defined in
`sumi-abi/src/hypercall.rs`:

| Offset | Name            | Args (write payload)          | Semantics |
|--------|-----------------|--------------------------------|-----------|
| 0x00   | `HC_KICK_CPU`   | target cpu_id                 | Host sends `pthread_kill(SIGUSR1)` to the target vCPU host thread, forcing `KVM_RUN` to return so the guest idle thread can re-check its runqueue. |
| 0x08   | `HC_TLB_FLUSH`  | cpu mask                      | Host pokes each target CPU via SIGUSR1; the guest handler (or the next CR3 reload) flushes TLB. |
| 0x10   | `HC_SHUTDOWN`   | i32 exit code (zero-extended) | Terminate the VM (exit_group path). |

`HC_STRIDE = 0x08` and `HYPERCALL_MMIO_SIZE = 0x1000` (one page, 512
possible slots). There is no return-value channel — hypercalls act by side
effect only. There is **no** `HC_SPAWN_VCPU`, **no** `HC_FUTEX_WAIT`, **no**
`HC_PARK_VCPU`. Wait/wake, idle, and spawn are all handled inside the guest
by the scheduler. Idle is guest-side `hlt` → `KVM_EXIT_HLT` (handled by
simply re-entering KVM_RUN after a blocking `KVM_RUN`, which naturally
parks the host pthread in the host kernel).

## 5. `clone()` syscall

### 5.1. Supported flags

```rust
const CLONE_VM:              u64 = 0x0000_0100;  // mandatory
const CLONE_FS:              u64 = 0x0000_0200;  // mandatory
const CLONE_FILES:           u64 = 0x0000_0400;  // mandatory
const CLONE_SIGHAND:         u64 = 0x0000_0800;  // mandatory
const CLONE_PARENT:          u64 = 0x0000_8000;  // ignored (no parent tracking)
const CLONE_THREAD:          u64 = 0x0001_0000;  // mandatory
const CLONE_SYSVSEM:         u64 = 0x0004_0000;  // ignored
const CLONE_SETTLS:          u64 = 0x0008_0000;  // FS base = tls arg
const CLONE_PARENT_SETTID:   u64 = 0x0010_0000;  // *parent_tid = new tid
const CLONE_CHILD_CLEARTID:  u64 = 0x0020_0000;  // store, used at exit
const CLONE_DETACHED:        u64 = 0x0040_0000;  // ignored
const CLONE_CHILD_SETTID:    u64 = 0x0100_0000;  // *child_tid = new tid
```

`CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD` is
trivially satisfied in a unikernel. **If any of the five is missing we
return `EINVAL`** — we refuse to silently degrade into `fork`-like
semantics.

### 5.2. Signature

Linux:
`long clone(unsigned long flags, void *stack, int *parent_tid, int *child_tid, unsigned long tls)`

x86-64 ABI: `flags=rdi, stack=rsi, parent_tid=rdx, child_tid=r10, tls=r8`.
Returns: new TID in parent, 0 in child. This means the kernel must make
**the child's first return from the syscall observe `rax = 0`** — without
running sys_clone at all on that side. With the M:N scheduler this is done
by building a fake kernel stack frame for the child that, when
`__switch_to_asm` restores it, jumps into a small trampoline
`thread_entry` which restores the user register frame (rax=0, rip=caller
rip, rsp=child_stack, rflags=caller rflags, fs=tls) and executes
`sysretq`.

### 5.3. `sys_clone` algorithm

File: new `sumi-kernel/src/syscall/handlers/clone.rs`.

```rust
pub fn sys_clone(args: &SyscallArgs) -> SyscallResult {
    let flags    = args.arg0;
    let stack    = args.arg1;                 // top of new user stack
    let ptid_ptr = args.arg2 as *mut i32;
    let ctid_ptr = args.arg3 as *mut i32;
    let new_tls  = args.arg4;                 // FS base for child

    // 1. Validate: the full pthread set is mandatory.
    const REQUIRED: u64 = CLONE_VM | CLONE_FS | CLONE_FILES
                        | CLONE_SIGHAND | CLONE_THREAD;
    if flags & REQUIRED != REQUIRED { return EINVAL; }
    if stack == 0 { return EINVAL; }

    // 2. Allocate kernel stack + Thread control block + TID.
    let kstack_phys = PAGE_ALLOCATOR.lock().alloc(1)?;
    let kstack_top  = kstack_phys
        .to_virtual(&KERNEL_DIRECT_MAP)
        .add(KERNEL_STACK_BYTES);

    let parent = current_thread();
    let tid = THREAD_REGISTRY.lock().alloc_tid();

    let thread = Arc::new(Thread {
        tid,
        tgid: parent.tgid,
        state: AtomicU32::new(ThreadState::New as u32),
        exit_code: AtomicI32::new(0),
        ctx: UnsafeCell::new(ThreadContext::zeroed()),
        kernel_stack_top: kstack_top,
        kernel_stack_phys: kstack_phys,
        kernel_stack_size: KERNEL_STACK_BYTES,
        user_stack_base: VirtualAddr::new(stack as usize - DEFAULT_PTHREAD_STACK),
        user_stack_size: DEFAULT_PTHREAD_STACK,
        fs_base: AtomicU64::new(
            if flags & CLONE_SETTLS != 0 { new_tls } else { 0 }),
        clear_child_tid: AtomicU64::new(
            if flags & CLONE_CHILD_CLEARTID != 0 { ctid_ptr as u64 } else { 0 }),
        robust_list_head: AtomicU64::new(0),
        cpu: AtomicU32::new(u32::MAX),
        run_link: RunLink::empty(),
        wait_link: WaitLink::empty(),
        futex_bucket: AtomicPtr::new(core::ptr::null_mut()),
    });

    // 3. Build the initial kernel stack frame so that when the scheduler
    //    context-switches onto this thread for the first time, it returns
    //    into `thread_entry_trampoline`, which pops a prepared sysret frame.
    unsafe {
        build_initial_frame(
            &thread,
            InitialFrame {
                entry_user_rip:    args.caller_rip,
                entry_user_rsp:    stack,
                entry_user_rflags: args.caller_rflags,
                entry_user_rax:    0,            // child sees clone() == 0
                entry_fs_base:     thread.fs_base.load(Ordering::Relaxed),
            });
    }

    // 4. Parent-side TID writeback.
    if flags & CLONE_PARENT_SETTID != 0 && !ptid_ptr.is_null() {
        unsafe { ptid_ptr.write(tid.0 as i32); }
    }
    if flags & CLONE_CHILD_SETTID != 0 && !ctid_ptr.is_null() {
        unsafe { ctid_ptr.write(tid.0 as i32); }
    }

    THREAD_REGISTRY.lock().register(thread.clone());

    // 5. Make it runnable. This may IPI an idle CPU.
    sched::wake_new(thread);

    // 6. Return new TID to parent.
    tid.0 as i64
}
```

`build_initial_frame` writes, at the top of the child's kernel stack:

- a saved register frame (as if the child had entered the kernel via
  `syscall_entry` and was about to return): user rip, rflags, rsp, rax, fs;
- a return address equal to `thread_entry_trampoline`;
- zeroes for the callee-saved registers that `__switch_to_asm` will load.

`thread_entry_trampoline` is a small `#[naked]` function that:
1. Pops the saved rax, rip, rflags, rsp, fs from the stack into the right
   MSR / registers,
2. Executes `swapgs` (not — see §7.3, no swapgs), then `sysretq`.

Actually, since we have no ring-3 code path (sumi runs user code in ring 0),
`thread_entry_trampoline` does a simple `ret`-like sequence: restore rflags
via `popfq`, restore rsp, restore rax, and `jmp` (indirect) to the saved
rip. FS base is loaded via `wrmsr IA32_FS_BASE`. All of this is performed
with interrupts disabled and on the thread's own kernel stack.

### 5.4. Caller RIP / RFLAGS

`syscall_entry` (in [sumi-kernel/src/arch/x86_64/syscall.rs](sumi-kernel/src/arch/x86_64/syscall.rs))
saves RCX (the user-space return RIP) and R11 (user RFLAGS). We extend
`SyscallArgs` with `caller_rip: u64, caller_rflags: u64` populated from
those saved slots before calling `syscall_dispatch`. This is a targeted
rewrite of the syscall entry asm.

### 5.5. `clone3`

`clone3(&clone_args, sizeof(clone_args))` takes:
```c
struct clone_args {
    u64 flags;
    u64 pidfd;
    u64 child_tid;
    u64 parent_tid;
    u64 exit_signal;
    u64 stack;        // NOTE: stack *base* here, not top
    u64 stack_size;
    u64 tls;
    ...
};
```

Implementation: copy the struct, translate `stack + stack_size` into the
same "child_stack top" value that `clone` receives, and dispatch to a
shared `do_clone(flags, stack_top, ptid, ctid, tls, caller_rip, caller_rflags)`
helper.

## 6. Scheduler

### 6.1. Runqueue topology: per-CPU with work stealing

**Decision: per-CPU runqueue, work-stealing.**

Alternative considered: single global runqueue protected by a `spin::Mutex`.
Rejected because:
- Every `wake_blocked`, every `schedule()`, every `wake_new` would
  serialise on one spinlock.
- With N=8 and a futex ping-pong workload the lock would dominate CPU time.

Per-CPU chosen because:
- Local ops (enqueue from `wake_new` pinning to current CPU, dequeue on
  `schedule()`) take only the owning CPU's spinlock, so N CPUs can work
  in parallel with zero cross-CPU traffic in the common case.
- Work stealing gives us load balancing for free when a CPU goes idle.

Global-queue variant could become attractive only if M is very small or
the workload is dominated by wakeups crossing CPUs; for M in the few
hundreds it is strictly worse.

### 6.2. `RunQueue` data structure

File: `sumi-kernel/src/sched/runqueue.rs`.

```rust
pub struct RunQueue {
    /// Intrusive doubly-linked list via `Thread.run_link`. Head = pop first.
    inner: spin::Mutex<RunQueueInner>,
    /// Load hint. Updated under `inner`'s lock on every push/pop; sampled
    /// WITHOUT the lock by the idle path and `try_steal_work` as a cheap
    /// "is there anything here?" check before paying for the lock.
    load: AtomicUsize,
}

struct RunQueueInner {
    head: *mut Thread,
    tail: *mut Thread,
}

impl RunQueue {
    pub fn push(&self, t: &Thread) { /* append at tail */ }
    pub fn pop(&self) -> Option<*mut Thread> { /* remove head (FIFO) */ }
    pub fn load(&self) -> usize { self.load.load(Ordering::Relaxed) }
}
```

FIFO only — no `push_front`, no `steal_half`/`StolenChain`. Preemption
(§6.7) re-enqueues a preempted thread at the tail via the ordinary `push`;
there is no separate "put back at the front" path. Good enough for v1;
can be replaced with CFS-style vruntime later.

### 6.3. Context switch — `__switch_to_asm`

File: new `sumi-kernel/src/arch/x86_64/switch.rs`.

```rust
/// Switch from `prev` to `next`, loading `next_fs_base` into IA32_FS_BASE
/// as part of the same asm sequence (3-arg SysV: rdi=prev, rsi=next,
/// rdx=next_fs_base). Called with interrupts disabled on this CPU.
#[unsafe(naked)]
pub unsafe extern "C" fn __switch_to_asm(
    prev_ctx: *mut ThreadContext,
    next_ctx: *mut ThreadContext,
    next_fs_base: u64,
) {
    naked_asm!(
        // Save prev callee-saved into *prev_ctx (rdi).
        "mov [rdi + 0x00], rsp",
        "mov [rdi + 0x08], rbp",
        "mov [rdi + 0x10], rbx",
        "mov [rdi + 0x18], r12",
        "mov [rdi + 0x20], r13",
        "mov [rdi + 0x28], r14",
        "mov [rdi + 0x30], r15",
        "pushfq",
        "pop qword ptr [rdi + 0x38]",

        // Restore next callee-saved from *next_ctx (rsi).
        "mov rsp, [rsi + 0x00]",
        "mov rbp, [rsi + 0x08]",
        "mov rbx, [rsi + 0x10]",
        "mov r12, [rsi + 0x18]",
        "mov r13, [rsi + 0x20]",
        "mov r14, [rsi + 0x28]",
        "mov r15, [rsi + 0x30]",
        "push qword ptr [rsi + 0x38]",
        "popfq",

        // Load IA32_FS_BASE = rdx (next_fs_base). wrmsr takes MSR in ECX,
        // value in EDX:EAX.
        "mov rax, rdx",
        "shr rdx, 32",
        "mov ecx, 0xc0000100",    // IA32_FS_BASE
        "wrmsr",

        "ret",      // returns into whatever was on the new stack
    )
}
```

Context save area on the stack = 64 bytes (callee-saved only, offsets
0x00–0x38 above `ThreadContext`). FS base is loaded inline as the 3rd
argument rather than "around" the call, so there is exactly one place
(the asm) that knows the FS-base wire format. XSAVE/XRSTOR of
`ThreadContext.xsave_area` is not yet wired into `__switch_to_asm` itself
(see §6.4 for the target design landing under F7). The scheduler
`schedule()` function is the only caller:

```rust
pub fn schedule() {
    let cpu = percpu::this_cpu();
    let prev_ptr = cpu.current_thread.load(Ordering::Relaxed);

    // Consume the reschedule request before popping, so a racing wake
    // either lands in the runqueue before pop (and we see it) or after
    // (and sets need_resched again for the next call). No wake is lost.
    cpu.need_resched.store(false, Ordering::Release);

    let next_ptr: *mut Thread = cpu.runqueue.pop()
        .unwrap_or_else(|| cpu.idle_thread.load(Ordering::Relaxed));

    if core::ptr::eq(next_ptr, prev_ptr) {
        return;                             // self-reschedule, queue empty
    }

    let prev: &Thread = unsafe { &*prev_ptr };
    let next: &Thread = unsafe { &*next_ptr };

    // Demote prev Running -> Runnable, but only if it is still Running (it
    // may already be Blocked/Exited) and it is not the idle thread (idle
    // stays Running while on-CPU).
    let idle_ptr = cpu.idle_thread.load(Ordering::Relaxed);
    if !core::ptr::eq(prev_ptr as *const _, idle_ptr as *const _) {
        let _ = prev.state.compare_exchange(
            ThreadState::Running as u32,
            ThreadState::Runnable as u32,
            Ordering::AcqRel, Ordering::Relaxed);
    }

    next.state.store(ThreadState::Running as u32, Ordering::Release);
    next.cpu.store(cpu.cpu_id, Ordering::Relaxed);
    cpu.current_thread.store(next_ptr, Ordering::Release);

    let next_fs_base = next.fs_base.load(Ordering::Relaxed);
    unsafe { __switch_to_asm(prev.ctx.get(), next.ctx.get(), next_fs_base); }

    // When control returns here, we are back on `prev`'s stack after some
    // future schedule() picks us again. Reap any zombies no longer current
    // on any CPU (see §10.1).
    reaper::reap_zombies();
}
```

### 6.4. FPU/SSE state

Decision for v1: **eager XSAVE** on every context switch. Simple and
correct. Target design (landing under fix F7): a per-thread XSAVE area
allocated at thread creation with the size reported by
`cpuid.leaf(0xD).ecx` (typically 1088 bytes for XSAVE with AVX);
`ThreadContext.xsave_area` (`sched/thread.rs`) holds the pointer, and
`xsave`/`xrstor` run around the callee-saved save/restore in
`__switch_to_asm` (§6.3). As of this revision `xsave_area` is allocated
in the `ThreadContext` layout but always written `0` and never read —
`__switch_to_asm` does not yet issue `xsave`/`xrstor` (see §6.3's note).

Lazy FPU (set CR0.TS on switch, save on first #NM) is a follow-up
optimisation. It requires an #NM handler in the IDT which we can add
together with preemption (§6.7).

### 6.5. Idle loop

Each CPU has an `idle_thread` that owns its own 16 KB kernel stack and
runs:

```rust
pub fn idle_loop() -> ! {
    loop {
        let cpu = percpu::this_cpu();
        cpu.is_idle.store(true, Ordering::Release);
        // Re-check the runqueue after publishing `is_idle`, to avoid a
        // wakeup lost between our last check and our flag publish.
        if cpu.runqueue.load() > 0 {
            cpu.is_idle.store(false, Ordering::Release);
            schedule();
            continue;
        }
        // Local queue empty — try to steal one thread from a peer (§6.8)
        // before parking.
        if try_steal_work() {
            cpu.is_idle.store(false, Ordering::Release);
            schedule();
            continue;
        }
        unsafe { core::arch::asm!("sti; hlt", options(nomem, nostack)); }
        // Woken by IPI or by a local interrupt. Loop around; the wake
        // path will have set `need_resched`.
        cpu.is_idle.store(false, Ordering::Release);
    }
}
```

`hlt` traps to `KVM_EXIT_HLT`, and sumi-vm simply re-enters `KVM_RUN`,
which parks the host pthread in the host kernel until the next event
(MSI / signal / timer).

### 6.6. Blocking and waking — unconditional store under the bucket lock

There is no separate `try_block()` CAS step. Blocking is done inline by the
caller (currently only `sched::futex::wait`/`wait_bitset`) while still
holding the wait-queue lock, as an **unconditional** `store`, not a CAS:

```rust
// (sched/futex.rs::wait, simplified)
let g = bucket.lock.lock();
if unsafe { AtomicU32::from_ptr(uaddr as *mut u32).load(Ordering::Acquire) } != expected {
    drop(g);
    return EAGAIN;
}
bucket_push(bucket, me);                 // publish presence in the wait queue
me.state.store(ThreadState::Blocked as u32, Ordering::Release);
drop(g);                                 // lock released only AFTER Blocked is visible

schedule();                              // may pick something else, or idle
```

This is safe *because* the transition happens under the same lock the waker
takes: a CAS would only be needed if the waiter could be marked `Blocked`
concurrently with a wake, and this protocol makes that impossible by
construction (see the ordering rules below). `debug_assert!` in
`futex::wake`/`wake_bitset` checks every dequeued waiter is `Blocked` —
if the assertion fires, the lock-ordering discipline was violated.

Waking is done by `sched::wake_blocked`, which *does* CAS (a woken thread
may already have been raced by another waker, or — per F3 below — still be
mid-switch off the CPU):

```rust
pub fn wake_blocked(t: &Thread) {
    // CAS Blocked -> Runnable. If it is already Runnable, someone else
    // woke us first; nothing to do.
    if t.state.compare_exchange(
            ThreadState::Blocked as u32,
            ThreadState::Runnable as u32,
            Ordering::AcqRel, Ordering::Relaxed).is_err() {
        return;
    }
    // Enqueue on the CPU it last ran on, for cache warmth.
    let home = t.cpu.load(Ordering::Relaxed);
    let target = percpu::get(home).unwrap_or_else(percpu::this_cpu);
    target.runqueue.push(t);
    target.need_resched.store(true, Ordering::Release);
    // Only kick a remote, currently-idle CPU — never ourselves.
    if target.cpu_id != percpu::this_cpu().cpu_id
        && target.is_idle.load(Ordering::Acquire) {
        hypercall::kick_cpu(target.cpu_id);
    }
}
```

The ordering rules are:
1. The waiter publishes its presence in the wait queue (`bucket_push`)
   *before* storing `Blocked`, and both happen under the bucket lock.
2. A waker removes the thread from the wait queue under the same bucket
   lock **before** CAS-ing `Blocked → Runnable` in `wake_blocked`.
3. These two together prevent the lost-wakeup problem: either the waker
   sees the waiter in the queue (and wakes it), or the waiter sees the
   updated condition before blocking (and bails out with `EAGAIN`).

**F3/on_cpu (landing alongside this revision).** The protocol above has a
known gap: `wait()` drops the bucket lock *before* `schedule()` actually
saves the waiter's context, so a waker (or a work-stealer, §6.8) can
CAS `Blocked → Runnable` and push the thread onto a runqueue while it is
still executing the tail of its own `schedule()` call on the original CPU —
two CPUs could then `__switch_to_asm` into the same stack. The fix is an
`on_cpu: AtomicBool` on `Thread`: `schedule()` sets it before the switch
and clears it immediately after `__switch_to_asm` returns/the context save
completes; `wake_blocked` and `try_steal_work` (§6.8) must spin until
`on_cpu == false` before enqueuing or running that thread (the Linux
`p->on_cpu` protocol). The same flag closes the matching reaper race
(F4, §10.1): the reaper must not free a thread's stack while `on_cpu` is
still true.

### 6.7. Preemption — mandatory from day one

**Decision: timer-driven preemption is part of the core design, not a
follow-up.** The rationale:

- A tight CPU-bound guest thread that never enters the kernel (e.g. a
  busy loop with a hoisted load) would otherwise starve every other
  thread on its vCPU. In a single-tenant Linux process this might be
  fine; in a unikernel meant to run arbitrary glibc/Rust workloads it is
  a correctness gap.
- `pthread_setcancelstate` / `pthread_testcancel` and even `std::thread`
  parking assume bounded latency wakeups. Pure cooperative makes these
  only "eventually correct".
- The cost is one small IDT, one LAPIC timer init, and `preempt_count`
  discipline around spinlocks — all of which we need anyway for
  debuggable fault handling.

#### 6.7.1. Interrupt infrastructure

Files:
- New `sumi-kernel/src/arch/x86_64/idt.rs` — 256-entry IDT, built once
  at boot, shared across all CPUs. Entries:
  - `0x0E` (#PF), `0x0D` (#GP), `0x06` (#UD), `0x07` (#NM), `0x08` (#DF)
    → diagnostic handlers that print the trapframe and call
    `hypercall::shutdown(-1)`. These exist regardless of preemption,
    but we ship them in the same phase to avoid adding an IDT twice.
  - `0x20` (PIT, unused), `0x40` (APIC timer vector) →
    `timer_interrupt`.
  - `0x41` (IPI vector) → `ipi_interrupt`, used by TLB shootdown (§8.3)
    and as an alternative to `HC_KICK_CPU` for idle-wake once the LAPIC
    path is warm.
- New `sumi-kernel/src/arch/x86_64/lapic.rs` — LAPIC init (via MSR
  `IA32_APIC_BASE` + memory-mapped region; KVM exposes the in-kernel
  LAPIC via `KVM_CREATE_IRQCHIP` in sumi-vm). Configures the LVT Timer
  in periodic mode at **1 ms** (1 kHz tick), writes `0x40` as the
  vector, and acknowledges each interrupt via EOI.
- New `sumi-kernel/src/arch/x86_64/interrupt.rs` — trampolines: save
  full GP frame, increment `preempt_count`, call Rust ISR, decrement
  `preempt_count`, check `need_resched` on exit, call `schedule()` if
  safe, `iretq`.

sumi-vm side:
- [sumi-vm/src/vm.rs](sumi-vm/src/vm.rs) — call `KVM_CREATE_IRQCHIP`
  before `KVM_CREATE_VCPU` to enable the in-kernel LAPIC model.
- No userspace emulation of timer or APIC is needed — KVM delivers the
  timer IRQ directly.

#### 6.7.2. Timer ISR

```rust
pub extern "C" fn timer_interrupt() {
    lapic::eoi();
    // Only the current CPU sets its own need_resched — no atomics needed.
    this_cpu().need_resched.store(true, Ordering::Relaxed);
}
```

The trampoline around this function is where the actual reschedule check
lives:

```rust
// Pseudocode for the IRQ trampoline (see interrupt.rs).
#[unsafe(naked)]
extern "C" fn irq_entry_timer() {
    // save_all_regs
    // preempt_count += 1
    // call timer_interrupt
    // preempt_count -= 1
    // if preempt_count == 0 && need_resched { call schedule_preempt }
    // restore_all_regs; iretq
}
```

`schedule_preempt` is a thin wrapper around `schedule()` that first
clears `need_resched`, then performs the switch. The only caller is
`irq_entry_*` when `preempt_count == 0`.

#### 6.7.3. `preempt_count` discipline

Rules:
1. `spin::Mutex::lock()` calls `preempt_disable()` before acquiring the
   spin; `drop` calls `preempt_enable()` after release. This makes every
   critical section non-preemptible, which avoids the classic "preempted
   while holding a spinlock, another thread on this CPU tries to acquire
   it, deadlock" scenario.
2. `irq_entry_*` increments `preempt_count` on entry. This is what lets
   nested interrupts Just Work and what lets lock operations inside an
   ISR remain safe.
3. `preempt_enable()` on drop-to-zero checks `need_resched` and, if set
   and we were called from a preemption-safe context (not inside an IRQ
   frame — easy to check, since `preempt_count == 0` implies we are not
   in an IRQ), calls `schedule_preempt`.
4. Syscall return also does a `preempt_enable`-style check: after all
   syscall-path locks are dropped and `preempt_count == 0`, check
   `need_resched` and schedule if set.

The `preempt_count` lives in `PerCpu` (§3.4) and is touched with
interrupts disabled so it needs no atomics.

#### 6.7.4. Voluntary preemption points

In addition to timer preemption, these still exist and still matter:
- `sched_yield` → direct `schedule()`.
- `futex_wait` → `Blocked` store under the bucket lock (§6.6) + `schedule()`.
- End of `wake_blocked` on the *current* CPU → sets `need_resched`, the
  caller will reschedule on its way out.
- Syscall return path with `need_resched == true`.

#### 6.7.5. Interaction with spinlocks

With preemption enabled, code that today does
`let g = X.lock(); /* ... */` must ensure that nothing inside the guarded
block can call `schedule()` — otherwise we would context-switch with a
spinlock held, and another thread on any CPU that wants the same lock
would spin forever (because the holder is off-CPU). Rule: **never call
`schedule()`, `sched_yield`, `futex_wait`, or any function that may
block, while a `spin::Mutex` is held**. This is exactly the Linux rule.
Violations are debug-assertable: `preempt_count > 0` ⇒ `schedule()` is a
bug, so `schedule()` starts with
`debug_assert_eq!(preempt_count(), 0)`.

#### 6.7.6. IRQ save on shared locks

Locks that can be taken both from thread context and from an ISR (the
per-CPU runqueue is the main one — timer ISR → `schedule()` →
`runqueue.pop()`) must disable interrupts on acquire to avoid a deadlock
where thread context holds the lock and the timer ISR on the same CPU
tries to take it. Use `spin::Mutex::lock_irqsave()` (a small addition to
our fork of spin, or a manual `cli` + lock + `sti`). Explicitly
irqsave-only locks: `runqueue.lock`, `ZOMBIE_LIST.lock`. Everything else
stays plain `spin::Mutex`.

### 6.8. Load balancing — work stealing

On idle, `sched::try_steal_work` (`sched/mod.rs`) does not pick the
busiest peer or steal a batch — it steals **one thread from the first
non-empty peer CPU found**, in `cpu_id` order starting after `my_id`:
1. Iterate `id in 0..MAX_VCPUS`, skipping `my_id`, skipping any CPU not
   yet initialised (`percpu::get(id) == None`).
2. Cheap hint check: `peer.runqueue.load() == 0` → skip without locking.
3. Otherwise `peer.runqueue.pop()`. If it returns a thread, reroute its
   `cpu` field to `my_id` (so future wakeups target the right runqueue),
   push it onto our own runqueue, and stop — return `true`.
4. If no peer yields a thread, return `false` and the idle loop `hlt`s.

No batch/half-steal, no busiest-queue selection, no `StolenChain`.
Adequate for the current single-steal-per-idle-wakeup workloads; periodic
rebalancing is not done.

### 6.9. `sched_yield`

```rust
pub fn sys_sched_yield(_: &SyscallArgs) -> SyscallResult {
    sched::schedule();
    0
}
```

## 7. TLS

### 7.1. What glibc does

After `clone` (or from `CLONE_SETTLS`) glibc calls
`arch_prctl(ARCH_SET_FS, tcb_addr)`, then every access of the form
`mov rax, fs:0x28` (stack canary) or `fs:0x10` (TCB → self) starts working.

### 7.2. Per-thread FS base with a scheduler

Because a thread can be scheduled onto different vCPUs over its lifetime,
we cannot keep FS base "in the MSR". `Thread.fs_base` holds the authoritative
value; the context switch code (§6.3) writes `IA32_FS_BASE` from
`next.fs_base` on every switch.

`sys_arch_prctl(ARCH_SET_FS)` updates both:

```rust
pub fn sys_arch_prctl(args: &SyscallArgs) -> SyscallResult {
    match args.arg0 {
        ARCH_SET_FS => {
            let val = args.arg1;
            // SAFETY: ring 0, MSR valid.
            unsafe { wrmsr(IA32_FS_BASE, val); }
            current_thread().fs_base.store(val, Ordering::Relaxed);
            0
        }
        ARCH_GET_FS => {
            let v = current_thread().fs_base.load(Ordering::Relaxed);
            unsafe { *(args.arg1 as *mut u64) = v; }
            0
        }
        _ => EINVAL,
    }
}
```

File: [sumi-kernel/src/syscall/handlers/process.rs](sumi-kernel/src/syscall/handlers/process.rs).

### 7.3. GS base — reserved for the kernel

GS base holds `&PerCpu` on every CPU. This is set once in `_start`
(BSP) / `ap_main` (APs) and **never changed** during execution. Because
everything runs in ring 0, there is no `swapgs` dance, no user/kernel GS
split, and user code that touches GS is illegal (glibc does not).

Context switches do not change GS base either: it is per-CPU, not per-thread.

### 7.4. `set_tid_address`

```rust
pub fn sys_set_tid_address(args: &SyscallArgs) -> SyscallResult {
    let ptr = args.arg0;
    current_thread().clear_child_tid.store(ptr, Ordering::Relaxed);
    current_thread().tid.0 as i64
}
```

### 7.5. `set_robust_list`

Stores the pointer in `Thread.robust_list_head`. Walking the list on exit
is deferred; without it a thread that dies while holding a mutex leaks it,
which is a latent bug but not one that trips pthread tests.

## 8. Kernel-side synchronisation

### 8.1. What must be protected

Every global in [sumi-kernel/src/lib.rs](sumi-kernel/src/lib.rs) is already
behind `spin::Mutex`. Under real contention we must revisit granularity:

| Object | Current granularity | Fine? |
|---|---|---|
| `PAGE_ALLOCATOR` | single `spin::Mutex` | Yes. 2 MB page allocation is infrequent. |
| `KERNEL_ALLOCATOR` | single `spin::Mutex` | On hot kmalloc/kfree paths this becomes a bottleneck. Plan: keep as-is, profile; if > 5% of time → per-CPU magazine cache. |
| `KERNEL_PAGE_TABLE` | single `spin::Mutex` | Yes. mmap/mprotect are not in the steady-state hot path. But: after PT changes a **TLB shootdown** is required. See §8.3. |
| `FD_TABLE` | single `spin::Mutex` | Yes. |
| `VMA_TABLE` | single `spin::Mutex` | Yes. |
| `BRK_BASE`/`BRK_CURRENT`/`MMAP_NEXT` | Fixed. Merged into one `MEMORY_STATE: spin::Mutex<MemoryState>` (`sumi-kernel/src/lib.rs`) holding `brk_base`/`brk_current`/`mmap_next` together, closing the two-mutex TOCTOU. | Yes. |
| `THREAD_REGISTRY` | new `spin::Mutex` | Yes. |
| Futex buckets | new | per-bucket `spin::Mutex` |
| Per-CPU runqueues | new | per-CPU `spin::Mutex` |

### 8.2. IRQ disable

With no IDT today and only hlt/hypercall traps, spinlocks do **not** need
`*_irqsave`. Once preemption is enabled (phase 9) all locks held across
context-switch points must disable preemption (easy via a per-CPU
`preempt_count`); once a timer IRQ is added, the scheduler-path locks must
also use `irqsave`.

### 8.3. TLB shootdown

After `mprotect` / `munmap` changes the shared page table, other vCPUs
still have stale TLB entries. Options:

1. **`HC_TLB_FLUSH` hypercall** → host `pthread_kill(SIGUSR1)` each target
   vCPU → KVM_RUN returns with `Intr` → sumi-vm calls
   `KVM_X86_FLUSH_TLB_GUEST` (where available) or injects a synthetic IPI.
2. **Lazy reload on next syscall entry**: re-write CR3 (full flush). Slow,
   always correct.
3. **Pure IPI via LAPIC**, requires IDT. Deferred until preemption lands.

**Decision for v1: option 2 (lazy reload) in `sys_mprotect` / `sys_munmap`,
plus an explicit global flush bit that every CPU checks on syscall
entry.** Move to option 1 once hypercall infrastructure is in place
(phase 2) and profile says it matters.

### 8.4. Atomics in kernel-coded paths

All `*uaddr` reads in futex / clone handlers must use
`core::sync::atomic::AtomicU32::from_ptr` with `Acquire` semantics. Today
[sumi-kernel/src/syscall/handlers/thread.rs](sumi-kernel/src/syscall/handlers/thread.rs)
uses raw pointer deref; that must be rewritten.

## 9. `futex`

### 9.1. Hash-table wait queues

File: new `sumi-kernel/src/sched/futex.rs`.

```rust
const FUTEX_BUCKETS: usize = 256;       // power of two

pub struct FutexBucket {
    lock: spin::Mutex<()>,
    head: AtomicPtr<Thread>,            // intrusive via Thread.wait_link
}

static BUCKETS: [FutexBucket; FUTEX_BUCKETS] =
    [const { FutexBucket::new() }; FUTEX_BUCKETS];

#[inline]
fn bucket_for(uaddr: VirtualAddr) -> &'static FutexBucket {
    let h = (uaddr.as_usize() >> 2).wrapping_mul(0x9E3779B97F4A7C15);
    &BUCKETS[(h as usize) & (FUTEX_BUCKETS - 1)]
}
```

Multiplicative (Knuth) hash, 256 buckets — fits comfortably in L1 without
false sharing per-bucket. Intrusive singly-linked list via `Thread.wait_link`,
so no allocation on the hot path.

### 9.2. `FUTEX_WAIT`

File: `sumi-kernel/src/sched/futex.rs::wait` (no timeout support yet —
`FUTEX_WAIT_BITSET` with `FUTEX_CLOCK_REALTIME` from glibc's timed paths
lands in `wait_bitset` with the bitset filter, not a real deadline).

```rust
pub fn wait(uaddr: *const u32, expected: u32) -> i64 {
    let me = current_thread();
    let bucket = bucket_for(uaddr as usize);
    let g = bucket.lock.lock();

    // Re-check *uaddr under the bucket lock: either the condition still
    // holds and we will safely park, or it changed and we bail with EAGAIN.
    let cur = unsafe {
        AtomicU32::from_ptr(uaddr as *mut u32).load(Ordering::Acquire)
    };
    if cur != expected {
        drop(g);
        return EAGAIN;
    }

    // Publish ourselves on the wait queue, THEN set Blocked — both still
    // under the bucket lock (see §6.6: unconditional store, not a CAS).
    me.wait_link.uaddr.store(uaddr as u64, Ordering::Relaxed);
    me.wait_link.bitset.store(!0, Ordering::Relaxed);
    bucket_push(bucket, me);
    me.futex_bucket.store(bucket as *const _ as *mut _, Ordering::Relaxed);
    me.state.store(ThreadState::Blocked as u32, Ordering::Release);
    drop(g);

    schedule();     // resumes here once some wake_blocked/schedule() picks us

    me.wait_link.uaddr.store(0, Ordering::Relaxed);
    me.futex_bucket.store(core::ptr::null_mut(), Ordering::Relaxed);
    0
}
```

There is no hypercall on this path: blocking is purely an in-kernel state
store + `schedule()`, which keeps futex waits O(1) plus the cost of one
context switch. Unlike the CAS-based sketch this replaces, unlinking from
the bucket on the wake side is the *only* place a waiter is removed from
the queue — `wait()` never re-takes the bucket lock to remove itself,
because by construction (§6.6) a wake that raced with us already did it.

### 9.3. `FUTEX_WAKE`

```rust
pub fn futex_wake(uaddr: *const u32, max: u32) -> i64 {
    let bucket = bucket_for(VirtualAddr::new(uaddr as usize));
    let _g = bucket.lock.lock();

    let mut woken = 0u32;
    let mut it = bucket_head(bucket);
    while let Some(t) = it.as_mut() && woken < max {
        let nxt = t.wait_link.next.load(Ordering::Relaxed);
        if t.wait_link.uaddr.load(Ordering::Relaxed) == uaddr as u64 {
            remove_bucket(bucket, t);
            sched::wake_blocked(t);
            woken += 1;
        }
        it = nxt;
    }
    woken as i64
}
```

`wake_blocked` handles the CAS `Blocked → Runnable`, enqueue on the target
CPU's runqueue, and IPI if that CPU is idle. Note `wake_blocked` is robust
against a waker racing with a waiter that bailed out on the re-check in
§9.2 — the CAS just fails and we move on.

### 9.4. Bitset / requeue

`FUTEX_WAIT_BITSET` / `FUTEX_WAKE_BITSET`: add a `bitset` filter inside the
walker in 9.3. `FUTEX_REQUEUE` / `FUTEX_CMP_REQUEUE`: atomically move nodes
from bucket(uaddr) to bucket(uaddr2), waking up to `n`. These are deferred
to phase 6; in the meantime glibc falls back gracefully.

### 9.5. Replacing existing `sys_futex`

[sumi-kernel/src/syscall/handlers/thread.rs](sumi-kernel/src/syscall/handlers/thread.rs)
is rewritten wholesale. Host-side mocking of the scheduler for unit tests
is done via a `SchedOps` trait (§14.1).

## 10. `exit` / `exit_group`

### 10.1. Single-thread `exit`

File: [sumi-kernel/src/syscall/handlers/process.rs](sumi-kernel/src/syscall/handlers/process.rs).

```rust
pub fn sys_exit(args: &SyscallArgs) -> ! {
    let code = args.arg0 as i32;
    let me = current_thread();
    me.exit_code.store(code, Ordering::Release);

    // CLONE_CHILD_CLEARTID handshake: zero *clear_child_tid and FUTEX_WAKE.
    // Without this, pthread_join in the parent hangs forever.
    let clear = me.clear_child_tid.swap(0, Ordering::AcqRel);
    if clear != 0 {
        // SAFETY: pointer from user space, glibc guarantees TCB validity.
        unsafe {
            AtomicU32::from_ptr(clear as *mut u32)
                .store(0, Ordering::Release);
        }
        let _ = futex::futex_wake(clear as *const u32, 1);
    }

    // Unregister from the thread registry so gettid lookups return None.
    THREAD_REGISTRY.lock().unregister(me.tid);

    // If we were the last thread in the thread group → exit_group semantics.
    if THREAD_REGISTRY.lock().alive_count() == 0 {
        return sys_exit_group(args);
    }

    // Push self onto the zombie list (by Arc<Thread>, not tid — the reaper
    // needs the live Thread, not just an id to look up). The reaper (run
    // from another CPU's `schedule()` hook) will drop this Arc and free the
    // kernel stack AFTER we have context-switched off — we can't free our
    // own stack while we are still on it.
    reaper::push_zombie(me_arc);
    me.state.store(ThreadState::Exited as u32, Ordering::Release);

    sched::schedule();          // will never come back
    unreachable!()
}
```

`sched::reaper::reap_zombies()` (called at the end of `schedule()` — see
§6.3) walks `ZOMBIE_LIST: spin::Mutex<Vec<Arc<Thread>>>`, and for any
zombie that is **not** the thread currently running on any CPU, drops the
last `Arc<Thread>` (unregistering it from `THREAD_REGISTRY`) and frees the
kernel stack page via `PAGE_ALLOCATOR`. This solves the "can't free your
own stack" problem: by the time the reaper runs, the zombie's stack is
unused because the CPU that ran it has already context-switched to
another thread.

### 10.2. `exit_group`

```rust
pub fn sys_exit_group(args: &SyscallArgs) -> ! {
    let code = args.arg0 as i32;
    kprintln!("[exit] code={}", code);
    hypercall::shutdown(code);     // HC_SHUTDOWN
    halt_forever();
}
```

On the host side `HC_SHUTDOWN` signals every vCPU host thread to break
out of `KVM_RUN` and sumi-vm joins them all before exiting with `code`.

## 11. Memory: stacks

### 11.1. User stack

glibc's `pthread_create` → `allocate_stack` mmaps its own stack with
`MAP_PRIVATE|MAP_ANONYMOUS|MAP_STACK` and mprotects a guard page. The
kernel does nothing special here — `mmap` already works. We must fix
`mprotect(PROT_NONE)` to actually clear PRESENT so guard pages trap
(currently a no-op in
[sumi-kernel/src/syscall/handlers/memory/mod.rs](sumi-kernel/src/syscall/handlers/memory/mod.rs)).

### 11.2. Kernel stack per thread

Allocated in `sys_clone` from `PAGE_ALLOCATOR` (one 2 MB page; 64 KB used
as stack, the rest is slack). Alternative: `kmalloc` 64 KB — more precise
but prone to fragmentation under bursty thread creation. The 2 MB / thread
cost caps at ~200 MB for 100 threads, which is acceptable.

### 11.3. Guard page for kernel stack

Optional: mark the first 4 KB of the kernel stack page as `PRESENT=0`.
Because the kernel page table operates on 2 MB pages this requires
splitting the huge page into 4 KB PTEs — costly. Deferred to a later phase,
tracked as a follow-up risk (§15.2).

## 12. Changes in sumi-vm

### 12.1. CLI

[sumi-vm/src/cmd/run.rs](sumi-vm/src/cmd/run.rs) adds:

```rust
#[arg(long = "vcpus", value_name = "N", default_value_t = default_vcpus())]
vcpus: usize,

fn default_vcpus() -> usize { num_cpus::get().clamp(1, 64) }
```

`VmCreateInfo.vcpu_count = self.vcpus`. This is the **only** CLI knob for
concurrency; there is no longer a `--max-threads`.

### 12.2. Parallel bring-up

[sumi-vm/src/vm.rs](sumi-vm/src/vm.rs): `run()` today already spawns one
`std::thread` per vCPU. Changes:

1. All N vCPUs enter `KVM_RUN` concurrently from the start. vCPU 0 starts
   at `kernel_entry` (BSP); vCPUs 1..N-1 start at `ap_start`.
2. Each vCPU host thread runs a plain `KVM_RUN` loop and handles
   `VcpuExit::Hypercall`, `VcpuExit::Hlt` (re-enter KVM_RUN — natural
   park), `VcpuExit::Intr` (from `pthread_kill(SIGUSR1)` for IPI/TLB
   shootdown; just re-enter).
3. `HC_KICK_CPU` handler: look up target vCPU's host pthread in a
   `vcpu_pool: Vec<VcpuSlot>`, call `pthread_kill(tid, SIGUSR1)`.
4. `HC_SHUTDOWN` handler: set a global shutdown flag and `pthread_kill`
   every other vCPU thread, then the main sumi-vm thread joins all.

```rust
struct VcpuSlot {
    id: u32,
    host_tid: libc::pthread_t,
    handle:   Option<thread::JoinHandle<Result<()>>>,
}
```

### 12.3. Shared memory: nothing extra

All vCPUs already share a single `GuestMemoryMmap` via `Arc`. CR3 is
common (`DIRECT_MAP_PML4`). No per-vCPU memslots.

### 12.4. Concurrent device access

`Arc<Mutex<DeviceRegistry>>` already serialises MMIO from different
vCPUs. Under heavy virtio-fs / console traffic this will become the
bottleneck; per-device locks are a follow-up, not a blocker for phase 10.

### 12.5. Hypercall mechanism

MMIO-only (no `KVM_CAP_EXIT_HYPERCALL`/`vmcall` path). The kernel writes an
8-byte value to `HYPERCALL_MMIO_BASE + offset`; sumi-vm's MMIO device
registry decodes `(offset, value)` per the table in §4.5 and dispatches to
the corresponding host action.

## 13. Implementation phases

Every phase must leave `make integration-test` green.

### Phase 0 — Per-CPU refactor (no behavioural change)

1. Introduce `PerCpu` (§3.4) in a static `[PerCpu; MAX_VCPUS]`.
2. BSP sets GS base in `_start` to `&PER_CPU[0]`.
3. Move `SYSCALL_STACK_TOP` and `SAVED_USER_RSP` from globals into
   `PerCpu`. `syscall_entry` asm reads them via `gs:offset`.
4. All existing tests pass unchanged.

Files:
- `sumi-kernel/src/sched/mod.rs` (new)
- `sumi-kernel/src/sched/percpu.rs` (new)
- [sumi-kernel/src/arch/x86_64/syscall.rs](sumi-kernel/src/arch/x86_64/syscall.rs) (asm edit)
- [sumi-kernel/src/kernel_main.rs](sumi-kernel/src/kernel_main.rs) (GS base init)

### Phase 1 — Fixed N vCPUs bootstrapped into idle

1. `--vcpus N` CLI flag.
2. Create N vCPUs, write `BootInfo.num_cpus` and AP entry table.
3. `ap_start.rs` (`global_asm!`) + `ap_main` initialising per-CPU state and
   spinning on `KERNEL_READY`.
4. BSP enables `KERNEL_READY` after its init is done.
5. Each AP runs an empty idle loop (`sti; hlt`) for now — no scheduler
   yet.
6. Smoke test: boot `hello_world` with `--vcpus 4`; all 4 CPUs come up,
   the program exits cleanly.

Files:
- `sumi-kernel/src/arch/x86_64/ap_start.rs` (new, `global_asm!`)
- `sumi-kernel/src/arch/x86_64/smp.rs` (new)
- [sumi-vm/src/cmd/run.rs](sumi-vm/src/cmd/run.rs), [sumi-vm/src/vm.rs](sumi-vm/src/vm.rs)

### Phase 2 — Hypercall mechanism + `HC_SHUTDOWN`

1. Define `HC_*` enum and `sumi-abi/src/hypercall.rs`.
2. Handle `VcpuExit::Hypercall` in sumi-vm (MMIO fallback too).
3. Kernel-side `hypercall::kick_cpu`, `hypercall::shutdown`.
4. `exit_group` uses `HC_SHUTDOWN` instead of `halt_forever`.

Files:
- `sumi-abi/src/hypercall.rs` (new)
- `sumi-kernel/src/arch/x86_64/hypercall.rs` (new)
- sumi-vm kvm backend

### Phase 3 — Scheduler + context switch (no `clone` yet)

1. `Thread`, `ThreadContext`, `ThreadRegistry`.
2. Per-CPU `RunQueue`, `schedule()`, `__switch_to_asm`.
3. Idle thread per CPU, idle loop.
4. `wake_blocked` + `need_resched` + `HC_KICK_CPU`-based IPI.
5. Test: `data/syscalls/sched_yield.rs` creates **two** kernel-side
   threads manually (through a test-only kthread API) and ping-pongs via
   `sched_yield`. This exercises context switch without going through
   `clone`.

Files:
- `sumi-kernel/src/sched/thread.rs`
- `sumi-kernel/src/sched/registry.rs`
- `sumi-kernel/src/sched/runqueue.rs`
- `sumi-kernel/src/arch/x86_64/switch.rs`
- `sumi-kernel/src/sched/mod.rs`

### Phase 4 — `clone()` syscall

1. `sys_clone`, `build_initial_frame`, `thread_entry_trampoline`.
2. Extend `SyscallArgs` with `caller_rip` / `caller_rflags` (asm edit).
3. Test `data/syscalls/clone_basic.rs`: raw clone without glibc; child
   writes a known value into a shared variable; parent waits on it.

Files:
- `sumi-kernel/src/syscall/handlers/clone.rs` (new)
- [sumi-kernel/src/syscall/mod.rs](sumi-kernel/src/syscall/mod.rs) (dispatch nr 56)
- [sumi-kernel/src/arch/x86_64/syscall.rs](sumi-kernel/src/arch/x86_64/syscall.rs)

### Phase 5 — Futex via scheduler

1. `FutexBucket`, `futex_wait`, `futex_wake`, `wake_blocked` integration.
2. Fix `*uaddr` reads to use `AtomicU32::from_ptr`.
3. Test `data/syscalls/futex_wait_wake.rs`: two kernel threads, futex
   ping-pong × 1000.

Files:
- `sumi-kernel/src/sched/futex.rs` (new)
- [sumi-kernel/src/syscall/handlers/thread.rs](sumi-kernel/src/syscall/handlers/thread.rs) (full rewrite)

### Phase 6 — TLS + `clone3`

1. Per-thread `fs_base`, written by context switch.
2. `arch_prctl(ARCH_SET_FS)` updates `current_thread().fs_base`.
3. `clone` with `CLONE_SETTLS` sets `fs_base` on child.
4. `clone3` (syscall 435).
5. Test `data/glibc/pthread_create_join.c`: the first real pthread.

### Phase 7 — `exit` / reaper / robust_list

1. Per-thread `sys_exit` with `CLONE_CHILD_CLEARTID` + `futex_wake`.
2. `sys_exit_group` via `HC_SHUTDOWN`.
3. `ZOMBIE_LIST` + `sched::reaper::reap_zombies` at the end of `schedule()`.
4. Test `data/glibc/pthread_join.c`: pthread_join returns cleanly.

### Phase 8 — TLB shootdown + real `mprotect`

1. Lazy CR3 reload in `sys_mprotect` / `sys_munmap`.
2. Make `mprotect(PROT_NONE)` actually clear `PRESENT`.
3. Merge `BRK_BASE`/`BRK_CURRENT`/`MMAP_NEXT` under one lock.
4. Tests `data/glibc/pthread_mutex.c`, `pthread_cond.c`.

### Phase 9 — Timer preemption

Implemented, matching §6.7's "mandatory from day one" decision — this phase
is not optional; §13's original wording contradicted §6.7 and has been
corrected. Cooperative-only scheduling was never shipped as a stopping
point.

1. Minimal IDT + LAPIC timer vector handler.
2. Timer ISR increments `preempt_count`, calls the handler, decrements, and
   checks `need_resched` on the way out.
3. Locks crossed by a scheduler tick use `irqsave`.
4. Test: a CPU-bound glibc thread yields within a bounded time
   (`glibc/preempt_timer.c`).

### Phase 10 — std::thread stress

1. Test `data/rust_std/thread_spawn.rs`: `std::thread::spawn` + `join` × 8.
2. Test `data/rust_std/mutex_arc.rs`: `Arc<Mutex<u64>>` ++ from 8 threads
   to 400_000.
3. Test `data/rust_std/mpsc_channel.rs`: channel ping-pong.
4. Stress: `data/glibc/pthread_storm.c` — 100 threads × 1000 mutex/cond
   iterations × join. Exercises vCPU saturation, work stealing, reaper,
   kernel-stack lifecycle.

## 14. Testing

### 14.1. Host unit tests

Every new module should have a `#[cfg(test)] mod tests`. As of this
revision `syscall/handlers/{clone,process,thread,random}.rs` and most of
`sched/*` still gate their real bodies behind `#[cfg(not(test))]`
(no-op/zeroed under test), which violates the project's rule against
`cfg(test)`-forked production code and leaves the riskiest logic —
`clone`, futex, the scheduler state machine, the reaper guard — with zero
host coverage. **Target design, landing under fix F14:** introduce a
small scheduler seam so handler bodies compile and run identically under
test, then delete every `#[cfg(not(test))]` in syscall handlers and
`sched/*`. The seam is a trait implemented once against the real
scheduler and once against a host-side mock:

```rust
pub trait SchedOps {
    fn block_current(&self); // store Blocked under the caller's lock (§6.6), then schedule()
    fn schedule(&self);
    fn wake_blocked(&self, t: &Thread);
    fn kick_cpu(&self, cpu: u32);
}
```

The real impl backs production; a `MockSched` (e.g. channel- or
queue-backed, single-threaded) is injected in tests. This unlocks host
coverage for `sched/futex.rs`, `schedule()`'s state machine,
`wake_blocked`, `try_steal_work`, and the reaper guard (T2).

- `sched/registry.rs`: TID alloc, register/unregister, lookup,
  `alive_count`.
- `sched/runqueue.rs`: FIFO ordering, concurrent push from two pseudo-CPUs
  via `loom` where possible, single-steal correctness (§6.8).
- `sched/futex.rs`: hash distribution, push/pop under lock, atomic
  acquire/release ordering, lost-wakeup regression. Use the existing
  `TestDirectMap` pattern from
  [sumi-kernel/src/memory/alloc/kmalloc.rs](sumi-kernel/src/memory/alloc/kmalloc.rs).
- `sched/thread.rs`: state machine CAS transitions.
- `syscall/handlers/clone.rs`: `EINVAL` on missing flags, successful
  `Thread` construction, initial frame layout.

### 14.2. Integration tests

New files under `sumi-integration-tests/data/syscalls/`:

| File | What it checks |
|---|---|
| `clone_basic.rs` | Plain `clone(CLONE_VM\|...)`; child writes a known value, parent waits. |
| `clone_einval.rs` | `clone(0, ...)` and `clone(CLONE_VM, ...)` → `EINVAL`. |
| `clone_settls.rs` | `clone(... CLONE_SETTLS, tls=ptr)` + child reads `fs:0` == `*ptr`. |
| `sched_yield.rs` | Two manually-created kernel threads ping-pong via sched_yield (phase 3). |
| `futex_wait_wake.rs` | Two threads, futex ping-pong N=1000. |
| `futex_wait_eagain.rs` | WAIT with mismatched val → `EAGAIN`. |
| `gettid_per_thread.rs` | Main = 1, each child gets a unique TID ≠ 1. |
| `exit_one_thread.rs` | Main spawns a child, child calls `exit(0)`, main keeps running and exits cleanly. |
| `exit_group_kills_all.rs` | Main spawns 4 children, one calls `exit_group(7)`, VM exits with code 7. |
| `clear_child_tid_wakes.rs` | Main spawns child with `CLONE_CHILD_CLEARTID`, waits on `*ctid==0` via `FUTEX_WAIT`, child exits, main wakes. |

New files under `sumi-integration-tests/data/glibc/`:

| File | What it checks |
|---|---|
| `pthread_create_join.c` | `pthread_create(&t, NULL, fn, NULL); pthread_join(t, NULL);` |
| `pthread_mutex.c` | 4 threads ++ a shared counter 100k times each → 400k. |
| `pthread_cond.c` | Producer/consumer on condvar. |
| `pthread_self.c` | `pthread_self()` is unique per thread. |
| `tls_keys.c` | `pthread_key_create` + `pthread_setspecific`/`getspecific`. |

New files under `sumi-integration-tests/data/rust_std/`:

| File | What it checks |
|---|---|
| `thread_spawn.rs` | `std::thread::spawn` + `join` × 8. |
| `mutex_arc.rs` | `Arc<Mutex<u64>>` ++ from 8 threads. |
| `mpsc_channel.rs` | `mpsc::channel`, N producers + 1 consumer. |

### 14.3. Stress test

`data/glibc/pthread_storm.c` — spawn 100 threads, each runs 1000 mutex +
condvar iterations, join all. Exercises:
- M:N scheduling saturation (N < M),
- work-stealing across idle vCPUs,
- futex wait/wake under load,
- reaper: no kernel-stack leak (add an assert on `PAGE_ALLOCATOR.free_count`
  before/after).

## 15. Open questions and risks

### 15.1. Open questions

1. **`KVM_CAP_EXIT_HYPERCALL` availability.** Needs Linux ≥ 5.16 on the
   host. On older kernels fall back to MMIO. Decide in phase 2 after
   testing on a real `/dev/kvm`.
2. **TID wraparound.** `next_tid: u32` — after 4G `clone()`s we wrap.
   Linux does the same. Deferred until it actually matters.
3. **`pthread_kill` across threads.** Out of scope, but glibc may call
   `tgkill(tgid, tid, 0)` to check liveness in certain paths. A "liveness
   probe" stub returning 0 / `ESRCH` can be added cheaply when a real
   test requires it.
4. **Robust list walk at exit.** Needed for robustness against a thread
   dying while holding a mutex. Deferred.
5. **`mmap(MAP_FIXED)` atomicity under concurrency.** VMA_TABLE is already
   locked, but we must verify the entire critical section (VMA insert +
   PT update) sits under one lock acquire.
6. **LAPIC timer availability in guest.** If we go to phase 9 preemption
   we need to be sure KVM exposes the LAPIC timer on the configuration
   we run — it should, but confirm.

### 15.2. Risks

1. **Race in context switch.** `__switch_to_asm` must be called with
   interrupts disabled (`cli` — easy, because there is no IDT) and with
   the per-CPU runqueue lock released. If `schedule()` is called while
   holding a lock that the next task also needs, we deadlock. Mitigation:
   strict rule — `schedule()` may only be called from: (a) syscall return,
   (b) `sched_yield`, (c) after storing `Blocked` and adding to a wait
   queue with its lock already dropped (§6.6).
2. **Lost wakeup.** Classic: waiter checks condition, sees false, adds
   itself to queue, blocks. Waker sets condition, checks queue, sees
   empty. Mitigation: the wait-queue lock orders "publish in queue" and
   "check condition" for the waiter, and "modify condition" and "inspect
   queue" for the waker. `futex_wait` (§9.2) takes the bucket lock,
   re-reads `*uaddr`, and only blocks under that lock. `wake_blocked`'s
   `Blocked → Runnable` CAS (§6.6) provides the final safety net by
   detecting a racing waker without needing a CAS on the blocking side.
3. **Use-after-free on exit.** A thread cannot free its own kernel stack
   before it has context-switched off. Mitigation: zombie list + reaper
   hook in `schedule()` (§10.1).
4. **FPU state saves.** Eager XSAVE on every context switch is simple and
   right, but costs ~150 cycles. Lazy FPU is a follow-up optimisation and
   requires an #NM handler.
5. **Kernel-stack overflow.** 64 KB per thread; no guard page yet.
   Mitigation: a canary word at the stack base checked at every schedule
   point, plus a follow-up phase to split the 2 MB page into 4 KB PTEs
   and drop a PROT_NONE guard.
6. **IPI storms.** A pathological `futex_wake_all` on a large cond_var
   could fire N `HC_KICK_CPU`s per wake. Mitigation: coalesce — only
   one IPI per target CPU per wake batch, and only if that CPU is
   actually `is_idle == true`.
7. **Priority inversion in futex.** Without priorities or PI-mutex
   support, a low-pri thread holding a glibc mutex can starve a high-pri
   waiter. Not a correctness problem, but a latency one. Out of scope.
8. **Lock ordering deadlock.** Any path that takes both a per-CPU runqueue
   lock and a futex-bucket lock must take them in a fixed order. Current
   design: runqueue locks are always taken **after** releasing the futex
   bucket lock, never held across. Document and enforce this.
9. **Stale TLB on free.** `kfree` can reuse memory whose TLB mapping on
   another CPU is stale. Not an issue today because mappings are never
   unmapped; if we ever unmap direct-map pages we'll need a shootdown on
   `kfree` too.
10. **Starting APs with a stale `KERNEL_READY`.** APs spin on
    `KERNEL_READY` before they ever enter the idle loop. Failure mode:
    BSP sets `KERNEL_READY = true` before `PAGE_ALLOCATOR` is ready →
    APs allocate their idle-thread stack from an empty allocator.
    Mitigation: BSP sets `KERNEL_READY` strictly as the last step of its
    init path.
11. **`pthread_kill` signal collision.** We use `SIGUSR1` for
    `HC_KICK_CPU`. If the guest program itself installed a `SIGUSR1`
    handler… it cannot, because signals are not delivered into guest code
    anyway (out of scope). Still worth documenting so a future signal
    subsystem chooses a different sumi-vm-internal signal.
12. **CPU pinning in sumi-vm.** Linux may migrate host pthreads across
    host CPUs, hurting L1/L2 locality and TLB. Optional CLI flag
    `--pin-vcpus` via `sched_setaffinity` on each host pthread.

## 16. Related files (current code)

Critical to touch:
- [sumi-kernel/src/kernel_main.rs](sumi-kernel/src/kernel_main.rs)
- [sumi-kernel/src/lib.rs](sumi-kernel/src/lib.rs)
- [sumi-kernel/src/arch/x86_64/syscall.rs](sumi-kernel/src/arch/x86_64/syscall.rs)
- [sumi-kernel/src/syscall/mod.rs](sumi-kernel/src/syscall/mod.rs)
- [sumi-kernel/src/syscall/handlers/process.rs](sumi-kernel/src/syscall/handlers/process.rs)
- [sumi-kernel/src/syscall/handlers/thread.rs](sumi-kernel/src/syscall/handlers/thread.rs)
- [sumi-kernel/src/syscall/handlers/memory/mod.rs](sumi-kernel/src/syscall/handlers/memory/mod.rs)
- [sumi-kernel/src/exec.rs](sumi-kernel/src/exec.rs)
- [sumi-kernel/src/memory/alloc/palloc.rs](sumi-kernel/src/memory/alloc/palloc.rs)
- [sumi-kernel/src/memory/alloc/kmalloc.rs](sumi-kernel/src/memory/alloc/kmalloc.rs)
- [sumi-kernel/src/arch/x86_64/pagetable.rs](sumi-kernel/src/arch/x86_64/pagetable.rs)
- [sumi-vm/src/vm.rs](sumi-vm/src/vm.rs)
- [sumi-vm/src/cmd/run.rs](sumi-vm/src/cmd/run.rs)
- [sumi-abi/src/arch/x86_64/layout.rs](sumi-abi/src/arch/x86_64/layout.rs)

New modules:
- `sumi-kernel/src/sched/mod.rs`
- `sumi-kernel/src/sched/percpu.rs`
- `sumi-kernel/src/sched/thread.rs`
- `sumi-kernel/src/sched/registry.rs`
- `sumi-kernel/src/sched/runqueue.rs`
- `sumi-kernel/src/sched/futex.rs`
- `sumi-kernel/src/syscall/handlers/clone.rs`
- `sumi-kernel/src/arch/x86_64/switch.rs`
- `sumi-kernel/src/arch/x86_64/smp.rs`
- `sumi-kernel/src/arch/x86_64/ap_start.rs`
- `sumi-kernel/src/arch/x86_64/hypercall.rs`
- `sumi-abi/src/hypercall.rs`
