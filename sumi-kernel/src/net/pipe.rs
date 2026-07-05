//! In-kernel `pipe`/`pipe2`, implemented inside the net module so it reuses
//! `net_wait`/`NetStateInner::waiters`/`poll_and_wake` — the same
//! block/wake machinery TCP sockets use (see `net::mod`'s doc comment) —
//! giving pipes blocking reads/writes and epoll/poll readiness for free.
//!
//! The pure state transition (`try_read`/`try_write`) is kept separate from
//! the locking/blocking wrapper (`pipe_read`/`pipe_write`) so it is
//! testable without the global `NET` singleton or a running scheduler,
//! mirroring how `socket::readiness` is tested independently of
//! `syscall::handlers::net`'s blocking dispatch.

use alloc::collections::VecDeque;

use crate::syscall::errno::{EAGAIN, EBADF, EPIPE};

use super::socket::{EPOLLERR, EPOLLHUP, EPOLLIN, EPOLLOUT};
use super::wait::Wait;

/// PIPE_BUF-style capacity cap, matching Linux's default pipe buffer size
/// (16 * 4 KiB pages). A write that would grow the buffer past this either
/// blocks or returns EAGAIN (nonblocking) instead of growing unbounded.
pub const PIPE_CAPACITY: usize = 65536;

pub struct PipeState {
    pub buf: VecDeque<u8>,
    pub readers: u32,
    pub writers: u32,
}

impl PipeState {
    fn new() -> Self {
        Self {
            buf: VecDeque::new(),
            readers: 1,
            writers: 1,
        }
    }
}

/// One read attempt against `p`, with no locking or blocking.
fn try_read(p: &mut PipeState, buf: &mut [u8]) -> Wait {
    if p.buf.is_empty() {
        return if p.writers == 0 {
            Wait::Ready(0) // EOF: no writer left, nothing buffered.
        } else {
            Wait::Block
        };
    }
    let n = buf.len().min(p.buf.len());
    for slot in buf[..n].iter_mut() {
        *slot = p.buf.pop_front().expect("checked non-empty above");
    }
    Wait::Ready(n as i64)
}

/// One write attempt against `p`, with no locking or blocking.
fn try_write(p: &mut PipeState, data: &[u8]) -> Wait {
    if p.readers == 0 {
        return Wait::Ready(EPIPE);
    }
    let space = PIPE_CAPACITY.saturating_sub(p.buf.len());
    if space == 0 {
        return Wait::Block;
    }
    let n = data.len().min(space);
    p.buf.extend(data[..n].iter().copied());
    Wait::Ready(n as i64)
}

/// Epoll/poll readiness for one end of a pipe (mirrors `socket::readiness`).
pub fn pipe_readiness(p: &PipeState, write_end: bool) -> u32 {
    if write_end {
        if p.readers == 0 {
            EPOLLERR
        } else if p.buf.len() < PIPE_CAPACITY {
            EPOLLOUT
        } else {
            0
        }
    } else {
        let mut ev = 0;
        if !p.buf.is_empty() || p.writers == 0 {
            ev |= EPOLLIN;
        }
        if p.writers == 0 {
            ev |= EPOLLHUP;
        }
        ev
    }
}

/// Allocate a fresh pipe (one reader, one writer) and return its id.
pub fn pipe_create() -> usize {
    super::lock().pipe_alloc(PipeState::new())
}

/// Read from the read end of pipe `id`, honoring `nonblocking`.
pub(crate) fn pipe_read(id: usize, buf: &mut [u8], nonblocking: bool) -> i64 {
    if buf.is_empty() {
        return 0;
    }
    let mut attempt = |g: &mut super::NetState| -> Wait {
        match g.pipe_get_mut(id) {
            Some(p) => try_read(p, buf),
            None => Wait::Ready(EBADF),
        }
    };

    let mut g = super::lock();
    match attempt(&mut g) {
        Wait::Ready(v) => {
            // Freed buffer space — wake any writer blocked on backpressure.
            g.poll_and_wake();
            v
        }
        Wait::Block => {
            drop(g);
            if nonblocking {
                EAGAIN
            } else {
                super::net_wait(None, 0, attempt)
            }
        }
    }
}

/// Write to the write end of pipe `id`, honoring `nonblocking`.
pub(crate) fn pipe_write(id: usize, data: &[u8], nonblocking: bool) -> i64 {
    if data.is_empty() {
        return 0;
    }
    let attempt = |g: &mut super::NetState| -> Wait {
        match g.pipe_get_mut(id) {
            Some(p) => try_write(p, data),
            None => Wait::Ready(EBADF),
        }
    };

    let mut g = super::lock();
    match attempt(&mut g) {
        Wait::Ready(v) => {
            // Made room / delivered bytes — wake any blocked reader.
            g.poll_and_wake();
            v
        }
        Wait::Block => {
            drop(g);
            if nonblocking {
                EAGAIN
            } else {
                super::net_wait(None, 0, attempt)
            }
        }
    }
}

/// Close one end (`write_end`) of pipe `id`. Callers (`sys_close`/`sys_dup2`
/// in `syscall::handlers::io`) have already established via
/// `FdTable::count_pipe_refs` that no other fd shares this (id, write_end)
/// pair, so this always decrements the matching counter. Frees the slot
/// once both ends are closed, and wakes the opposite side so it observes
/// EOF (writer closed) or EPIPE (reader closed) on its next attempt.
pub fn close_pipe(id: usize, write_end: bool) {
    let mut g = super::lock();
    let Some(p) = g.pipe_get_mut(id) else {
        return;
    };
    if write_end {
        p.writers = p.writers.saturating_sub(1);
    } else {
        p.readers = p.readers.saturating_sub(1);
    }
    if p.readers == 0 && p.writers == 0 {
        g.pipe_free(id);
    }
    g.poll_and_wake();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready(w: Wait) -> i64 {
        match w {
            Wait::Ready(v) => v,
            Wait::Block => panic!("expected Ready, got Block"),
        }
    }

    fn is_block(w: Wait) -> bool {
        matches!(w, Wait::Block)
    }

    #[test]
    fn write_then_read_roundtrip() {
        let mut p = PipeState::new();
        assert_eq!(ready(try_write(&mut p, b"hello")), 5);
        let mut buf = [0u8; 16];
        let n = ready(try_read(&mut p, &mut buf)) as usize;
        assert_eq!(&buf[..n], b"hello");
    }

    #[test]
    fn read_on_empty_buffer_with_open_writer_blocks() {
        let mut p = PipeState::new();
        let mut buf = [0u8; 8];
        assert!(is_block(try_read(&mut p, &mut buf)));
    }

    #[test]
    fn read_returns_eof_after_writer_closes() {
        let mut p = PipeState::new();
        p.writers = 0;
        let mut buf = [0u8; 8];
        assert_eq!(ready(try_read(&mut p, &mut buf)), 0);
    }

    #[test]
    fn write_returns_epipe_after_reader_closes() {
        let mut p = PipeState::new();
        p.readers = 0;
        assert_eq!(ready(try_write(&mut p, b"x")), EPIPE);
    }

    #[test]
    fn write_blocks_at_capacity() {
        let mut p = PipeState::new();
        p.buf.extend(core::iter::repeat_n(0u8, PIPE_CAPACITY));
        assert!(is_block(try_write(&mut p, b"x")));
    }

    #[test]
    fn write_short_fills_only_remaining_capacity() {
        let mut p = PipeState::new();
        p.buf.extend(core::iter::repeat_n(0u8, PIPE_CAPACITY - 3));
        // Only 3 bytes of space left — a 5-byte write is short.
        assert_eq!(ready(try_write(&mut p, b"hello")), 3);
        assert_eq!(p.buf.len(), PIPE_CAPACITY);
    }

    #[test]
    fn nonblocking_dispatch_maps_block_to_eagain() {
        // Mirrors the `Wait::Block => EAGAIN` arm in `pipe_read`/`pipe_write`.
        let mut p = PipeState::new();
        let mut buf = [0u8; 4];
        let read_result = match try_read(&mut p, &mut buf) {
            Wait::Ready(v) => v,
            Wait::Block => EAGAIN,
        };
        assert_eq!(read_result, EAGAIN);

        p.buf.extend(core::iter::repeat_n(0u8, PIPE_CAPACITY));
        let write_result = match try_write(&mut p, b"x") {
            Wait::Ready(v) => v,
            Wait::Block => EAGAIN,
        };
        assert_eq!(write_result, EAGAIN);
    }

    #[test]
    fn readiness_read_end_tracks_data_and_writer_hangup() {
        let mut p = PipeState::new();
        assert_eq!(
            pipe_readiness(&p, false),
            0,
            "empty, writer open -> not readable"
        );
        p.buf.push_back(b'x');
        assert_eq!(pipe_readiness(&p, false), EPOLLIN);
        p.buf.clear();
        p.writers = 0;
        assert_eq!(
            pipe_readiness(&p, false),
            EPOLLIN | EPOLLHUP,
            "writer closed -> EOF readable + hangup"
        );
    }

    #[test]
    fn readiness_write_end_tracks_space_and_reader_hangup() {
        let mut p = PipeState::new();
        assert_eq!(pipe_readiness(&p, true), EPOLLOUT);
        p.buf.extend(core::iter::repeat_n(0u8, PIPE_CAPACITY));
        assert_eq!(pipe_readiness(&p, true), 0, "full buffer -> not writable");
        p.readers = 0;
        assert_eq!(pipe_readiness(&p, true), EPOLLERR);
    }

    #[test]
    fn net_state_pipe_alloc_reuses_freed_slots() {
        let mut st = crate::net::TestNetState::new_loopback();
        let id0 = st.pipe_alloc(PipeState::new());
        st.pipe_free(id0);
        let id1 = st.pipe_alloc(PipeState::new());
        assert_eq!(id0, id1);
    }
}
