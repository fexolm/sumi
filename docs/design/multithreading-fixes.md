# Multithreading & Project Audit — Problems and Fixes

> Status: fix plan (2026-07-03). Source: full-project audit + adversarial review of the
> uncommitted multithreading work on `fexolm/refactor`.
> Baseline: build ✓, unit tests ✓, integration 74/75 — `glibc/preempt_timer` times out.

## 1. Bugs (kernel / VM)

### P0 — blockers

**F1. LAPIC MMIO page shadowed by RAM memslot** — `sumi-vm/src/arch/x86_64/kvm/mod.rs:131`
(+ `run.rs:53`, `vm.rs:313`). With 2 GiB RAM + 2 GiB kernel-code region, memslot 0 spans
guest-phys [0, 4 GiB), covering the in-kernel LAPIC page at `0xFEE0_0000`. All LAPIC
register writes (SVR, LVT timer, initial count) land in RAM; the timer never fires; a
CPU-bound thread is never preempted → `preempt_timer` 30 s timeout. Empirically confirmed:
shrinking RAM below the APIC makes the timer fire immediately.
*Fix:* never map a memslot over `0xFEE0_0000`: split RAM around the LAPIC page (or cap the
low slot below it and put remaining RAM above 4 GiB), and reserve the containing 2 MiB
frame in `PageAllocator` so it is never handed out.

**F2. #DF on the preemptive return path** — `sumi-kernel/src/arch/x86_64/interrupt.rs`
(`isr_timer` return path). Once F1 is fixed, preempting user code double-faults: the
switch path re-enables IF (`popfq` of the new thread's RFLAGS=0x202) while logically still
on the timer's IST1 stack, so a second tick re-enters on the same IST stack.
*Fix:* IF must stay 0 across the entire `schedule()`/`__switch_to_asm` path; the timer ISR
must never be re-enterable on its own IST stack (leave IST for faults only, or defer the
switch to a non-IST exit path).

**F3. Blocked-thread can run on two CPUs at once** — `sched/futex.rs:119`,
`sched/mod.rs:127`, `try_steal_work` (`sched/mod.rs:506`). A waiter sets `Blocked` and drops
the bucket lock *before* `schedule()` saves its context; a waker (or work stealer) can
enqueue it and another CPU can `__switch_to_asm` into its stale `ctx.rsp` while the first
CPU is still mid-switch → one kernel stack executing on two CPUs.
*Fix:* add an `on_cpu` flag (set before the switch, cleared just after the context save
completes); wake/steal paths spin until `on_cpu == false` before running the thread
(Linux `p->on_cpu` protocol).

### P1 — serious

**F4. Reaper use-after-free window** — `sched/mod.rs:127` vs `sched/reaper.rs:45`.
`schedule()` publishes `current_thread = next` *before* the switch, so the reaper's
`is_running_on_any_cpu` sees the exiting thread as off-CPU while it is still executing on
its own stack, and frees that stack mid-switch. *Fix:* same `on_cpu` flag as F3 — reap only
when `on_cpu == false`.

**F5. Missing preempt/irqsave lock discipline** — `runqueue.rs:18`, `reaper.rs:10`, all
`spin::Mutex` sites. Design §6.7.3/§6.7.6 (preempt_disable on lock, `lock_irqsave` for
runqueue/zombie locks) is unimplemented. A timer tick while holding a runqueue/zombie lock
in an IF=1 window re-enters `schedule_preempt` → self-deadlock.
*Fix:* cli/sti-bracketed irqsave locking for the runqueue and zombie-list locks; make the
`preempt_count` discipline real (see F13) or document precisely why IF=0 covers each path.

**F6. `idle_loop` switches with IF=1** — `sched/mod.rs:540`. After the first `sti; hlt` the
loop calls `schedule()`/`__switch_to_asm` with interrupts enabled, violating the switch
precondition (contributes to F2). *Fix:* `cli` before `schedule()` in the idle loop;
assert/guarantee IF=0 at every `__switch_to_asm` entry.

**F7. No FPU/SSE state save on context switch** — `arch/x86_64/switch.rs:17`
(`xsave_area` always 0). glibc/Rust freely use SSE/AVX; a preemptive switch mid-computation
silently corrupts vector state. Design §6.4 promised eager XSAVE from day one.
*Fix:* allocate a per-thread XSAVE area (size from `cpuid.0xD.ecx`) and eager
`xsave`/`xrstor` around the switch.

### P2 — minor bugs

**F8. Dead "last thread → shutdown" path** — `syscall/handlers/process.rs:62`.
`alive_count()` includes idle threads and unreaped zombies, so `== 1` never fires; a raw
clone+exit last-thread program hangs. *Fix:* dedicated atomic live-user-thread counter.

**F9. Fragile hand-rolled interrupt return** — `interrupt.rs:250`. Comment premise is false
(64-bit `iretq` at CPL0 *does* pop SS:RSP); the manual return skips SS restore and abuses
`gs:[saved_user_rsp]` as scratch. *Fix:* push a proper 5-qword frame and use `iretq`.

**F10. Stale TLB after preemptive switch** — `syscall/mod.rs:152` checks `TLB_GENERATION`
only in the syscall postamble; a thread preempted via `isr_timer` returns with stale TLB
after another CPU's `mprotect`/`munmap` until its next syscall. *Fix:* perform the same
generation check/CR3 reload on the timer-preemption return path.

## 2. Architecture / overengineering

**F11. Debug spam in hot paths** — `thread.rs:58–88` (every futex op), `interrupt.rs:45–50`
(timer ticks + forever counter), `interrupt.rs:71` (every preemption). Synchronous debugcon
writes under load. *Fix:* delete (default) or gate behind a `trace` cfg.

**F12. Test scaffolding in production kernel** — `syscall/handlers/sumi_debug.rs`
(syscall 500) + `kthread_spawn` (`sched/mod.rs:195`) compiled unconditionally; its "parked"
threads busy-yield forever. *Fix:* delete now that `clone` works (sched_yield/futex tests
can use raw clone).

**F13. Vacuous preempt_count** — only the timer trampoline touches it, so
`debug_assert_eq!(preempt_count(), 0)` in `schedule()` can't fail. Correctness silently
rests on IF=0 at kernel entry. *Fix:* fold into F5 — either real discipline or an explicit
documented IF=0 invariant plus assertions that check IF instead.

**F14. `cfg(not(test))`-gated production syscall bodies** — `process.rs`, `thread.rs`,
`clone.rs`, `random.rs` no-op under test. Violates the project rule (no cfg(test) forks of
production code) and makes the riskiest logic untestable (the doc's §14.1 `SchedOps` seam
was never built). *Fix:* introduce the small scheduler seam so handler bodies compile and
run identically under test, and delete every `cfg(not(test))` in syscall handlers.

**F15. Dead / speculative code** — `PerCpu.syscall_stack_top` (`percpu.rs:39`, no reader),
`hypercall::tlb_flush` + `HC_TLB_FLUSH` (no callers), `Thread.futex_bucket` (write-only),
`Thread.robust_list_head` (stored, never walked — keep, it's ABI-visible via
`set_robust_list`, but delete the other three). *Fix:* delete dead items; re-add TLB-flush
hypercall only when Phase 8 needs it.

**F16. Reaper runs on every context switch** — `sched/mod.rs:143` takes the global
zombie lock (even to early-return) on every switch. *Fix:* gate on a cheap
`ZOMBIES_PENDING` atomic flag set by `sys_exit`.

**F17. `sched/mod.rs` is 614 lines** (cap 500) mixing scheduler core, thread builders,
kthread spawn, idle loop, trampoline. *Fix:* F12 deletion + move Thread builders to
`thread.rs`/`clone.rs`; keep `mod.rs` = schedule/wake/steal only.

## 3. Design doc (`multithreading-v2.md`) corrections

- **D1** Header says "implementation not started" — implemented through Phase 9. Update status.
- **D2** §13 calls Phase 9 preemption "optional", contradicting §6.7 "mandatory day one".
  Code sides with §6.7 — fix §13.
- **D3** §4.3/§13/§16: `ap_start.S` → reality is `ap_start.rs` (`global_asm!`).
- **D4** §4.5/§12.5: doc prefers `KVM_CAP_EXIT_HYPERCALL` vmcall w/ MMIO fallback — reality
  is MMIO-only. Document MMIO as the mechanism (offsets 0x00/0x08/0x10, not nums 0x01–0x03).
- **D5** §6.3: `__switch_to_asm` is 3-arg (fs_base loaded inside asm), not 2-arg.
- **D6** §6.4: eager XSAVE — not implemented; doc must match whatever F7 lands.
- **D7** §6.6/§9.2: `try_block` CAS → reality: unconditional `Blocked` store under bucket
  lock (which is *correct* for lost-wakeup); document the real protocol incl. F3's `on_cpu`.
- **D8** §6.2/§6.8: `push_front`/`steal_half`/busiest-queue stealing → reality: steal one
  from first non-empty peer. Document reality (adequate for current workloads).
- **D9** §8.1: BRK/MMAP TOCTOU listed as open bug — already fixed (`MEMORY_STATE`, `lib.rs:44`).
- **D10** §10.1/§3.3/§3.4 naming drift: `reap_zombies` (not `reaper_hook`), zombie list holds
  `Arc<Thread>` (not tid), `BTreeMap<u32,_>`, `AtomicPtr` idle thread.
- **D11** §14.1 `SchedOps` mock seam — implement per F14, then doc matches.

## 4. Test coverage

- **T1** Missing acceptance tests promised in §1.2/§14: `clone_einval`, `clone_settls`,
  `futex_wait_eagain`, `gettid_per_thread`, `exit_one_thread`, `exit_group_kills_all`,
  `clear_child_tid_wakes` (syscalls); `pthread_create_join.c`, `pthread_self.c`,
  `tls_keys.c` (glibc); rust_std `thread_spawn`/`mutex_arc`/`mpsc_channel`; `pthread_storm.c`.
- **T2** Concurrency-critical logic has zero host coverage (schedule state machine,
  wake_blocked, steal, reaper guard) because of F14's cfg-gating — unlocked by F14.
- **T3** No steal/concurrent-push tests for runqueue; clone frame construction untested.

## 5. Execution order

1. Wave 1 (bugs): F1–F10 + cleanups F11–F13, F15–F17. Gate: build + unit + integration all green, incl. `preempt_timer`.
2. Wave 2 (structure+tests): F14 seam, then T1–T3 tests. Gate: full suite green.
3. Doc sync D1–D11 (parallel with wave 1).
