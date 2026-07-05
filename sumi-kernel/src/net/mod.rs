//! In-guest TCP/IP networking (smoltcp).
//!
//! See `docs/networking-design.md` for the full design. This module owns a
//! single global smoltcp `Interface` + `SocketSet` + `Device` behind one
//! lock (`NetState`), and exposes a plain `poll()` that advances the stack
//! and wakes blocked threads whose sockets became ready.
//!
//! `NetStateInner<D>` is generic over the device: production runs over
//! `device::VirtioNetDevice` (`NetState`, Phase 2, real guest<->host
//! networking through the host userspace gateway), while host unit tests
//! run over smoltcp's own `Loopback` (`TestNetState`) to validate the
//! socket + epoll syscall layer without KVM. `docs/networking-design.md` R2
//! covers the ARP-to-self loopback handshake the test device implies.
//!
//! ## Locking (see the plan's "blocking / wakeup contract")
//! Three locks exist: `FD_TABLE`, `NET` (this module's global), and the
//! scheduler runqueue. A handler holds either `FD_TABLE` or `NET`, never
//! both — fd-to-socket-id translation happens under `FD_TABLE`, which is
//! dropped before any smoltcp work runs under `NET`. The only nesting is
//! `NET -> runqueue`, inside `poll_and_wake` -> `wake_blocked`; `schedule()`
//! is never called while `NET` is held (see `wait::net_wait`).
//!
//! `NET` needs no IRQ-disable: it is only ever locked with IF=0 (syscall
//! handlers clear IF via SFMASK; `timer_interrupt` is an interrupt gate),
//! and the timer never fires from inside a kernel critical section that
//! could be holding `NET` — the same reasoning `sched::futex` documents for
//! its bucket locks.

mod abi;
pub mod device;
pub mod epoll;
pub mod pipe;
pub mod socket;
pub mod stack;
pub mod wait;

use alloc::vec::Vec;

use smoltcp::iface::{Interface, PollResult, SocketSet};
use smoltcp::phy::Device;

pub use abi::{EpollEvent, parse_sockaddr, read_epoll_event, write_epoll_event, write_sockaddr};
pub use epoll::EpollInstance;
pub use pipe::{close_pipe, pipe_create, pipe_readiness};
pub(crate) use pipe::{pipe_read, pipe_write};
pub use socket::SocketObject;
pub use stack::now;
pub use wait::{Wait, net_wait};

use device::{PollDevice, VirtioNetDevice};

/// Device poll iterations per `poll_and_wake` call. A device only enqueues
/// or drains one frame per `iface.poll()` call, so a full TCP handshake
/// (SYN, SYN-ACK, ACK, ...) needs several calls; looping a fixed count
/// drives it to completion within one lock acquisition (see
/// docs/networking-design.md R3).
const NET_POLL_ITERS: usize = 5;

/// The global net state, generic over the device backing the smoltcp
/// `Interface`. Production uses `VirtioNetDevice` (see the `NetState`
/// alias below); host unit tests use smoltcp's own `Loopback` (see
/// `TestNetState`) — the refactor from a hardcoded `Loopback` field to this
/// generic struct must be behavior-preserving, which is exactly what the
/// Phase 1 loopback unit tests (unchanged) prove.
pub struct NetStateInner<D> {
    pub iface: Interface,
    pub sockets: SocketSet<'static>,
    pub device: D,
    pub socktab: Vec<Option<SocketObject>>,
    pub epolltab: Vec<Option<EpollInstance>>,
    /// In-kernel pipes (`pipe`/`pipe2`), indexed the same way as `socktab`.
    /// Sharing this struct's lock and `waiters` queue with sockets is what
    /// lets pipes reuse `net_wait`/`poll_and_wake` for blocking I/O and
    /// epoll readiness — see `net::pipe`'s module doc comment.
    pub pipetab: Vec<Option<pipe::PipeState>>,
    pub waiters: wait::WaitQueue,
    pub deadline_waiters: usize,
    pub next_ephemeral: u16,
    /// Handles whose fd was closed while the TCP conversation was still
    /// draining: `close(2)` initiates a graceful FIN (`socket.close()`)
    /// and parks the handle here; `poll_and_wake` reaps it once the socket
    /// reaches `Closed`. Removing a live handle from the `SocketSet`
    /// outright would vanish the endpoint mid-conversation — the peer's
    /// next segment would be answered with RST instead of a FIN handshake,
    /// turning a clean EOF into ECONNRESET/ENOTCONN on the other side.
    pub closing: Vec<smoltcp::iface::SocketHandle>,
}

/// Production net state: virtio-net backed.
pub type NetState = NetStateInner<VirtioNetDevice>;

/// Host-test net state: smoltcp's in-guest `Loopback` device, no virtio.
#[cfg(test)]
pub type TestNetState = NetStateInner<smoltcp::phy::Loopback>;

/// Global net state, published once by `init()`. A plain `spin::Mutex` is
/// correct without IRQ-disable — see the module doc comment.
static NET: spin::Once<spin::Mutex<NetState>> = spin::Once::new();

/// Probe and build the Phase 2 virtio-net stack and publish it. Must run
/// after `exec::read_boot_info()` (so `RNG_SEED` is available for the TCP
/// sequence-number seed) and before `KERNEL_READY` is published. A no-op
/// (with a log line) if virtio-net is absent — sockets syscalls then see
/// `NET` uninitialized, same as if `init()` were never called.
pub fn init() {
    match NetState::new() {
        Some(st) => {
            NET.call_once(|| spin::Mutex::new(st));
        }
        None => crate::kprintln!("[net] virtio-net absent"),
    }
}

/// Timer-tick hook: advance the stack and wake anyone whose socket became
/// ready. A no-op before `init()` has run.
pub fn poll() {
    if let Some(m) = NET.get() {
        if let Some(mut g) = m.try_lock() {
            g.poll_and_wake();
        }
    }
}

/// Lock the global net state. Every socket/epoll syscall handler and
/// `wait::net_wait` go through this.
pub(crate) fn lock() -> spin::MutexGuard<'static, NetState> {
    NET.get().expect("net not initialized").lock()
}

/// Close a socket fd: drop its `SocketObject`, initiating a graceful FIN
/// for a live stream handle (see `NetStateInner::closing`) and aborting
/// (RST) any un-accepted backlog connections, exactly like Linux does when
/// a listener closes.
pub fn close_socket(id: usize) {
    let mut g = lock();
    g.socket_close(id);
    // Push the FIN/RST segments out immediately rather than waiting for
    // the next timer tick.
    g.poll_and_wake();
}

/// Close an epoll fd: drop its `EpollInstance`.
pub fn close_epoll(id: usize) {
    lock().epoll_free(id);
}

impl NetState {
    /// Probe the virtio-net device and build the Phase 2 stack around it.
    /// `None` if the device is absent — `net::init()` then leaves `NET`
    /// unpublished.
    fn new() -> Option<Self> {
        let mut device = VirtioNetDevice::init(&crate::KERNEL_ALLOCATOR)?;
        let (iface, sockets) = stack::build_virtio(&mut device);
        Some(Self {
            iface,
            sockets,
            device,
            socktab: Vec::new(),
            epolltab: Vec::new(),
            pipetab: Vec::new(),
            waiters: wait::WaitQueue::new(),
            deadline_waiters: 0,
            next_ephemeral: stack::EPHEMERAL_LO,
            closing: Vec::new(),
        })
    }
}

#[cfg(test)]
impl TestNetState {
    /// Host-test constructor: builds the Phase 1 loopback stack directly
    /// (no scheduler, no boot sequence, no virtio probing).
    pub fn new_loopback() -> Self {
        let (iface, device, sockets) = stack::build_loopback();
        Self {
            iface,
            sockets,
            device,
            socktab: Vec::new(),
            epolltab: Vec::new(),
            pipetab: Vec::new(),
            waiters: wait::WaitQueue::new(),
            deadline_waiters: 0,
            next_ephemeral: stack::EPHEMERAL_LO,
            closing: Vec::new(),
        }
    }
}

impl<D> NetStateInner<D> {
    /// Allocate the lowest free socket id, matching `FdTable::alloc`.
    pub fn socket_alloc(&mut self, o: SocketObject) -> usize {
        for (i, slot) in self.socktab.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(o);
                return i;
            }
        }
        self.socktab.push(Some(o));
        self.socktab.len() - 1
    }

    pub fn socket_get(&self, id: usize) -> Option<&SocketObject> {
        self.socktab.get(id)?.as_ref()
    }

    pub fn socket_get_mut(&mut self, id: usize) -> Option<&mut SocketObject> {
        self.socktab.get_mut(id)?.as_mut()
    }

    /// Close a socket id gracefully: a live stream handle gets a FIN
    /// (`close()`) and is parked on `self.closing` until the conversation
    /// drains (reaped by `poll_and_wake`); un-accepted backlog connections
    /// are aborted (RST), matching Linux listener-close semantics. Handles
    /// already fully closed are removed immediately.
    pub fn socket_close(&mut self, id: usize) -> Option<SocketObject> {
        let obj = self.socktab.get_mut(id)?.take()?;
        if let Some(h) = obj.handle {
            let s = self.sockets.get_mut::<smoltcp::socket::tcp::Socket>(h);
            if s.is_active() {
                s.close();
                self.closing.push(h);
            } else {
                self.sockets.remove(h);
            }
        }
        for &h in &obj.backlog {
            let s = self.sockets.get_mut::<smoltcp::socket::tcp::Socket>(h);
            if s.is_active() {
                // Un-accepted connection: reset it (Linux does the same);
                // the RST goes out on the caller's follow-up poll, after
                // which the socket is Closed and the reaper removes it.
                s.abort();
                self.closing.push(h);
            } else {
                self.sockets.remove(h);
            }
        }
        Some(obj)
    }

    /// Free a socket id, removing its smoltcp handle(s) (stream handle
    /// and/or listener backlog pool) from the socket set.
    pub fn socket_free(&mut self, id: usize) -> Option<SocketObject> {
        let obj = self.socktab.get_mut(id)?.take()?;
        if let Some(h) = obj.handle {
            self.sockets.remove(h);
        }
        for &h in &obj.backlog {
            self.sockets.remove(h);
        }
        Some(obj)
    }

    /// Allocate the lowest free epoll id.
    pub fn epoll_alloc(&mut self, e: EpollInstance) -> usize {
        for (i, slot) in self.epolltab.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(e);
                return i;
            }
        }
        self.epolltab.push(Some(e));
        self.epolltab.len() - 1
    }

    pub fn epoll_get(&self, id: usize) -> Option<&EpollInstance> {
        self.epolltab.get(id)?.as_ref()
    }

    pub fn epoll_get_mut(&mut self, id: usize) -> Option<&mut EpollInstance> {
        self.epolltab.get_mut(id)?.as_mut()
    }

    pub fn epoll_free(&mut self, id: usize) -> Option<EpollInstance> {
        self.epolltab.get_mut(id)?.take()
    }

    /// Allocate the lowest free pipe id, matching `socket_alloc`/`epoll_alloc`.
    pub fn pipe_alloc(&mut self, p: pipe::PipeState) -> usize {
        for (i, slot) in self.pipetab.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(p);
                return i;
            }
        }
        self.pipetab.push(Some(p));
        self.pipetab.len() - 1
    }

    pub fn pipe_get(&self, id: usize) -> Option<&pipe::PipeState> {
        self.pipetab.get(id)?.as_ref()
    }

    pub fn pipe_get_mut(&mut self, id: usize) -> Option<&mut pipe::PipeState> {
        self.pipetab.get_mut(id)?.as_mut()
    }

    pub fn pipe_free(&mut self, id: usize) -> Option<pipe::PipeState> {
        self.pipetab.get_mut(id)?.take()
    }

    /// Allocate the next ephemeral port in `[EPHEMERAL_LO, EPHEMERAL_HI]`,
    /// wrapping back to `EPHEMERAL_LO`. Does not scan for in-use ports —
    /// Phase 1's short-lived test connections make a collision vanishingly
    /// unlikely; a real free-port scan is deferred to whichever later phase
    /// needs many concurrent outbound connections.
    pub fn alloc_ephemeral(&mut self) -> u16 {
        let p = self.next_ephemeral;
        self.next_ephemeral = if p == stack::EPHEMERAL_HI {
            stack::EPHEMERAL_LO
        } else {
            p + 1
        };
        p
    }
}

impl<D: Device + PollDevice> NetStateInner<D> {
    pub(crate) fn wake_waiters(&mut self) {
        self.deadline_waiters = 0;
        self.waiters.wake_all();
    }

    /// Advance the stack and wake every waiter (level-triggered: each
    /// re-checks its own readiness on resume). Called before every
    /// blocking-or-not readiness check (`wait::net_wait`'s poll-before-block
    /// step) and from the timer tick (`poll()`), so TCP timers keep
    /// advancing and `epoll_wait` timeouts fire even with no socket
    /// activity.
    ///
    /// `pub(crate)`, not private as the original interface sketch listed:
    /// syscall handlers (`syscall::handlers::net`, `::epoll`) call this
    /// directly after mutating a socket (see e.g. `sys_listen`,
    /// `sys_connect`) rather than re-locking `NET` through `net::poll()`.
    ///
    /// `device.pre_poll()` runs once per call, before the poll loop: for
    /// `VirtioNetDevice` this rings the RX queue's doorbell exactly once
    /// per `poll_and_wake` (not once per `receive()`), which is the RX
    /// pull handshake Phase 2 relies on in place of a host->guest IRQ (see
    /// `docs/networking-design.md`). `Loopback` needs nothing here.
    ///
    /// Deviation from the original plan: `Interface::poll()` runs its
    /// ingress loop (drain whatever the device already has queued) BEFORE
    /// its egress loop (let sockets send). An ARP miss is discovered and
    /// its request transmitted from *inside* the egress loop, so that
    /// request sits in the device queue without being ingested until the
    /// *next* top-level `poll()` call — and that first call still reports
    /// `PollResult::None` (dispatch of the real packet failed with a
    /// pending-neighbor error, so no socket state actually changed yet).
    /// Breaking on the first `None` therefore stops before the ARP
    /// request/reply round-trip completes. Looping a fixed `NET_POLL_ITERS`
    /// times unconditionally instead is a few no-op scans more expensive in
    /// the steady state but reliably drains a full ARP + TCP handshake.
    pub(crate) fn poll_and_wake(&mut self) -> bool {
        let now = stack::now();
        let mut changed = false;
        self.device.pre_poll();
        for _ in 0..NET_POLL_ITERS {
            if self.iface.poll(now, &mut self.device, &mut self.sockets)
                == PollResult::SocketStateChanged
            {
                changed = true;
            }
        }
        // Reap gracefully-closing handles (see `closing`) whose TCP
        // conversation has fully drained. TIME-WAIT is left to smoltcp:
        // the state only becomes `Closed` once 2MSL (or the FIN handshake)
        // completes, so removal here is always safe.
        let mut reaped = false;
        if !self.closing.is_empty() {
            let sockets = &mut self.sockets;
            self.closing.retain(|&h| {
                let done = sockets.get::<smoltcp::socket::tcp::Socket>(h).state()
                    == smoltcp::socket::tcp::State::Closed;
                if done {
                    sockets.remove(h);
                    reaped = true;
                }
                !done
            });
        }
        let should_wake = changed || reaped || self.deadline_waiters != 0;
        if should_wake {
            self.wake_waiters();
        }
        should_wake
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smoltcp::socket::tcp;
    use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint};
    use socket::{EPOLLIN, EPOLLOUT, SockRole, new_tcp, readiness};

    fn listener_obj() -> SocketObject {
        SocketObject {
            domain: socket::AF_INET,
            typ: socket::SOCK_STREAM,
            nonblocking: true,
            role: SockRole::Listener,
            handle: None,
            backlog: Vec::new(),
            local: None,
            listen_ep: None,
        }
    }

    fn stream_obj(handle: smoltcp::iface::SocketHandle) -> SocketObject {
        SocketObject {
            domain: socket::AF_INET,
            typ: socket::SOCK_STREAM,
            nonblocking: true,
            role: SockRole::Stream,
            handle: Some(handle),
            backlog: Vec::new(),
            local: None,
            listen_ep: None,
        }
    }

    #[test]
    fn alloc_ephemeral_wraps_within_range() {
        let mut st = TestNetState::new_loopback();
        st.next_ephemeral = stack::EPHEMERAL_HI;
        assert_eq!(st.alloc_ephemeral(), stack::EPHEMERAL_HI);
        assert_eq!(st.alloc_ephemeral(), stack::EPHEMERAL_LO);
        let p = st.alloc_ephemeral();
        assert!((stack::EPHEMERAL_LO..=stack::EPHEMERAL_HI).contains(&p));
    }

    #[test]
    fn socket_alloc_reuses_freed_slots() {
        let mut st = TestNetState::new_loopback();
        let id0 = st.socket_alloc(listener_obj());
        st.socket_free(id0);
        let id1 = st.socket_alloc(listener_obj());
        assert_eq!(id0, id1);
    }

    #[test]
    fn socket_free_removes_backlog_handles() {
        let mut st = TestNetState::new_loopback();
        let h = st.sockets.add(new_tcp());
        let mut obj = listener_obj();
        obj.backlog.push(h);
        let id = st.socket_alloc(obj);
        st.socket_free(id);
        assert!(st.socket_get(id).is_none());
    }

    /// End-to-end: listen, connect, accept-by-scan, bidirectional echo —
    /// the whole Phase 1 acceptance flow, driven directly at the `NetState`
    /// layer with no scheduler involved. Validates R2 (ARP-to-self loopback
    /// handshake) and R3 (`poll_and_wake` looping a handshake to
    /// completion).
    #[test]
    fn end_to_end_listen_connect_echo() {
        let mut st = TestNetState::new_loopback();
        let port = 3456u16;
        let listen_ep = IpListenEndpoint::from((IpAddress::v4(127, 0, 0, 1), port));

        // 1. Listener with a backlog of one.
        let mut listener_sock = new_tcp();
        listener_sock.listen(listen_ep).unwrap();
        let lh = st.sockets.add(listener_sock);
        let mut listener = listener_obj();
        listener.local = Some(IpEndpoint::new(IpAddress::v4(127, 0, 0, 1), port));
        listener.listen_ep = Some(listen_ep);
        listener.backlog.push(lh);
        let listener_id = st.socket_alloc(listener);

        // 2. Client connect.
        let client_port = st.alloc_ephemeral();
        let mut client_sock = new_tcp();
        client_sock
            .connect(
                st.iface.context(),
                (IpAddress::v4(127, 0, 0, 1), port),
                client_port,
            )
            .unwrap();
        let ch = st.sockets.add(client_sock);
        let client_id = st.socket_alloc(stream_obj(ch));

        // 3. Drive the handshake to completion.
        st.poll_and_wake();
        assert_eq!(
            st.sockets.get::<tcp::Socket>(lh).state(),
            tcp::State::Established
        );
        assert_eq!(
            st.sockets.get::<tcp::Socket>(ch).state(),
            tcp::State::Established
        );

        let listener_ref = st.socket_get(listener_id).unwrap();
        assert_ne!(readiness(listener_ref, &st.sockets) & EPOLLIN, 0);
        let accepted_handle = listener_ref.backlog[0];

        // 4. "accept": hand out the backlog member that left Listen, refill
        // the pool exactly as sys_accept4 will.
        let mut refill = new_tcp();
        refill.listen(listen_ep).unwrap();
        let refill_handle = st.sockets.add(refill);
        st.socket_get_mut(listener_id).unwrap().backlog = alloc::vec![refill_handle];
        let server_id = st.socket_alloc(stream_obj(accepted_handle));

        assert_ne!(
            readiness(st.socket_get(client_id).unwrap(), &st.sockets) & EPOLLOUT,
            0
        );

        // 5. Client sends, server echoes, client receives.
        const MSG: &[u8] = b"hello sumi net stack";
        let n = st
            .sockets
            .get_mut::<tcp::Socket>(ch)
            .send_slice(MSG)
            .unwrap();
        assert_eq!(n, MSG.len());
        st.poll_and_wake();

        assert_ne!(
            readiness(st.socket_get(server_id).unwrap(), &st.sockets) & EPOLLIN,
            0
        );
        let mut buf = [0u8; 64];
        let n = st
            .sockets
            .get_mut::<tcp::Socket>(accepted_handle)
            .recv_slice(&mut buf)
            .unwrap();
        assert_eq!(&buf[..n], MSG);

        let n = st
            .sockets
            .get_mut::<tcp::Socket>(accepted_handle)
            .send_slice(&buf[..n])
            .unwrap();
        assert_eq!(n, MSG.len());
        st.poll_and_wake();

        assert_ne!(
            readiness(st.socket_get(client_id).unwrap(), &st.sockets) & EPOLLIN,
            0
        );
        let mut buf2 = [0u8; 64];
        let n = st
            .sockets
            .get_mut::<tcp::Socket>(ch)
            .recv_slice(&mut buf2)
            .unwrap();
        assert_eq!(&buf2[..n], MSG);
    }
}
