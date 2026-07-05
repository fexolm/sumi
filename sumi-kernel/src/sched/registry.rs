//! Global thread registry — maps Tid to Arc<Thread>.
//!
//! See `docs/design/multithreading-v2.md`.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::sync::Arc;

use super::thread::{Thread, Tid};

pub struct ThreadRegistry {
    by_tid: BTreeMap<u32, Arc<Thread>>,
    next_tid: u32,
}

impl Default for ThreadRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadRegistry {
    pub const fn new() -> Self {
        Self {
            by_tid: BTreeMap::new(),
            // next_tid starts at 2: 0 is reserved, 1 is the BSP main thread
            // (registered separately via `register_main`).
            next_tid: 2,
        }
    }

    /// Allocate the next available TID. Never returns 0 or 1.
    ///
    /// Uses wrapping arithmetic; the debug_assert fires if the TID space
    /// wraps around to the reserved values 0 or 1 (requires 4G+ threads).
    pub fn alloc_tid(&mut self) -> Tid {
        let tid = self.next_tid;
        let next = self.next_tid.wrapping_add(1);
        // Panic if TID space wraps: 4 billion threads is a runaway leak.
        debug_assert!(next > 1, "TID space wrapped (4G threads)");
        self.next_tid = next;
        Tid(tid)
    }

    /// Insert a thread. Panics (debug_assert) if the TID is already present.
    pub fn register(&mut self, t: Arc<Thread>) {
        let tid = t.tid.0;
        debug_assert!(!self.by_tid.contains_key(&tid), "duplicate TID");
        self.by_tid.insert(tid, t);
    }

    pub fn lookup(&self, tid: Tid) -> Option<Arc<Thread>> {
        self.by_tid.get(&tid.0).cloned()
    }

    pub fn unregister(&mut self, tid: Tid) -> Option<Arc<Thread>> {
        self.by_tid.remove(&tid.0)
    }

    pub fn alive_count(&self) -> usize {
        self.by_tid.len()
    }
}

pub static THREAD_REGISTRY: spin::Mutex<ThreadRegistry> = spin::Mutex::new(ThreadRegistry::new());

/// Count of live *user* threads: the BSP main thread plus every `clone()`/
/// `clone3()` child, incremented at creation and decremented by `sys_exit`.
/// Deliberately separate from `ThreadRegistry::alive_count`, which also
/// counts idle threads and zombies still awaiting the reaper — counting
/// those make the "last thread exits" check in `sys_exit` impossible to
/// express with `alive_count()`.
pub static LIVE_USER_THREADS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Allocate a fresh TID from the global registry.
pub fn alloc_tid() -> Tid {
    THREAD_REGISTRY.lock().alloc_tid()
}

/// Register a thread. Keeps an Arc alive for the kernel's lifetime.
pub fn register(t: Arc<Thread>) {
    THREAD_REGISTRY.lock().register(t);
}

/// Register the BSP main thread (TID = 1). Must be called before any
/// `alloc_tid()` so that TID 1 is not accidentally reused.
pub fn register_main(t: Arc<Thread>) {
    let mut g = THREAD_REGISTRY.lock();
    debug_assert_eq!(t.tid, Tid(1), "register_main called with wrong TID");
    debug_assert!(!g.by_tid.contains_key(&1), "main thread already registered");
    g.by_tid.insert(1, t);
}

pub fn lookup(tid: Tid) -> Option<Arc<Thread>> {
    THREAD_REGISTRY.lock().lookup(tid)
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate alloc;
    use alloc::sync::Arc;

    fn make_thread(tid: u32) -> Arc<Thread> {
        Arc::new(Thread::new_test(tid))
    }

    #[test]
    fn alloc_tid_starts_at_2() {
        let mut reg = ThreadRegistry::new();
        let tid = reg.alloc_tid();
        assert_eq!(tid, Tid(2));
    }

    #[test]
    fn alloc_tid_increments() {
        let mut reg = ThreadRegistry::new();
        let t1 = reg.alloc_tid();
        let t2 = reg.alloc_tid();
        assert_eq!(t1, Tid(2));
        assert_eq!(t2, Tid(3));
    }

    #[test]
    fn register_and_lookup() {
        let mut reg = ThreadRegistry::new();
        let t = make_thread(42);
        reg.register(t.clone());
        let found = reg.lookup(Tid(42));
        assert!(found.is_some());
        assert_eq!(found.unwrap().tid, Tid(42));
    }

    #[test]
    fn unregister_removes_entry() {
        let mut reg = ThreadRegistry::new();
        let t = make_thread(5);
        reg.register(t);
        let removed = reg.unregister(Tid(5));
        assert!(removed.is_some());
        assert!(reg.lookup(Tid(5)).is_none());
    }

    #[test]
    fn alive_count_tracks_insertions() {
        let mut reg = ThreadRegistry::new();
        assert_eq!(reg.alive_count(), 0);
        reg.register(make_thread(10));
        assert_eq!(reg.alive_count(), 1);
        reg.register(make_thread(11));
        assert_eq!(reg.alive_count(), 2);
        reg.unregister(Tid(10));
        assert_eq!(reg.alive_count(), 1);
    }

    #[test]
    fn register_main_stores_tid1() {
        let mut reg = ThreadRegistry::new();
        let t = Arc::new(Thread::new_test(1));
        reg.by_tid.insert(1, t.clone()); // simulate register_main
        assert!(reg.lookup(Tid(1)).is_some());
    }
}
