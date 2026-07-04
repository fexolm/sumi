//! Scheduler subsystem: Thread, RunQueue, context switching, and
//! cooperative + preemptive scheduling. See
//! `docs/design/multithreading-v2.md` §3.4, §4.3, §13 and
//! `docs/design/multithreading-fixes.md`.

extern crate alloc;

use core::sync::atomic::AtomicBool;

pub mod clone;
pub mod futex;
pub mod irq;
pub mod percpu;
pub mod reaper;
pub mod registry;
pub mod runqueue;
pub mod thread;

pub use percpu::{MAX_VCPUS, PerCpu};
pub use thread::{Thread, ThreadContext, ThreadState, Tid};

#[cfg(not(test))]
pub use percpu::{get as get_cpu, init_for_cpu, this_cpu};

/// Published by the BSP as the last step of its init path. APs spin on this
/// flag in `arch::x86_64::smp::ap_main_rust` before entering their idle loop,
/// so no AP can observe a partially-initialised kernel (virtio, FD table,
/// allocators, user-program image).
///
/// Single-writer (BSP), multi-reader (APs). Release on the BSP store pairs
/// with Acquire on the AP load, giving a happens-before edge that covers every
/// global the BSP wrote before publishing.
pub static KERNEL_READY: AtomicBool = AtomicBool::new(false);

/// Return the currently executing `Thread` on this CPU.
///
/// Must only be called after `init_phase3_bsp` / `init_phase3_ap` — the
/// `current_thread` field is non-null only from that point on.
#[cfg(not(test))]
pub fn current_thread() -> &'static Thread {
    use core::sync::atomic::Ordering;
    let ptr = percpu::this_cpu().current_thread.load(Ordering::Relaxed);
    debug_assert!(!ptr.is_null(), "current_thread() called before init_phase3");
    // SAFETY: `current_thread` is set to a valid Arc<Thread> backing by
    // init_phase3_bsp/ap and updated exclusively by schedule(). The Arc
    // lives in THREAD_REGISTRY for the kernel's lifetime.
    unsafe { &*ptr }
}

/// Cooperative reschedule point.
///
/// The caller is responsible for having placed `current_thread()` in the
/// correct state (Runnable + already on a runqueue, Blocked + on a wait
/// queue, Exited + on the zombie list) BEFORE calling. This function pops
/// the next runnable thread, or falls back to the idle thread if none exist.
///
/// Every kernel entry point disables interrupts (SFMASK clears IF on
/// syscall; all ISRs are interrupt gates that clear IF) and `__switch_to_asm`
/// keeps IF at 0 across a switch (F2), so IF must always be 0 here — see
/// the debug assertion below (F13: replaces the vacuous `preempt_count`
/// check, since nothing outside the timer ISR trampoline ever increments
/// `preempt_count`, so it can never actually be nonzero at this point).
#[cfg(not(test))]
pub fn schedule() {
    use core::sync::atomic::Ordering;

    debug_assert!(
        !irq::interrupts_enabled(),
        "schedule() called with interrupts enabled",
    );

    let cpu = percpu::this_cpu();

    let prev_ptr = cpu.current_thread.load(Ordering::Relaxed);
    debug_assert!(!prev_ptr.is_null(), "schedule() called before init_phase3_bsp/ap");

    // Consume the reschedule request BEFORE we pop, so any wake that
    // races with us either (a) lands in the runqueue before pop (and
    // we see it), or (b) lands after and sets need_resched again for
    // the next schedule() call. Either way no wake is lost.
    cpu.need_resched.store(false, Ordering::Release);

    // Pop the next runnable thread, or fall back to idle.
    let next_ptr: *mut Thread = cpu.runqueue.pop().unwrap_or_else(|| {
        let idle = cpu.idle_thread.load(Ordering::Relaxed);
        debug_assert!(!idle.is_null(), "idle_thread not set");
        idle
    });

    if core::ptr::eq(next_ptr, prev_ptr) {
        // Self-reschedule with empty queue. need_resched already cleared.
        return;
    }

    // SAFETY: prev and next are distinct (checked above). Each `ctx` belongs
    // exclusively to one Thread; the caller (sched_yield, syscall postamble,
    // kthread_pingpong, or idle_loop) ensures prev has been placed on the
    // correct list before calling, so prev.ctx is not aliased by another
    // CPU's runqueue traversal.
    let prev: &Thread = unsafe { &*prev_ptr };
    let next: &Thread = unsafe { &*next_ptr };

    // Demote prev from Running → Runnable, but only if it is still Running
    // (it could have been set to Blocked or Exited by the caller before this).
    // Skip the CAS for the idle thread: idle is always in Running state while
    // on-CPU, and demoting it to Runnable would conflict with `is_idle` logic.
    let idle_ptr = cpu.idle_thread.load(Ordering::Relaxed);
    if !core::ptr::eq(prev_ptr as *const _, idle_ptr as *const _) {
        let _ = prev.state.compare_exchange(
            ThreadState::Running as u32,
            ThreadState::Runnable as u32,
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
    }

    // F3/F4: mark next on-CPU *before* handing its ctx.rsp to
    // __switch_to_asm, so a concurrent wake_blocked/try_steal_work/
    // reap_zombies on another CPU that observes this store knows not to
    // touch it until __switch_to_asm clears it again (on next's own future
    // switch-away).
    next.on_cpu.store(true, Ordering::Release);
    next.state.store(ThreadState::Running as u32, Ordering::Release);
    next.cpu.store(cpu.cpu_id, Ordering::Relaxed);
    cpu.current_thread.store(next_ptr, Ordering::Release);

    let next_fs_base = next.fs_base.load(Ordering::Relaxed);

    // SAFETY: see SAFETY comment above. No non-irqsave spin::Mutex is held
    // across this call. `next_fs_base` is canonical: arch_prctl validates
    // user writes, clone forwards user pointers that either fault at access
    // or are valid. `&prev.on_cpu` points at the same Thread as `prev.ctx`.
    unsafe {
        crate::arch::x86_64::switch::__switch_to_asm(
            prev.ctx.get(),
            next.ctx.get(),
            next_fs_base,
            &prev.on_cpu,
        );
    }
    // Execution resumes here when a later schedule() switches back to `prev`.
    // Phase 7: reap any zombie threads no longer current on any CPU.
    reaper::reap_zombies();
}

/// Transition a blocked thread to Runnable and enqueue it on its home CPU.
///
/// No-op if the thread is not in the Blocked state (idempotent CAS). After
/// enqueuing, sets the target CPU's `need_resched` and sends an IPI if the
/// CPU is idling.
#[cfg(not(test))]
pub fn wake_blocked(t: &Thread) {
    use core::sync::atomic::Ordering;

    if t.state
        .compare_exchange(
            ThreadState::Blocked as u32,
            ThreadState::Runnable as u32,
            Ordering::AcqRel,
            Ordering::Relaxed,
        )
        .is_err()
    {
        return;
    }

    // F3: `t` may still be mid-switch — the waiter set Blocked and dropped
    // the bucket lock before its own schedule() finished saving its
    // context. Spin until that save completes (on_cpu observed false)
    // before handing t.ctx.rsp to another CPU's __switch_to_asm; otherwise
    // the same kernel stack could execute on two CPUs at once.
    while t.on_cpu.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }

    let home = t.cpu.load(Ordering::Relaxed);
    // Push to the thread's home CPU if it is online, else use this CPU.
    let target = percpu::get(home).unwrap_or_else(percpu::this_cpu);
    target.runqueue.push(t);
    target.need_resched.store(true, Ordering::Release);
    // Only kick a remote CPU: kicking ourselves is a no-op at best and
    // wasteful at worst. The idle flag check is still needed to avoid
    // sending an IPI when the target is already in the runqueue path.
    if target.cpu_id != percpu::this_cpu().cpu_id
        && target.is_idle.load(Ordering::Acquire)
    {
        crate::arch::x86_64::hypercall::kick_cpu(target.cpu_id);
    }
}

/// Initialise Phase 3 scheduler state for the BSP (cpu 0).
///
/// Constructs the "main" thread (TID = 1, wrapping the current boot stack)
/// and a dedicated idle thread (fresh 2 MB stack), registers both, and
/// stores their pointers in `PER_CPU[0]`. Must be called BEFORE
/// `KERNEL_READY.store(true, Release)` so APs see valid state.
#[cfg(not(test))]
pub fn init_phase3_bsp() {
    use core::sync::atomic::Ordering;

    let main = thread::build_current_main_thread();
    registry::register_main(main.clone());
    registry::LIVE_USER_THREADS.fetch_add(1, Ordering::Relaxed);

    let idle = thread::build_idle_thread();
    registry::register(idle.clone());

    let cpu = percpu::this_cpu();
    // SAFETY: Arc::as_ptr returns a pointer valid for the Arc's lifetime;
    // the Arc lives in THREAD_REGISTRY for the kernel's lifetime.
    let main_ptr = alloc::sync::Arc::as_ptr(&main) as *mut Thread;
    let idle_ptr = alloc::sync::Arc::as_ptr(&idle) as *mut Thread;
    cpu.current_thread.store(main_ptr, Ordering::Release);
    cpu.idle_thread.store(idle_ptr, Ordering::Release);

    main.cpu.store(cpu.cpu_id, Ordering::Relaxed);
    main.state.store(ThreadState::Running as u32, Ordering::Release);
}

/// Initialise Phase 3 scheduler state for an AP and enter the idle loop.
///
/// Builds an idle thread that reuses the AP's existing boot stack (no
/// allocation), registers it, stores it in `PER_CPU[cpu_id]`, and enters
/// `idle_loop`. Never returns.
#[cfg(not(test))]
pub fn init_phase3_ap(cpu_id: u32) -> ! {
    use core::sync::atomic::Ordering;

    let idle = thread::build_idle_thread_for_ap_reusing_boot_stack(cpu_id);
    registry::register(idle.clone());

    let cpu = percpu::this_cpu();
    // SAFETY: same as in init_phase3_bsp.
    let idle_ptr = alloc::sync::Arc::as_ptr(&idle) as *mut Thread;
    cpu.current_thread.store(idle_ptr, Ordering::Release);
    cpu.idle_thread.store(idle_ptr, Ordering::Release);
    idle.cpu.store(cpu.cpu_id, Ordering::Relaxed);
    idle.state.store(ThreadState::Running as u32, Ordering::Release);

    // Enter idle directly — we're already on the AP boot stack.
    idle_loop()
}

/// Try to steal one runnable thread from any other CPU's runqueue.
///
/// Iterates over all initialised CPUs (excluding this one). If a thread
/// is found, updates its home CPU to this CPU (so future wakeups land
/// here) and pushes it onto this CPU's runqueue. Returns `true` if a
/// thread was stolen.
///
/// Work-stealing is necessary so that a CPU idling with a non-empty
/// global workload does not starve threads that were enqueued on a busy
/// peer (e.g. a CPU blocked in a long syscall with IF=0).
#[cfg(not(test))]
fn try_steal_work() -> bool {
    use core::sync::atomic::Ordering;

    let cpu = percpu::this_cpu();
    let my_id = cpu.cpu_id;

    for id in 0..percpu::MAX_VCPUS as u32 {
        if id == my_id {
            continue;
        }
        let Some(peer) = percpu::get(id) else { continue };
        // Quick hint check before taking the lock.
        if peer.runqueue.load() == 0 {
            continue;
        }
        let Some(stolen_ptr) = peer.runqueue.pop() else { continue };
        // SAFETY: stolen_ptr came from a live runqueue entry; the backing
        // Thread lives in THREAD_REGISTRY for the kernel's lifetime.
        let stolen = unsafe { &*stolen_ptr };
        // F3: schedule_preempt() can push the currently-running thread onto
        // its own CPU's runqueue before that CPU's own schedule() call has
        // finished switching away from it. A concurrent steal from this CPU
        // could win that runqueue pop race, so wait for the context save to
        // finish (same protocol as wake_blocked) before using stolen.ctx.
        while stolen.on_cpu.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
        // Reroute the thread to run on this CPU so subsequent wakeups
        // (e.g. futex_wake) push to the correct runqueue.
        stolen.cpu.store(my_id, Ordering::Relaxed);
        cpu.runqueue.push(stolen);
        return true;
    }
    false
}

/// Idle loop. Sets `is_idle`, checks the runqueue, and `hlt`s if empty.
/// Loops back after a wake to re-check the queue.
///
/// This function never returns (it is the `-> !` endpoint for idle threads
/// on every CPU).
#[cfg(not(test))]
pub fn idle_loop() -> ! {
    use core::sync::atomic::Ordering;
    loop {
        // F6: schedule() must be called with IF=0 — it is not reentrant,
        // and a timer tick landing inside it (e.g. between the runqueue
        // check and the switch) would call schedule_preempt() while this
        // CPU is already mid-schedule(). IF is only re-enabled below at
        // the designated `sti; hlt` wait point.
        //
        // SAFETY: ring 0, no memory access, no stack use.
        unsafe { core::arch::asm!("cli", options(nomem, nostack, preserves_flags)); }

        let cpu = percpu::this_cpu();
        cpu.is_idle.store(true, Ordering::Release);
        // Re-check local runqueue AFTER publishing is_idle to close the
        // race where wake_blocked checks is_idle before we set it.
        if cpu.runqueue.load() > 0 {
            cpu.is_idle.store(false, Ordering::Release);
            schedule();
            continue;
        }
        // Local queue empty — try to steal from a peer CPU before parking.
        if try_steal_work() {
            cpu.is_idle.store(false, Ordering::Release);
            schedule();
            continue;
        }
        // Nothing runnable anywhere. Park the vCPU with interrupts enabled
        // so a wake (IPI) or timer tick can bring it out of `hlt`. `hlt`
        // traps to KVM_EXIT_HLT, parking this vCPU's host thread until
        // SIGUSR1 (from HC_KICK_CPU) wakes it.
        //
        // SAFETY: ring 0, no memory access, no stack use.
        unsafe { core::arch::asm!("sti; hlt", options(nomem, nostack)); }
        cpu.is_idle.store(false, Ordering::Release);
        // Loop back to re-check the runqueue (which re-asserts `cli`).
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::Ordering;

    #[test]
    fn kernel_ready_starts_false() {
        // Note: KERNEL_READY is a process-wide static, so this test is
        // order-sensitive. It runs early in the suite because no kernel code
        // path under `cargo test` ever sets it.
        assert!(!KERNEL_READY.load(Ordering::Acquire));
    }
}
