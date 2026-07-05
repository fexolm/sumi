//! Futex wait queues.
//!
//! See `docs/design/multithreading-v2.md` for the supported futex semantics.
//!
//! Design note — lock ordering:
//!   bucket.lock  →  runqueue.lock
//!
//! `wait()` sets `ThreadState::Blocked` **while holding the bucket lock**,
//! eliminating the classic lost-wakeup race where a waker's
//! Running→Runnable CAS (via `wake_blocked`) fires before the waiter
//! finishes transitioning to Blocked. Because the waker holds the same
//! bucket lock while calling `wake_blocked`, it cannot observe the waiter
//! in `Running` state: either the waiter has not yet pushed itself into
//! the bucket (and the waker will not see it), or it has pushed itself and
//! already set `Blocked` under the lock.

use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use crate::sched::thread::{Thread, ThreadState};

/// Number of hash buckets. Power of two.
pub const FUTEX_BUCKETS: usize = 256;

/// Per-bucket wait queue. Cache-line aligned to avoid false sharing
/// between buckets (buckets are laid out back-to-back in a static array).
#[repr(C, align(64))]
pub struct FutexBucket {
    pub lock: spin::Mutex<()>,
    /// Head of an intrusive singly-linked list of waiters, chained via
    /// `Thread.wait_link.next`. Mutated only while holding `lock`, but
    /// typed as `AtomicPtr` so an inner-loop walker can take an address
    /// of the slot uniformly (`head` and every `wait_link.next`).
    pub head: AtomicPtr<Thread>,
}

impl FutexBucket {
    pub const fn new() -> Self {
        Self {
            lock: spin::Mutex::new(()),
            head: AtomicPtr::new(core::ptr::null_mut()),
        }
    }
}

impl Default for FutexBucket {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: every field is either a Mutex (already Sync) or an atomic
// pointer. The intrusive list is always mutated under `lock`.
unsafe impl Sync for FutexBucket {}

static BUCKETS: [FutexBucket; FUTEX_BUCKETS] = [const { FutexBucket::new() }; FUTEX_BUCKETS];

/// Knuth multiplicative hash on (uaddr >> 2). The shift discards the two
/// low bits that are always zero for an aligned u32.
#[inline]
fn bucket_for(uaddr: usize) -> &'static FutexBucket {
    let h = (uaddr >> 2).wrapping_mul(0x9E3779B97F4A7C15);
    &BUCKETS[h & (FUTEX_BUCKETS - 1)]
}

/// Prepend `t` at the head of `bucket`. Caller must hold `bucket.lock`.
fn bucket_push(bucket: &FutexBucket, t: &Thread) {
    let t_ptr = t as *const Thread as *mut Thread;
    let old_head = bucket.head.load(Ordering::Relaxed);
    t.wait_link.next.store(old_head, Ordering::Relaxed);
    bucket.head.store(t_ptr, Ordering::Relaxed);
}

/// FUTEX_WAIT. Parks the current thread if `*uaddr == expected`, until
/// a matching `wake` unblocks it. Returns 0 on wake, EAGAIN if the
/// value already changed.
pub fn wait(uaddr: *const u32, expected: u32) -> i64 {
    use crate::syscall::errno::EAGAIN;

    let me = crate::sched::current_thread();
    let bucket = bucket_for(uaddr as usize);
    let g = bucket.lock.lock();

    // Re-check *uaddr under the bucket lock. `AtomicU32::from_ptr` lets us
    // read it with proper Acquire ordering without UB from a plain deref.
    //
    // SAFETY: sumi is a unikernel — kernel and user share the same
    // address space, all guest-visible memory is mapped, and callers
    // (raw-clone test worker threads, glibc pthread mutex paths) pass
    // aligned u32 addresses by construction. We do NOT validate
    // alignment here — a misaligned user pointer is UB.
    // TODO: align-check the user pointer in sys_futex.
    let cur = unsafe { AtomicU32::from_ptr(uaddr as *mut u32).load(Ordering::Acquire) };
    if cur != expected {
        drop(g);
        return EAGAIN;
    }

    // Publish self on the queue.
    me.wait_link.uaddr.store(uaddr as u64, Ordering::Relaxed);
    me.wait_link.bitset.store(!0, Ordering::Relaxed);
    bucket_push(bucket, me);

    // Transition to Blocked *under the bucket lock* so the waker (which
    // also holds this lock while calling wake_blocked) cannot race our
    // state transition.
    me.state
        .store(ThreadState::Blocked as u32, Ordering::Release);
    drop(g);

    // We are Blocked and in the queue. A concurrent wake will CAS us
    // Blocked→Runnable and push us on a runqueue; otherwise schedule()
    // falls through to idle until a wake arrives. Either way we resume
    // here once schedule() comes back.
    crate::sched::schedule();

    // schedule() returned. Normal path: state == Running (set by the
    // schedule that picked us). Race path (waker pushed us before we
    // reached schedule): state may be Runnable on the early-return ptr_eq
    // branch — harmless because nothing checks state here. The waker
    // already unlinked us from the bucket before calling wake_blocked.
    me.wait_link.uaddr.store(0, Ordering::Relaxed);
    0
}

/// FUTEX_WAIT_BITSET. Like `wait` but stores a caller-supplied `bitset`
/// instead of the default `!0`. A subsequent `wake_bitset` only wakes
/// threads whose `wait_link.bitset & wake_bitset != 0`.
///
/// glibc 2.34+ uses `FUTEX_WAIT_BITSET | FUTEX_CLOCK_REALTIME` with
/// `bitset = FUTEX_BITSET_MATCH_ANY` for timed waits; the bitset filter
/// is a no-op in that case but the syscall variant is required.
pub fn wait_bitset(uaddr: *const u32, expected: u32, bitset: u32) -> i64 {
    use crate::syscall::errno::EAGAIN;

    if bitset == 0 {
        return crate::syscall::errno::EINVAL;
    }

    let me = crate::sched::current_thread();
    let bucket = bucket_for(uaddr as usize);
    let g = bucket.lock.lock();

    let cur = unsafe { AtomicU32::from_ptr(uaddr as *mut u32).load(Ordering::Acquire) };
    if cur != expected {
        drop(g);
        return EAGAIN;
    }

    me.wait_link.uaddr.store(uaddr as u64, Ordering::Relaxed);
    me.wait_link.bitset.store(bitset, Ordering::Relaxed);
    bucket_push(bucket, me);
    me.state
        .store(ThreadState::Blocked as u32, Ordering::Release);
    drop(g);

    crate::sched::schedule();

    me.wait_link.uaddr.store(0, Ordering::Relaxed);
    0
}

/// FUTEX_WAKE_BITSET. Like `wake` but only wakes threads whose stored
/// `wait_link.bitset & wake_bitset != 0`.
pub fn wake_bitset(uaddr: *const u32, max: u32, bitset: u32) -> i64 {
    if max == 0 || bitset == 0 {
        return 0;
    }
    let bucket = bucket_for(uaddr as usize);
    let g = bucket.lock.lock();

    let mut woken: u32 = 0;
    let mut cur_link: *const AtomicPtr<Thread> = &bucket.head;
    loop {
        if woken >= max {
            break;
        }
        let cur_ptr = unsafe { (*cur_link).load(Ordering::Relaxed) };
        if cur_ptr.is_null() {
            break;
        }
        let cur = unsafe { &*cur_ptr };
        if cur.wait_link.uaddr.load(Ordering::Relaxed) == uaddr as u64
            && cur.wait_link.bitset.load(Ordering::Relaxed) & bitset != 0
        {
            let next_ptr = cur.wait_link.next.load(Ordering::Relaxed);
            unsafe {
                (*cur_link).store(next_ptr, Ordering::Release);
            }
            cur.wait_link
                .next
                .store(core::ptr::null_mut(), Ordering::Relaxed);
            debug_assert_eq!(
                cur.state.load(Ordering::Relaxed),
                ThreadState::Blocked as u32,
                "waiter found in futex bucket must be in Blocked state",
            );
            crate::sched::wake_blocked(cur);
            woken += 1;
        } else {
            cur_link = &cur.wait_link.next;
        }
    }
    drop(g);
    woken as i64
}

/// FUTEX_WAKE. Wakes up to `max` waiters queued on `uaddr`. Returns the
/// number actually woken.
pub fn wake(uaddr: *const u32, max: u32) -> i64 {
    if max == 0 {
        return 0;
    }
    let bucket = bucket_for(uaddr as usize);
    let g = bucket.lock.lock();

    let mut woken: u32 = 0;
    // `cur_link` is the slot that currently points at the thread under
    // consideration: it starts at `bucket.head` and moves to
    // `prev.wait_link.next` as we walk. Unlinking is a single store into
    // `*cur_link` that skips the current node.
    let mut cur_link: *const AtomicPtr<Thread> = &bucket.head;
    loop {
        if woken >= max {
            break;
        }
        // SAFETY: `cur_link` starts at `&bucket.head` and is reassigned
        // only to `&cur.wait_link.next` where `cur` is a valid, non-null
        // Thread pointer currently in the list. Both are live for the
        // duration of this walk because we hold `bucket.lock`.
        let cur_ptr = unsafe { (*cur_link).load(Ordering::Relaxed) };
        if cur_ptr.is_null() {
            break;
        }
        // SAFETY: cur_ptr came from the bucket list, populated by
        // `bucket_push` from a live `&Thread`. Threads are Arc-owned by
        // THREAD_REGISTRY for the kernel's lifetime, so the reference
        // remains valid.
        let cur = unsafe { &*cur_ptr };
        if cur.wait_link.uaddr.load(Ordering::Relaxed) == uaddr as u64 {
            let next_ptr = cur.wait_link.next.load(Ordering::Relaxed);
            // Unlink in place: store `next_ptr` into the slot that
            // previously pointed at `cur`. cur_link stays put so that
            // after the store it refers to "what was next".
            // SAFETY: see cur_link SAFETY above.
            unsafe {
                (*cur_link).store(next_ptr, Ordering::Release);
            }
            cur.wait_link
                .next
                .store(core::ptr::null_mut(), Ordering::Relaxed);
            // Invariant: every thread in this bucket was set to Blocked under
            // this same bucket lock by `wait()`. The wake_blocked CAS is therefore
            // guaranteed to find Blocked. If this fires, the lock-ordering or the
            // "set state in lock" discipline has been broken somewhere.
            debug_assert_eq!(
                cur.state.load(Ordering::Relaxed),
                ThreadState::Blocked as u32,
                "waiter found in futex bucket must be in Blocked state",
            );
            crate::sched::wake_blocked(cur);
            woken += 1;
            // Do NOT advance cur_link: the slot now holds `next_ptr`.
        } else {
            cur_link = &cur.wait_link.next;
        }
    }
    drop(g);
    woken as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_array_len_is_power_of_two() {
        assert_eq!(FUTEX_BUCKETS.count_ones(), 1);
        assert_eq!(BUCKETS.len(), FUTEX_BUCKETS);
    }

    #[test]
    fn bucket_for_is_stable() {
        let a = bucket_for(0x1000) as *const _;
        let b = bucket_for(0x1000) as *const _;
        assert_eq!(a, b);
    }

    #[test]
    fn bucket_for_distributes_across_aligned_addresses() {
        // Distinct 4-byte-aligned addresses should hit >= 200 distinct
        // buckets out of 256 for 1024 inputs.
        use std::collections::HashSet;
        let mut set: HashSet<*const FutexBucket> = HashSet::new();
        for i in 0..1024usize {
            set.insert(bucket_for(0x10_0000 + i * 4) as *const _);
        }
        assert!(set.len() >= 200, "poor distribution: {}", set.len());
    }

    #[test]
    fn bucket_for_masks_to_array_bounds() {
        let base = &BUCKETS[0] as *const _ as usize;
        let end = base + FUTEX_BUCKETS * core::mem::size_of::<FutexBucket>();
        for i in 0..4096usize {
            let p = bucket_for(i * 4) as *const _ as usize;
            assert!(p >= base && p < end);
        }
    }
}
