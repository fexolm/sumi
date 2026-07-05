//! Wait queue for blocking socket/epoll operations, mirroring the
//! `sched::futex` block/wake discipline (see `docs/networking-design.md`
//! and `sched::futex`'s module doc for the race this protocol closes).

use core::sync::atomic::Ordering;

use crate::sched::thread::{Thread, ThreadState};

/// Intrusive singly-linked wait queue, reusing `Thread.wait_link.next`. A
/// thread blocks in exactly one syscall at a time, so it is in exactly one
/// wait queue (futex or net) at a time — reusing the link field is safe.
pub struct WaitQueue {
    head: core::sync::atomic::AtomicPtr<Thread>,
}

impl WaitQueue {
    pub const fn new() -> Self {
        Self {
            head: core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()),
        }
    }

    /// Prepend `t` to the queue. Caller holds the NET lock.
    pub fn push(&mut self, t: &Thread) {
        let t_ptr = t as *const Thread as *mut Thread;
        let old_head = self.head.load(Ordering::Relaxed);
        t.wait_link.next.store(old_head, Ordering::Relaxed);
        self.head.store(t_ptr, Ordering::Relaxed);
    }

    /// Wake every waiter unconditionally (level-triggered: each waiter
    /// re-checks its own readiness/deadline on resume). Caller holds the
    /// NET lock; `wake_blocked` takes the runqueue lock internally — the
    /// only NET -> runqueue nesting in the design.
    pub fn wake_all(&mut self) {
        let mut cur_ptr = self.head.swap(core::ptr::null_mut(), Ordering::Relaxed);
        while !cur_ptr.is_null() {
            // SAFETY: `cur_ptr` came from `push`, which only stores the
            // address of a live `&Thread` (Arc-owned by THREAD_REGISTRY for
            // the kernel's lifetime).
            let cur = unsafe { &*cur_ptr };
            let next_ptr = cur.wait_link.next.load(Ordering::Relaxed);
            cur.wait_link
                .next
                .store(core::ptr::null_mut(), Ordering::Relaxed);
            debug_assert_eq!(
                cur.state.load(Ordering::Relaxed),
                ThreadState::Blocked as u32,
                "waiter found in NET wait queue must be in Blocked state",
            );
            crate::sched::wake_blocked(cur);
            cur_ptr = next_ptr;
        }
    }
}

impl Default for WaitQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Outcome of a `net_wait` readiness check.
pub enum Wait {
    /// Ready — return this syscall value immediately.
    Ready(i64),
    /// Not ready yet — park and re-check after the next wake.
    Block,
}

/// Block the current thread until `check` reports `Ready`, or `deadline_ns`
/// (if any) passes. Mirrors `sched::futex::wait`'s lost-wakeup-free
/// protocol: the thread is enqueued and set to `Blocked` under the NET
/// lock, the same lock a waker (`NetState::poll_and_wake`, called from here
/// and from the timer tick) holds while calling `wake_blocked`.
///
/// Every iteration polls the stack (`poll_and_wake`) before deciding to
/// block: over loopback this delivers the peer's bytes synchronously, so on
/// a single vCPU a cooperating pair of threads never actually parks (see
/// docs/networking-design.md). If a thread does park, the timer tick's
/// `net::poll()` re-runs `poll_and_wake` every ~1ms, so every waiter is
/// re-checked even with no further socket activity — this is what makes
/// `epoll_wait` timeouts fire.
pub fn net_wait(
    deadline_ns: Option<u64>,
    on_timeout: i64,
    mut check: impl FnMut(&mut super::NetState) -> Wait,
) -> i64 {
    loop {
        let mut g = super::lock();
        if let Wait::Ready(v) = check(&mut g) {
            drop(g);
            return v;
        }

        g.poll_and_wake();
        if let Wait::Ready(v) = check(&mut g) {
            drop(g);
            return v;
        }
        if let Some(dl) = deadline_ns
            && crate::time::monotonic_ns() >= dl
        {
            drop(g);
            return on_timeout;
        }
        let me = crate::sched::current_thread();
        g.waiters.push(me);
        if deadline_ns.is_some() {
            g.deadline_waiters = g.deadline_waiters.saturating_add(1);
        }
        // Transition to Blocked *under the NET lock* so the waker (which
        // also holds this lock while calling wake_blocked) cannot race our
        // state transition — same discipline as sched::futex::wait.
        me.state
            .store(ThreadState::Blocked as u32, Ordering::Release);
        drop(g);

        crate::sched::schedule();
        // Resumed: loop back, re-lock, re-check (level-triggered, tolerant
        // of spurious wakes).
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sched::thread::Thread;

    #[test]
    fn push_and_wake_all_empties_queue() {
        let t = Thread::new_test(100);
        t.state
            .store(ThreadState::Blocked as u32, Ordering::Relaxed);

        let mut q = WaitQueue::new();
        q.push(&t);
        assert!(!q.head.load(Ordering::Relaxed).is_null());

        q.wake_all();
        assert!(q.head.load(Ordering::Relaxed).is_null());
        assert_eq!(
            t.state.load(Ordering::Relaxed),
            ThreadState::Runnable as u32
        );
    }

    #[test]
    fn wake_all_on_empty_queue_is_a_noop() {
        let mut q = WaitQueue::new();
        q.wake_all();
        assert!(q.head.load(Ordering::Relaxed).is_null());
    }
}
