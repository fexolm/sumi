# Multithreading in sumi

Status: current implementation notes, synced with the codebase on 2026-07-04.

`sumi` is a single-address-space unikernel. A thread is not a Linux process or
task with separate `mm`, credentials, namespaces, or file tables. It is an
independent execution flow over shared kernel/user data, scheduled onto a fixed
set of KVM vCPUs.

## What Is Implemented

- `sumi-vm run --vcpus N`, clamped to `1..=64`; default is host CPU count.
- BSP/AP boot with a `KERNEL_READY` release/acquire barrier.
- Per-CPU state via GS: syscall stack, saved user rsp, current thread, idle
  thread, runqueue, TLB generation, preemption flags.
- M:N scheduling: many `Thread`s over fixed vCPUs.
- Per-CPU runqueues with work stealing from idle CPUs.
- Cooperative reschedule points plus LAPIC timer preemption.
- `clone` and `clone3` for pthread-style threads.
- `futex` wait/wake and wait_bitset/wake_bitset.
- Per-thread `tid`, `tgid`, `fs_base`, `clear_child_tid`, and stored
  `robust_list_head`.
- Per-thread `exit`, process-wide `exit_group`, zombie reaping, and
  `CLONE_CHILD_CLEARTID` wakeup for `pthread_join`.

## Thread Model

Main types live under `sumi-kernel/src/sched/`:

- `thread.rs`: `Thread`, `Tid`, `ThreadContext`, `ThreadState`.
- `registry.rs`: TID allocation and `Arc<Thread>` registry.
- `percpu.rs`: fixed `PER_CPU[MAX_VCPUS]`.
- `runqueue.rs`: per-CPU FIFO runqueue.
- `clone.rs`: initial kernel frame for a new user thread.
- `futex.rs`: hashed futex wait queues.
- `reaper.rs`: delayed freeing of exited thread stacks.

Important thread states:

- `Runnable`: can be queued or selected by the scheduler.
- `Running`: currently executing on one vCPU.
- `Blocked`: parked on a futex wait queue.
- `Exited`: will be reclaimed by the reaper after it has switched off its
  kernel stack.

The scheduler keeps `Thread::on_cpu` as the handoff guard. A waking CPU must
wait until a blocked thread has completed its switch-away before another CPU can
run the same saved context.

## vCPU And Scheduler Flow

`sumi-vm` starts all vCPUs as host pthreads. vCPU 0 starts at the kernel entry;
APs start at `ap_start_asm(cpu_id)`, initialize per-CPU state, wait for
`KERNEL_READY`, then enter the idle loop.

Scheduling rules:

- `schedule()` must run with interrupts disabled.
- Callers must put the current thread into its next state before scheduling:
  requeued as runnable, parked as blocked, or marked exited.
- Context switching is done by `arch/x86_64/switch.rs`.
- The switch restores per-thread FS base so TLS follows the thread across CPUs.
- Idle CPUs use `hlt` and are kicked with the hypercall MMIO path.
- Page-table changes bump `TLB_GENERATION`; each CPU lazily reloads CR3 before
  returning to user code if its local generation is stale.

## Syscall Surface

Implemented threading-related syscalls:

| Syscall | Current behavior |
|---|---|
| `clone` | Requires the pthread-style shared-resource flag set. Returns child TID to parent; child returns 0 through the trampoline. |
| `clone3` | Supports the v0 `clone_args` layout. `exit_signal` must be 0. |
| `futex` | Supports `WAIT`, `WAKE`, `WAIT_BITSET`, `WAKE_BITSET`; no timed waits. |
| `sched_yield` | Pushes current thread to the local runqueue when another thread is runnable. |
| `gettid` / `getpid` | Per-thread TID, shared thread-group PID. |
| `set_tid_address` | Stores `clear_child_tid` for exit wakeup. |
| `set_robust_list` | Stores the pointer; robust-list walking is not implemented. |
| `arch_prctl` | Per-thread `ARCH_SET_FS` / `ARCH_GET_FS`. |
| `exit` | Exits the current user thread; last user thread shuts down the VM. |
| `exit_group` | Shuts down the VM with the supplied code. |
| `tkill` / `tgkill` | Minimal liveness/signal compatibility stubs. |

Unsupported or deliberately narrow:

- `fork`, `vfork`, process isolation, namespaces, ptrace, cgroups.
- Real signal delivery.
- Real-time scheduling and affinity syscalls.
- CPU hotplug.
- Futex PI, requeue, and timeout semantics.
- Robust-list recovery when a thread dies while holding a mutex.

## Host Hypercalls

Hypercalls use an MMIO trap range, not KVM's `KVM_EXIT_HYPERCALL` feature.
Current operations:

- `HC_KICK_CPU`: signal a peer vCPU host pthread with `SIGUSR1` so `KVM_RUN`
  returns and the guest can leave `hlt`.
- `HC_SHUTDOWN`: publish an exit code, stop the issuing vCPU, and signal peers.

`--gdb` forces `--vcpus 1`; the GDB stub does not yet model multiple vCPUs as
debugger threads.

## Tests

Relevant integration coverage lives in:

- `sumi-integration-tests/data/syscalls/clone_*.rs`
- `sumi-integration-tests/data/syscalls/futex_*.rs`
- `sumi-integration-tests/data/syscalls/sched_yield.rs`
- `sumi-integration-tests/data/syscalls/gettid_per_thread.rs`
- `sumi-integration-tests/data/syscalls/exit_*`
- `sumi-integration-tests/data/glibc/pthread_*.c`
- `sumi-integration-tests/data/rust_std/thread_spawn.rs`
- `sumi-integration-tests/data/rust_std/mutex_arc.rs`
- `sumi-integration-tests/data/rust_std/mpsc_channel.rs`

Run with:

```bash
make test
make integration-test
```

## Current Risks

- All user and kernel code shares one address space and ring 0 privilege.
- A bad user pointer can still fault the kernel; many syscalls rely on this
  unikernel trust model.
- Kernel stacks are fixed-size; guard behavior is coarse because mappings are
  2 MB pages.
- Futex semantics are intentionally just enough for pthread mutex/cond/join.
- Device MMIO is serialized through one `DeviceRegistry` mutex.
