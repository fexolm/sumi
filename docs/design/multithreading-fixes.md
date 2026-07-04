# Multithreading & Project Audit — Problems and Resolutions

> Status: **resolved** (2026-07-04). This began as a fix plan from a full-project
> audit + adversarial review of the multithreading work on `fexolm/refactor`;
> every item below has now landed and is verified. F14 is the only item still in
> progress at the time of writing (tracked at the bottom).
> Baseline before fixes: build ✓, unit tests ✓, integration 74/75 —
> `glibc/preempt_timer` timed out.
> After fixes: build ✓, 119 kernel unit tests ✓, integration **88/88** ✓
> (`preempt_timer` now fires in ~3 s instead of a 30 s timeout).

How to read this: each entry states the original defect and, under *Resolution:*,
what actually shipped. Where the implementation deviated from the original plan
(the audit was written before the code was proven on hardware), the deviation is
called out explicitly.

## 1. Bugs (kernel / VM) — all resolved

### P0 — blockers

**F1. LAPIC MMIO page shadowed by RAM memslot** — `sumi-vm/src/arch/x86_64/kvm/mod.rs`.
With 2 GiB RAM + 2 GiB kernel-code region, memslot 0 spanned guest-phys [0, 4 GiB),
covering the in-kernel LAPIC page at `0xFEE0_0000`. All LAPIC register writes (SVR,
LVT timer, initial count) landed in RAM; the timer never fired; a CPU-bound thread
was never preempted → `preempt_timer` 30 s timeout. Empirically confirmed: shrinking
RAM below the APIC made the timer fire immediately. **This was the root cause of the
baseline failure.**
*Resolution:* the RAM memslot is split around a 2 MiB hole at the LAPIC page so no
memslot ever covers `0xFEE0_0000`; the containing frame is reserved in
`PageAllocator` (`palloc.rs`) so it is never handed out. The LAPIC base is a shared
constant `LAPIC_BASE_PHYS` in `sumi-abi` layout.

**F2. #DF on the preemptive return path** — `arch/x86_64/{interrupt,switch,syscall}.rs`.
Once F1 was fixed, preempting user code double-faulted: switch paths re-enabled IF
(`popfq` of the new thread's RFLAGS=0x202) while logically still on the timer's IST1
stack, so a second tick re-entered on the same IST stack.
*Resolution:* IF is masked out of the pushed RFLAGS before every `popfq`
(`__switch_to_asm`, the syscall-return tail, and `thread_entry_trampoline`), with an
explicit `sti` issued immediately before the final `jmp`/`ret`. A related bug was
found and fixed in the same pass: **POPF has no STI-style interrupt shadow**, so a
pending tick could be recognized between `popfq` and the subsequent `pop rsp` — fixed
at all three sites by the mask-IF-then-late-`sti` pattern.

**F3. Blocked thread could run on two CPUs at once** — `sched/{futex,mod}.rs`,
`try_steal_work`. A waiter set `Blocked` and dropped the bucket lock *before*
`schedule()` saved its context; a waker or work-stealer could enqueue it and another
CPU could `__switch_to_asm` into its stale `ctx.rsp` while the first CPU was still
mid-switch → one kernel stack executing on two CPUs.
*Resolution:* `Thread.on_cpu: AtomicBool`, set before the switch and cleared in
`__switch_to_asm` only after the context save completes (Linux `p->on_cpu` protocol).
Wake and steal paths spin until `on_cpu == false` before running the thread.

### P1 — serious

**F4. Reaper use-after-free window** — `sched/mod.rs` vs `sched/reaper.rs`.
`schedule()` published `current_thread = next` before the switch, so the reaper saw
the exiting thread as off-CPU while it was still executing on its own stack, and could
free that stack mid-switch.
*Resolution:* closed by the same `on_cpu` flag as F3 — a zombie is reaped only once
`on_cpu == false`.

**F5. Missing preempt/irqsave lock discipline** — `runqueue.rs`, `reaper.rs`, all
`spin::Mutex` sites. A timer tick while holding a runqueue/zombie lock in an IF=1
window could re-enter `schedule_preempt` → self-deadlock.
*Resolution:* new `sched/irq.rs` with an `IrqGuard` (cli-on-acquire / restore-on-drop)
bracketing the runqueue and zombie-list critical sections. The guard is the sanctioned
host-stub seam (no-op under `cfg(test)`, since cli/sti can't run on the host).

**F6. `idle_loop` switched with IF=1** — `sched/mod.rs`. After the first `sti; hlt`
the loop called `schedule()`/`__switch_to_asm` with interrupts enabled, violating the
switch precondition.
*Resolution:* `cli` before `schedule()` in the idle loop; `schedule()` now asserts
`!interrupts_enabled()` on entry (replacing the vacuous `preempt_count` check, see F13).

**F7. No FPU/SSE state save on context switch** — `arch/x86_64/switch.rs`.
glibc/Rust freely use SSE; a preemptive switch mid-computation silently corrupted
vector state.
*Resolution:* eager FPU save/restore around the switch. **Deviation from the plan:**
the audit called for XSAVE, but sumi-vm deliberately leaves `CR4.OSXSAVE` clear (to
hide AVX from glibc's IFUNC resolvers), which makes `XSAVE` #UD. Shipped with the
512-byte `FXSAVE`/`FXRSTOR` (`FxsaveArea`) instead, which is correct for the SSE state
sumi actually exposes.

### P2 — minor bugs

**F8. Dead "last thread → shutdown" path** — `syscall/handlers/process.rs`.
`alive_count()` included idle threads and unreaped zombies, so `== 1` never fired; a
raw clone+exit last-thread program hung.
*Resolution:* a dedicated atomic live-user-thread counter (`registry::LIVE_USER_THREADS`)
drives the shutdown decision.

**F9. Hand-rolled interrupt return** — `interrupt.rs`.
*Deviation — the audit's premise was wrong.* The audit claimed 64-bit `iretq` at CPL0
pops SS:RSP and proposed switching to `iretq`. In fact `iretq` at the **same CPL** does
*not* pop SS:RSP (that pop is conditional on `CS.RPL != CPL`), and sumi runs everything
at CPL0 — so the original manual RSP-restore mechanism was correct and `iretq` would
have been wrong. *Resolution:* the manual return was kept; an identical latent bug was
found and fixed in `isr_ipi`, and the IF-masking fix from F2 was applied here too.

**F10. Stale TLB after preemptive switch** — `syscall/mod.rs`.
The `TLB_GENERATION` check ran only in the syscall postamble; a thread preempted via
the timer returned with a stale TLB after another CPU's `mprotect`/`munmap`.
*Resolution:* `PerCpu::reload_tlb_if_stale()` is now called on the timer-preemption
return path as well as the syscall postamble.

## 2. Architecture / overengineering — all resolved

**F11. Debug spam in hot paths** — every futex op, every timer tick, every preemption
did a synchronous debugcon write. *Resolution:* removed.

**F12. Test scaffolding in the production kernel** — `syscall/handlers/sumi_debug.rs`
(syscall 500) + `kthread_spawn`, compiled unconditionally. *Resolution:* deleted;
the `sched_yield`/`futex_wait_wake` tests were rewritten to use raw `clone` + an
`exit_thread` helper in `data/common.rs`.

**F13. Vacuous preempt_count** — nothing incremented it, so the `debug_assert` in
`schedule()` could not fail. *Resolution:* replaced with an explicit
`!interrupts_enabled()` assertion (the real invariant), per F6.

**F14. `cfg(not(test))`-gated production syscall bodies** — `process.rs`, `thread.rs`,
`clone.rs`, `random.rs` and much of `sched/` no-op under test, violating the
no-cfg-forks rule and leaving the riskiest logic untestable.
*Status: in progress.* Being resolved by introducing a minimal arch-leaf seam (the
`IrqGuard` shape) so handler/scheduler bodies compile and run identically under test,
deleting every non-arch `cfg(not(test))`, and adding host unit tests for the
now-testable logic (wake/on_cpu, steal, reaper guard, futex bookkeeping). This is the
last open audit item.

**F15. Dead / speculative code** — `PerCpu.syscall_stack_top` (no reader),
`hypercall::tlb_flush` + `HC_TLB_FLUSH` (no callers), `Thread.futex_bucket`
(write-only). *Resolution:* all three deleted (`Thread.robust_list_head` was kept — it
is ABI-visible via `set_robust_list`). Asm offsets that referenced the deleted
`syscall_stack_top` now use `offset_of!`-computed named operands.

**F16. Reaper ran on every context switch** — took the global zombie lock (even to
early-return) on every switch. *Resolution:* gated on a cheap `ZOMBIES_PENDING`
atomic set by `sys_exit`.

**F17. `sched/mod.rs` was 614 lines** (cap 500). *Resolution:* F12 deletion + moving
the Thread builders/trampolines into `thread.rs`; `mod.rs` is now 348 lines
(schedule/wake/steal/init) and `thread.rs` is 480.

## 3. Design doc (`multithreading-v2.md`) corrections — all applied

D1–D11 are all synced in the design doc: status header now reads "implemented (phases
0–9)"; Phase 9 preemption documented as mandatory (not optional); `ap_start.rs`
(`global_asm!`) not `ap_start.S`; MMIO hypercalls (offsets 0x00/0x08/0x10) documented
as the real mechanism; 3-arg `__switch_to_asm`; FXSAVE/`on_cpu`/`IrqGuard` described as
what shipped; the real `Blocked`-under-bucket-lock protocol and single-peer steal
documented; the already-fixed BRK/MMAP TOCTOU noted as closed; `reap_zombies` naming
and `Arc<Thread>` zombie list corrected.

## 4. Test coverage

- **T1 — done.** Acceptance tests added: `clone_einval`, `clone_settls`,
  `futex_wait_eagain`, `gettid_per_thread`, `exit_one_thread`, `clear_child_tid_wakes`,
  `exit_group_kills_all` (syscalls); `pthread_self.c`, `tls_keys.c`, `pthread_storm.c`
  (glibc, storm = 32 threads × 200 rounds); `thread_spawn`, `mutex_arc`,
  `mpsc_channel` (rust_std). Integration suite is 88/88.
- **T2/T3 — landing with F14.** Host-side unit coverage of the concurrency state
  machine (schedule/wake/steal, reaper guard, futex bookkeeping) is unlocked by F14's
  removal of the cfg-gating and added in the same change.

## 5. History note

Original execution plan: wave 1 = bugs F1–F13, F15–F17 + doc sync (done); wave 2 =
F14 seam + T1–T3 tests (T1 done, F14 + T2/T3 in progress); wave 3 = final adversarial
re-review of the whole diff (pending F14). The multithreading work has since been
rebased onto master as a single commit.
