//! `EpollInstance`: the interest set for one `epoll_create1()` fd.
//!
//! Readiness computation lives in `syscall::handlers::epoll` (it needs the
//! FD table to resolve each interested fd's socket id); this module only
//! owns the registered `(fd, events, data)` triples.

use alloc::collections::BTreeMap;

/// One registered interest: the event mask the caller asked for, plus the
/// opaque `data` word `epoll_wait` must echo back unchanged.
pub struct EpollInterest {
    pub events: u32,
    pub data: u64,
}

pub struct EpollInstance {
    pub interest: BTreeMap<i32, EpollInterest>,
}

impl EpollInstance {
    pub fn new() -> Self {
        Self {
            interest: BTreeMap::new(),
        }
    }
}

impl Default for EpollInstance {
    fn default() -> Self {
        Self::new()
    }
}
