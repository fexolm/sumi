//! Per-fd socket state: the mapping between a `Socket` fd (an id into
//! `NetState::socktab`) and its smoltcp handle(s).
//!
//! smoltcp has no `accept()`: a `tcp::Socket` is either `Listen` or
//! connected. A listening `SocketObject` therefore owns a pool of listening
//! smoltcp sockets (`backlog`); `accept4` hands out whichever pool socket
//! has left `Listen` and refills the pool with a fresh `listen()` call (see
//! `syscall/handlers/net.rs::sys_accept4`).

use alloc::vec::Vec;

use smoltcp::iface::{SocketHandle, SocketSet};
use smoltcp::socket::tcp;
use smoltcp::wire::{IpEndpoint, IpListenEndpoint};

use super::stack::{TCP_RX, TCP_TX};

pub const AF_INET: u16 = 2;
pub const AF_INET6: u16 = 10;
pub const SOCK_STREAM: u16 = 1;
pub const SOCK_NONBLOCK: u32 = 0o4000; // 0x800 == O_NONBLOCK
pub const SOCK_CLOEXEC: u32 = 0o2000000;

pub const EPOLLIN: u32 = 0x001;
pub const EPOLLOUT: u32 = 0x004;
pub const EPOLLERR: u32 = 0x008;
pub const EPOLLHUP: u32 = 0x010;

pub enum SockRole {
    Listener,
    Stream,
}

pub struct SocketObject {
    pub domain: u16,
    pub typ: u16,
    pub nonblocking: bool,
    pub role: SockRole,
    /// Stream: the connected smoltcp socket.
    pub handle: Option<SocketHandle>,
    /// Listener: the pool of listening smoltcp sockets.
    pub backlog: Vec<SocketHandle>,
    /// Bound local address, set by `bind()`.
    pub local: Option<IpEndpoint>,
    /// Listener: endpoint used to refill the backlog pool after accept.
    pub listen_ep: Option<IpListenEndpoint>,
}

/// A freshly created (unconnected, unlisted) TCP socket with Phase 1's
/// fixed buffer sizes.
pub fn new_tcp() -> tcp::Socket<'static> {
    let rx = tcp::SocketBuffer::new(alloc::vec![0u8; TCP_RX]);
    let tx = tcp::SocketBuffer::new(alloc::vec![0u8; TCP_TX]);
    tcp::Socket::new(rx, tx)
}

/// States in which the remote end has closed its side of the connection —
/// treated as "readable" (the next `recv` observes EOF) for epoll purposes.
/// Also used directly by `syscall::handlers::net::recv_bytes` to tell
/// `recv_slice`'s `Ok(0)` (EOF) apart from `Ok(0)` (nothing buffered yet).
pub(crate) fn peer_closed(state: tcp::State) -> bool {
    matches!(
        state,
        tcp::State::CloseWait
            | tcp::State::LastAck
            | tcp::State::Closing
            | tcp::State::Closed
            | tcp::State::TimeWait
    )
}

/// Compute the epoll readiness mask for `obj` given the current socket set.
pub fn readiness(obj: &SocketObject, sockets: &SocketSet) -> u32 {
    match obj.role {
        SockRole::Listener => {
            let any_ready = obj
                .backlog
                .iter()
                .any(|&h| sockets.get::<tcp::Socket>(h).state() != tcp::State::Listen);
            if any_ready { EPOLLIN } else { 0 }
        }
        SockRole::Stream => {
            let Some(h) = obj.handle else {
                return EPOLLHUP;
            };
            let s = sockets.get::<tcp::Socket>(h);
            let mut ev = 0;
            if s.can_recv() || peer_closed(s.state()) {
                ev |= EPOLLIN;
            }
            if s.can_send() {
                ev |= EPOLLOUT;
            }
            if !s.is_active() {
                ev |= EPOLLHUP;
            }
            ev
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::stack;
    use smoltcp::wire::IpAddress;

    /// Drive a real listen+connect handshake to completion over a loopback
    /// device and return the socket set plus both handles. The listener
    /// handle becomes the "accepted" connection in place (smoltcp has no
    /// separate accept() socket — see the module doc comment).
    fn established_pair() -> (SocketSet<'static>, SocketHandle, SocketHandle) {
        let (mut iface, mut device, mut sockets) = stack::build_loopback();
        let ep = IpListenEndpoint::from((IpAddress::v4(127, 0, 0, 1), 4242u16));

        let mut listener = new_tcp();
        listener.listen(ep).unwrap();
        let lh = sockets.add(listener);

        let mut client = new_tcp();
        client
            .connect(
                iface.context(),
                (IpAddress::v4(127, 0, 0, 1), 4242u16),
                50000u16,
            )
            .unwrap();
        let ch = sockets.add(client);

        // Fixed iteration count, not "loop until PollResult::None": an ARP
        // miss is discovered and its request transmitted from inside
        // `poll_egress`, so that first `poll()` call still reports `None`
        // even though a frame is now queued for the next call to ingest
        // (see `NetState::poll_and_wake`'s doc comment for the full chain).
        for _ in 0..32 {
            iface.poll(stack::now(), &mut device, &mut sockets);
        }

        assert_eq!(
            sockets.get::<tcp::Socket>(lh).state(),
            tcp::State::Established
        );
        assert_eq!(
            sockets.get::<tcp::Socket>(ch).state(),
            tcp::State::Established
        );
        (sockets, lh, ch)
    }

    fn stream_obj(handle: SocketHandle) -> SocketObject {
        SocketObject {
            domain: AF_INET,
            typ: SOCK_STREAM,
            nonblocking: true,
            role: SockRole::Stream,
            handle: Some(handle),
            backlog: Vec::new(),
            local: None,
            listen_ep: None,
        }
    }

    #[test]
    fn established_is_writable_and_not_readable() {
        let (sockets, _lh, ch) = established_pair();
        let ev = readiness(&stream_obj(ch), &sockets);
        assert_ne!(ev & EPOLLOUT, 0, "empty tx buffer -> writable");
        assert_eq!(
            ev & EPOLLIN,
            0,
            "empty rx buffer, peer open -> not readable"
        );
        assert_eq!(ev & EPOLLHUP, 0, "is_active -> no EPOLLHUP");
    }

    #[test]
    fn unopened_socket_reports_epollin_and_epollhup() {
        // A freshly constructed, never-listened/connected socket starts in
        // `State::Closed` — the same terminal state a fully torn-down
        // connection reaches. `readiness()` treats both identically:
        // EOF-readable and hung up.
        let mut sockets = SocketSet::new(Vec::new());
        let h = sockets.add(new_tcp());
        let ev = readiness(&stream_obj(h), &sockets);
        assert_ne!(ev & EPOLLIN, 0, "Closed -> readable (observes EOF)");
        assert_ne!(ev & EPOLLHUP, 0, "!is_active -> EPOLLHUP");
    }

    #[test]
    fn listener_pool_pure_listen_is_not_ready() {
        let mut sockets = SocketSet::new(Vec::new());
        let ep = IpListenEndpoint::from((IpAddress::v4(127, 0, 0, 1), 3456u16));
        let mut s = new_tcp();
        s.listen(ep).unwrap();
        let h = sockets.add(s);
        let obj = SocketObject {
            domain: AF_INET,
            typ: SOCK_STREAM,
            nonblocking: true,
            role: SockRole::Listener,
            handle: None,
            backlog: alloc::vec![h],
            local: Some(IpEndpoint::new(IpAddress::v4(127, 0, 0, 1), 3456)),
            listen_ep: Some(ep),
        };
        assert_eq!(readiness(&obj, &sockets), 0);
    }

    #[test]
    fn listener_pool_with_established_member_is_ready() {
        let (sockets, lh, _ch) = established_pair();
        let obj = SocketObject {
            domain: AF_INET,
            typ: SOCK_STREAM,
            nonblocking: true,
            role: SockRole::Listener,
            handle: None,
            backlog: alloc::vec![lh],
            local: None,
            listen_ep: None,
        };
        assert_ne!(readiness(&obj, &sockets) & EPOLLIN, 0);
    }
}
