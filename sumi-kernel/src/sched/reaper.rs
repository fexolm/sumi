//! Phase 7 zombie reaper. See docs/design/multithreading-v2.md §10.1.

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::sched::irq::IrqGuard;
use crate::sched::thread::Thread;

pub static ZOMBIE_LIST: spin::Mutex<Vec<Arc<Thread>>> = spin::Mutex::new(Vec::new());

/// Cheap hint checked by `reap_zombies` on every context switch before
/// taking `ZOMBIE_LIST.lock()` (F16). Set by `push_zombie`, cleared once
/// the list drains back to empty.
static ZOMBIES_PENDING: AtomicBool = AtomicBool::new(false);

pub fn push_zombie(t: Arc<Thread>) {
    // F5: irqsave, see `runqueue::RunQueue::push`.
    let _irq = IrqGuard::new();
    ZOMBIE_LIST.lock().push(t);
    ZOMBIES_PENDING.store(true, Ordering::Release);
}

#[cfg(not(test))]
pub fn reap_zombies() {
    if !ZOMBIES_PENDING.load(Ordering::Acquire) {
        return;
    }
    let _irq = IrqGuard::new();
    let mut zombies = ZOMBIE_LIST.lock();
    let mut i = 0;
    while i < zombies.len() {
        // F4: reap only once the exiting thread's context save has fully
        // completed (on_cpu == false). The old `current_thread`-scanning
        // check saw the thread as "off-CPU" as soon as schedule() published
        // `next`, which happens *before* `__switch_to_asm` finishes reading
        // the exiting thread's stack — freeing kernel_stack_phys underneath
        // it was a genuine use-after-free window.
        if zombies[i].on_cpu.load(Ordering::Acquire) {
            i += 1;
            continue;
        }
        let arc = zombies.swap_remove(i);
        let tid = arc.tid;
        let stack_phys = arc.kernel_stack_phys;
        let freeable = arc.kernel_stack_freeable;
        // Drop registry's Arc.
        let _ = crate::sched::registry::THREAD_REGISTRY.lock().unregister(tid);
        // Drop our Arc (may free the Thread if refcount reaches 0).
        drop(arc);
        // Free kernel stack page if it was palloc-allocated.
        if freeable {
            let _ = crate::PAGE_ALLOCATOR.free(stack_phys);
        }
        // i stays: swap_remove brought the tail here.
    }
    if zombies.is_empty() {
        ZOMBIES_PENDING.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::Ordering;
    use crate::sched::thread::ThreadState;

    fn new_zombie(tid: u32) -> Arc<Thread> {
        let t = Arc::new(Thread::new_test(tid));
        t.state.store(ThreadState::Exited as u32, Ordering::Relaxed);
        t
    }

    #[test]
    fn zombie_list_starts_empty() {
        ZOMBIE_LIST.lock().clear();
        assert_eq!(ZOMBIE_LIST.lock().len(), 0);
    }

    #[test]
    fn push_zombie_appends_to_list() {
        ZOMBIE_LIST.lock().clear();
        push_zombie(new_zombie(7001));
        push_zombie(new_zombie(7002));
        let g = ZOMBIE_LIST.lock();
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].tid.0, 7001);
        assert_eq!(g[1].tid.0, 7002);
        drop(g);
        ZOMBIE_LIST.lock().clear();
    }

    #[test]
    fn push_zombie_preserves_arc_count() {
        ZOMBIE_LIST.lock().clear();
        let t = new_zombie(7003);
        let t_clone = t.clone();
        assert_eq!(Arc::strong_count(&t), 2);
        push_zombie(t);
        assert_eq!(Arc::strong_count(&t_clone), 2);
        ZOMBIE_LIST.lock().clear();
        assert_eq!(Arc::strong_count(&t_clone), 1);
    }
}
