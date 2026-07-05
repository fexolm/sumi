//! `epoll_create1`/`epoll_ctl`/`epoll_wait` over the Phase 1 socket layer.
//!
//! Readiness is level-triggered: `epoll_wait` recomputes every registered
//! fd's readiness from scratch each time it wakes (no separate "just
//! became ready" edge tracking), matching `net::wait::net_wait`'s
//! poll-and-recheck discipline.

use alloc::vec::Vec;

use crate::fs::FdKind;
use crate::net::{self, Wait, net_wait, socket};
use crate::syscall::errno::*;
use crate::syscall::{SyscallArgs, SyscallResult};

const EPOLL_CTL_ADD: u64 = 1;
const EPOLL_CTL_DEL: u64 = 2;
const EPOLL_CTL_MOD: u64 = 3;

/// Size of one `struct epoll_event` on x86_64 (packed: 4-byte events + an
/// 8-byte data word).
const EPOLL_EVENT_SIZE: u64 = 12;

/// Resolve an epoll fd to its `net::NetState::epolltab` id.
fn epoll_fd(fd: i64) -> Result<usize, SyscallResult> {
    if fd < 0 {
        return Err(EBADF);
    }
    let table = crate::FD_TABLE.lock();
    match table.get(fd as usize) {
        Some(d) => match d.kind {
            FdKind::Epoll { id } => Ok(id),
            _ => Err(EINVAL),
        },
        None => Err(EBADF),
    }
}

pub fn sys_epoll_create1(_args: &SyscallArgs) -> SyscallResult {
    let id = net::lock().epoll_alloc(net::EpollInstance::new());
    crate::FD_TABLE.lock().alloc(crate::fs::FileDescriptor {
        kind: FdKind::Epoll { id },
        flags: 0,
    }) as SyscallResult
}

/// `epoll_create(size)` — deprecated alias; `size` is ignored (same as
/// Linux, which only ever used it as a hint).
pub fn sys_epoll_create(args: &SyscallArgs) -> SyscallResult {
    sys_epoll_create1(args)
}

pub fn sys_epoll_ctl(args: &SyscallArgs) -> SyscallResult {
    let epoll_id = match epoll_fd(args.arg0 as i64) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let op = args.arg1;
    let target_fd = args.arg2 as i32;
    let ev_ptr = args.arg3;

    // Verify the target fd exists. Independent of NET — just an FD_TABLE
    // lookup, dropped immediately (Invariant N1).
    {
        let table = crate::FD_TABLE.lock();
        if table.get(target_fd as usize).is_none() {
            return EBADF;
        }
    }

    match op {
        EPOLL_CTL_ADD | EPOLL_CTL_MOD => {
            if ev_ptr == 0 {
                return EFAULT;
            }
            // SAFETY: `ev_ptr` points at a valid `struct epoll_event` per
            // the syscall ABI.
            let ev = unsafe { net::read_epoll_event(ev_ptr) };
            // Always implicitly watch for error/hangup, matching Linux's
            // epoll (EPOLLERR/EPOLLHUP fire regardless of the requested mask).
            let events = ev.events | socket::EPOLLERR | socket::EPOLLHUP;

            let mut g = net::lock();
            let Some(epi) = g.epoll_get_mut(epoll_id) else {
                return EBADF;
            };
            if op == EPOLL_CTL_ADD {
                if epi.interest.contains_key(&target_fd) {
                    return EEXIST;
                }
                epi.interest.insert(
                    target_fd,
                    net::epoll::EpollInterest {
                        events,
                        data: ev.data,
                    },
                );
            } else {
                let Some(slot) = epi.interest.get_mut(&target_fd) else {
                    return ENOENT;
                };
                slot.events = events;
                slot.data = ev.data;
            }
            0
        }
        EPOLL_CTL_DEL => {
            let mut g = net::lock();
            let Some(epi) = g.epoll_get_mut(epoll_id) else {
                return EBADF;
            };
            if epi.interest.remove(&target_fd).is_none() {
                return ENOENT;
            }
            0
        }
        _ => EINVAL,
    }
}

/// Snapshot `(socket_id, interest_events, data)` for every fd registered on
/// `epoll_id`, resolving each fd to its socket id. Locks `NET` (to read the
/// interest set) and then `FD_TABLE` (to resolve fds) — sequentially, never
/// nested — once, before any possible block, so the `net_wait` loop itself
/// never touches `FD_TABLE` (Invariant N1). fds that aren't sockets, or
/// that were closed after being registered, are silently skipped: they can
/// never contribute smoltcp-backed readiness.
fn snapshot_interest(epoll_id: usize) -> Result<Vec<(usize, u32, u64)>, SyscallResult> {
    let raw: Vec<(i32, u32, u64)> = {
        let g = net::lock();
        let Some(epi) = g.epoll_get(epoll_id) else {
            return Err(EBADF);
        };
        epi.interest
            .iter()
            .map(|(&fd, i)| (fd, i.events, i.data))
            .collect()
    };

    let table = crate::FD_TABLE.lock();
    Ok(raw
        .into_iter()
        .filter_map(|(fd, events, data)| match table.get(fd as usize) {
            Some(d) => match d.kind {
                FdKind::Socket { id } => Some((id, events, data)),
                _ => None,
            },
            None => None,
        })
        .collect())
}

/// Recompute readiness for every entry in `snapshot` against the current
/// `NetState`, capped at `max` events.
fn compute_ready(g: &net::NetState, snapshot: &[(usize, u32, u64)], max: usize) -> Vec<(u32, u64)> {
    let mut out = Vec::new();
    for &(id, events, data) in snapshot {
        if out.len() >= max {
            break;
        }
        if let Some(obj) = g.socket_get(id) {
            let r = socket::readiness(obj, &g.sockets) & events;
            if r != 0 {
                out.push((r, data));
            }
        }
    }
    out
}

pub fn sys_epoll_wait(args: &SyscallArgs) -> SyscallResult {
    let epoll_id = match epoll_fd(args.arg0 as i64) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let events_ptr = args.arg1;
    let maxevents = args.arg2 as i32;
    let timeout_ms = args.arg3 as i32;

    if !(1..=1024).contains(&maxevents) {
        return EINVAL;
    }
    let snapshot = match snapshot_interest(epoll_id) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let max = maxevents as usize;

    let ready: Vec<(u32, u64)> = if timeout_ms == 0 {
        let mut g = net::lock();
        g.poll_and_wake();
        compute_ready(&g, &snapshot, max)
    } else {
        let deadline = if timeout_ms < 0 {
            None
        } else {
            Some(crate::time::monotonic_ns() + timeout_ms as u64 * 1_000_000)
        };
        let mut out = Vec::new();
        net_wait(deadline, 0, |g| {
            let ev = compute_ready(g, &snapshot, max);
            if ev.is_empty() {
                Wait::Block
            } else {
                let n = ev.len() as i64;
                out = ev;
                Wait::Ready(n)
            }
        });
        out
    };

    for (i, &(events, data)) in ready.iter().enumerate() {
        // SAFETY: `events_ptr .. events_ptr + maxevents * 12` is a valid,
        // writable user buffer per the syscall ABI (caller-allocated
        // `struct epoll_event[maxevents]`), and `i < ready.len() <= max <=
        // maxevents`.
        unsafe {
            net::write_epoll_event(
                events_ptr + i as u64 * EPOLL_EVENT_SIZE,
                &net::EpollEvent { events, data },
            );
        }
    }
    ready.len() as SyscallResult
}

/// `epoll_pwait(epfd, events, maxevents, timeout, sigmask, sigsetsize)` —
/// Phase 1 has no signal delivery to mask, so this is exactly `epoll_wait`.
pub fn sys_epoll_pwait(args: &SyscallArgs) -> SyscallResult {
    sys_epoll_wait(args)
}
