//! Per-CPU run queue — intrusive FIFO of runnable threads.
//!
//! All mutations are serialized by `inner` (a `spin::Mutex`). The `load`
//! counter is updated while the lock is held, but can be sampled without
//! the lock for a cheap "is anything runnable?" hint in the idle path.
//!
//! See `docs/design/multithreading-v2.md`.

use core::sync::atomic::{AtomicUsize, Ordering};

use super::irq::IrqGuard;
use super::thread::{RunLinkInner, Thread};

struct RunQueueInner {
    head: *mut Thread,
    tail: *mut Thread,
}

pub struct RunQueue {
    inner: spin::Mutex<RunQueueInner>,
    load:  AtomicUsize,
}

// SAFETY: RunQueue is logically owned by one PerCpu but can be pushed to by
// other CPUs (wake_blocked). spin::Mutex serializes concurrent pushes.
// Thread's own Sync impl covers the payload pointers.
unsafe impl Sync for RunQueue {}
// SAFETY: the raw pointers inside are only valid while the backing Thread
// Arcs are alive (in THREAD_REGISTRY). No ownership transfer occurs.
unsafe impl Send for RunQueue {}

impl Default for RunQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl RunQueue {
    pub const fn new() -> Self {
        Self {
            inner: spin::Mutex::new(RunQueueInner {
                head: core::ptr::null_mut(),
                tail: core::ptr::null_mut(),
            }),
            load: AtomicUsize::new(0),
        }
    }

    /// Append `t` to the tail. Safe to call from any CPU.
    pub fn push(&self, t: &Thread) {
        // irqsave: a timer tick that re-entered schedule_preempt while
        // this lock is held would self-deadlock.
        let _irq = IrqGuard::new();
        let t_ptr: *mut Thread = t as *const Thread as *mut Thread;
        let mut g = self.inner.lock();
        // SAFETY: `t.run_link` is only ever mutated while `self.inner` is
        // held. The push/pop pair covers every transition of next/prev.
        // The debug_assert here is inside the lock to avoid TOCTOU; we also
        // check head/tail aliasing to catch the single-element re-push case
        // (where both links are legitimately null but the thread is still
        // the lone entry in the queue).
        unsafe {
            let link = &*t.run_link.inner.get();
            debug_assert!(
                link.next.is_null()
                    && link.prev.is_null()
                    && !core::ptr::eq(g.head, t_ptr)
                    && !core::ptr::eq(g.tail, t_ptr),
                "double-push: thread already in a runqueue",
            );
            let link: &mut RunLinkInner = &mut *t.run_link.inner.get();
            link.next = core::ptr::null_mut();
            link.prev = g.tail;
            if g.tail.is_null() {
                g.head = t_ptr;
            } else {
                let tail_link: &mut RunLinkInner = &mut *(*g.tail).run_link.inner.get();
                tail_link.next = t_ptr;
            }
            g.tail = t_ptr;
        }
        // Update load while still under the lock so load() is never stale
        // in the direction that causes a spurious idle.
        self.load.fetch_add(1, Ordering::Relaxed);
    }

    /// Remove the head. Returns `None` if empty. Safe from any CPU.
    pub fn pop(&self) -> Option<*mut Thread> {
        // irqsave, see `push`.
        let _irq = IrqGuard::new();
        let mut g = self.inner.lock();
        let head = g.head;
        if head.is_null() {
            return None;
        }
        // SAFETY: `head` is valid (non-null) and its run_link is ours to
        // mutate under the lock.
        unsafe {
            let link: &mut RunLinkInner = &mut *(*head).run_link.inner.get();
            g.head = link.next;
            if g.head.is_null() {
                g.tail = core::ptr::null_mut();
            } else {
                let next_link: &mut RunLinkInner = &mut *(*g.head).run_link.inner.get();
                next_link.prev = core::ptr::null_mut();
            }
            // Clear stale links so a re-push does not see leftover pointers.
            link.next = core::ptr::null_mut();
            link.prev = core::ptr::null_mut();
        }
        self.load.fetch_sub(1, Ordering::Relaxed);
        Some(head)
    }

    /// Number of threads currently in the queue. May be transiently stale
    /// (sampled without the lock) — only use as a hint.
    pub fn load(&self) -> usize {
        self.load.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate alloc;
    use alloc::boxed::Box;

    fn make_thread(tid: u32) -> Box<Thread> {
        Box::new(Thread::new_test(tid))
    }

    #[test]
    fn empty_queue_pop_returns_none() {
        let q = RunQueue::new();
        assert!(q.pop().is_none());
    }

    #[test]
    fn load_starts_at_zero() {
        let q = RunQueue::new();
        assert_eq!(q.load(), 0);
    }

    #[test]
    fn push_increments_load() {
        let q = RunQueue::new();
        let t = make_thread(1);
        q.push(&t);
        assert_eq!(q.load(), 1);
    }

    #[test]
    fn push_pop_roundtrip() {
        let q = RunQueue::new();
        let t = make_thread(42);
        let t_ptr = &*t as *const Thread as *mut Thread;
        q.push(&t);
        let popped = q.pop().expect("should have one entry");
        assert_eq!(popped, t_ptr);
        assert_eq!(q.load(), 0);
    }

    #[test]
    fn fifo_ordering() {
        let q = RunQueue::new();
        let a = make_thread(1);
        let b = make_thread(2);
        let c = make_thread(3);
        let a_ptr = &*a as *const Thread as *mut Thread;
        let b_ptr = &*b as *const Thread as *mut Thread;
        let c_ptr = &*c as *const Thread as *mut Thread;
        q.push(&a);
        q.push(&b);
        q.push(&c);
        assert_eq!(q.pop().unwrap(), a_ptr);
        assert_eq!(q.pop().unwrap(), b_ptr);
        assert_eq!(q.pop().unwrap(), c_ptr);
        assert!(q.pop().is_none());
    }

    #[test]
    fn load_decrements_on_pop() {
        let q = RunQueue::new();
        let t1 = make_thread(7);
        let t2 = make_thread(8);
        q.push(&t1);
        q.push(&t2);
        assert_eq!(q.load(), 2);
        q.pop();
        assert_eq!(q.load(), 1);
        q.pop();
        assert_eq!(q.load(), 0);
    }

    #[test]
    fn interleaved_push_pop() {
        let q = RunQueue::new();
        let a = make_thread(1);
        let b = make_thread(2);
        let a_ptr = &*a as *const Thread as *mut Thread;
        let b_ptr = &*b as *const Thread as *mut Thread;
        q.push(&a);
        let p = q.pop().unwrap();
        assert_eq!(p, a_ptr);
        q.push(&b);
        let p2 = q.pop().unwrap();
        assert_eq!(p2, b_ptr);
    }
}
