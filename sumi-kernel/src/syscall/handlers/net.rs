//! Socket syscalls (Phase 1: TCP/IPv4 over the in-guest loopback stack).
//!
//! See `docs/networking-design.md` and `net::mod`'s doc comment for the
//! locking/blocking discipline: a handler resolves `fd -> socket id` under
//! `FD_TABLE`, drops it, then does smoltcp work under `NET`
//! (`crate::net::lock()`). Blocking ops go through `net::net_wait`, which
//! mirrors `sched::futex`'s block/wake protocol.

use alloc::vec::Vec;

use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp;
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint};

use crate::fs::{FdKind, FileDescriptor};
use crate::net::{self, Wait, net_wait, socket};
use crate::syscall::errno::*;
use crate::syscall::{SyscallArgs, SyscallResult};

/// Resolve a socket fd to its `net::NetState::socktab` id. Runs entirely
/// under `FD_TABLE`, which is dropped before the caller touches `NET`
/// (Invariant N1: never hold both locks at once).
fn socket_fd(fd: i64) -> Result<usize, SyscallResult> {
    if fd < 0 {
        return Err(EBADF);
    }
    let table = crate::FD_TABLE.lock();
    match table.get(fd as usize) {
        Some(d) => match d.kind {
            FdKind::Socket { id } => Ok(id),
            _ => Err(ENOTSOCK),
        },
        None => Err(EBADF),
    }
}

/// Read an iovec entry (base, len) from user memory. Mirrors
/// `handlers::io::read_iovec` (kept as a small local duplicate rather than
/// exporting that one — the net syscalls are the only other consumer).
fn read_iovec(iov_ptr: usize, i: usize) -> (u64, u64) {
    // SAFETY: sumi's single-address-space model maps every user pointer
    // into kernel-visible memory; the caller supplies a valid iovec array
    // per the syscall ABI.
    unsafe {
        let base = core::ptr::read_volatile((iov_ptr + i * 16) as *const u64);
        let len = core::ptr::read_volatile((iov_ptr + i * 16 + 8) as *const u64);
        (base, len)
    }
}

/// Read `msg_iov`/`msg_iovlen` out of a `struct msghdr` (offsets 16 and 24
/// in the x86_64 Linux ABI: `msg_name`(8) + `msg_namelen`(4, padded to 8) +
/// `msg_iov`(8) + `msg_iovlen`(8) + ...).
fn read_msghdr_iov(msghdr_ptr: u64) -> (usize, usize) {
    // SAFETY: single-address-space model; caller passes a valid `msghdr`
    // per the syscall ABI.
    unsafe {
        let iov = core::ptr::read((msghdr_ptr + 16) as *const u64);
        let iovlen = core::ptr::read((msghdr_ptr + 24) as *const u64);
        (iov as usize, iovlen as usize)
    }
}

pub fn sys_socket(args: &SyscallArgs) -> SyscallResult {
    let domain = args.arg0 as u16;
    let typ_raw = args.arg1 as u32;
    let typ = (typ_raw & 0xFF) as u16;

    if domain != socket::AF_UNIX && domain != socket::AF_INET && domain != socket::AF_INET6 {
        return EAFNOSUPPORT;
    }
    if typ != socket::SOCK_STREAM {
        return EPROTONOSUPPORT;
    }
    let nonblocking = typ_raw & socket::SOCK_NONBLOCK != 0;

    let id = net::lock().socket_alloc(socket::SocketObject {
        domain,
        typ,
        nonblocking,
        role: socket::SockRole::Stream,
        handle: None,
        backlog: Vec::new(),
        local: None,
        listen_ep: None,
    });

    let flags = if nonblocking {
        socket::SOCK_NONBLOCK
    } else {
        0
    };
    crate::FD_TABLE.lock().alloc(FileDescriptor {
        kind: FdKind::Socket { id },
        flags,
    }) as SyscallResult
}

pub fn sys_bind(args: &SyscallArgs) -> SyscallResult {
    let id = match socket_fd(args.arg0 as i64) {
        Ok(id) => id,
        Err(e) => return e,
    };
    {
        let g = net::lock();
        let Some(obj) = g.socket_get(id) else {
            return EBADF;
        };
        if obj.domain == socket::AF_UNIX {
            return 0;
        }
    }
    let ep = match net::parse_sockaddr(args.arg1, args.arg2 as u32) {
        Ok(ep) => ep,
        Err(e) => return e,
    };

    let mut g = net::lock();
    match g.socket_get_mut(id) {
        Some(obj) => {
            obj.local = Some(ep);
            0
        }
        None => EBADF,
    }
}

pub fn sys_listen(args: &SyscallArgs) -> SyscallResult {
    let id = match socket_fd(args.arg0 as i64) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let backlog = (args.arg1 as usize).clamp(1, 8);

    let mut g = net::lock();
    let Some(obj) = g.socket_get(id) else {
        return EBADF;
    };
    if obj.domain == socket::AF_UNIX {
        let obj = g.socket_get_mut(id).expect("id validated above");
        obj.role = socket::SockRole::Listener;
        g.poll_and_wake();
        return 0;
    }
    let Some(local) = obj.local else {
        return EINVAL; // must bind() before listen()
    };
    let listen_ep: IpListenEndpoint = local.into();

    let mut handles: Vec<SocketHandle> = Vec::with_capacity(backlog);
    for _ in 0..backlog {
        let mut s = socket::new_tcp();
        if let Err(e) = s.listen(listen_ep) {
            for h in handles.drain(..) {
                g.sockets.remove(h);
            }
            return match e {
                tcp::ListenError::Unaddressable => EINVAL,
                tcp::ListenError::InvalidState => EINVAL,
            };
        }
        handles.push(g.sockets.add(s));
    }

    let obj = g.socket_get_mut(id).expect("id validated above");
    obj.role = socket::SockRole::Listener;
    obj.listen_ep = Some(listen_ep);
    obj.backlog = handles;
    g.poll_and_wake();
    0
}

pub fn sys_accept4(args: &SyscallArgs) -> SyscallResult {
    let listener_id = match socket_fd(args.arg0 as i64) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let addr_ptr = args.arg1;
    let addrlen_ptr = args.arg2;
    let uflags = args.arg3 as u32;

    let mut accepted: Option<(SocketHandle, Option<IpEndpoint>)> = None;

    let check = |g: &mut net::NetState| -> Wait {
        let Some(obj) = g.socket_get(listener_id) else {
            return Wait::Ready(EBADF);
        };
        if !matches!(obj.role, socket::SockRole::Listener) {
            return Wait::Ready(EINVAL);
        }
        let pos = obj
            .backlog
            .iter()
            .position(|&h| g.sockets.get::<tcp::Socket>(h).state() != tcp::State::Listen);
        let Some(i) = pos else {
            return if obj.nonblocking || uflags & socket::SOCK_NONBLOCK != 0 {
                Wait::Ready(EAGAIN)
            } else {
                Wait::Block
            };
        };

        // Hand out the backlog member that left Listen; refill the pool
        // with a fresh listener exactly like it (smoltcp has no accept()
        // — see net::socket's module doc comment).
        let listen_ep = obj.listen_ep.expect("listener always has listen_ep");
        let obj = g.socket_get_mut(listener_id).expect("checked above");
        let handle = obj.backlog.remove(i);

        let mut refill = socket::new_tcp();
        // `listen_ep` was already validated by the original listen() call,
        // so this cannot fail.
        let _ = refill.listen(listen_ep);
        let refill_handle = g.sockets.add(refill);
        g.socket_get_mut(listener_id)
            .expect("checked above")
            .backlog
            .push(refill_handle);

        let remote = g.sockets.get::<tcp::Socket>(handle).remote_endpoint();
        accepted = Some((handle, remote));
        Wait::Ready(0)
    };

    let r = net_wait(None, EAGAIN, check);
    if r < 0 {
        return r;
    }
    let Some((handle, remote)) = accepted else {
        // Ready(0) is only ever returned alongside `accepted` being set.
        return EAGAIN;
    };

    let nonblocking = uflags & socket::SOCK_NONBLOCK != 0;
    let new_id = net::lock().socket_alloc(socket::SocketObject {
        domain: socket::AF_INET,
        typ: socket::SOCK_STREAM,
        nonblocking,
        role: socket::SockRole::Stream,
        handle: Some(handle),
        backlog: Vec::new(),
        local: None,
        listen_ep: None,
    });

    if addr_ptr != 0
        && let Some(ep) = remote
        && let Err(e) = net::write_sockaddr(addr_ptr, addrlen_ptr, ep)
    {
        return e;
    }

    let fd_flags = if nonblocking {
        socket::SOCK_NONBLOCK
    } else {
        0
    };
    crate::FD_TABLE.lock().alloc(FileDescriptor {
        kind: FdKind::Socket { id: new_id },
        flags: fd_flags,
    }) as SyscallResult
}

/// `accept(fd, addr, addrlen)` == `accept4(fd, addr, addrlen, 0)`.
pub fn sys_accept(args: &SyscallArgs) -> SyscallResult {
    let accept4_args = SyscallArgs {
        nr: 288,
        arg0: args.arg0,
        arg1: args.arg1,
        arg2: args.arg2,
        arg3: 0,
        arg4: 0,
        arg5: 0,
        caller_rip: args.caller_rip,
        caller_rflags: args.caller_rflags,
    };
    sys_accept4(&accept4_args)
}

pub fn sys_connect(args: &SyscallArgs) -> SyscallResult {
    let id = match socket_fd(args.arg0 as i64) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let ep = match net::parse_sockaddr(args.arg1, args.arg2 as u32) {
        Ok(ep) => ep,
        Err(e) => return e,
    };

    let nonblocking = {
        let mut g = net::lock();
        let Some(obj) = g.socket_get(id) else {
            return EBADF;
        };
        if obj.handle.is_some() {
            return EISCONN;
        }
        let nonblocking = obj.nonblocking;
        let lport = g.alloc_ephemeral();
        let mut s = socket::new_tcp();
        match s.connect(g.iface.context(), ep, lport) {
            Ok(()) => {
                let h = g.sockets.add(s);
                g.socket_get_mut(id).expect("checked above").handle = Some(h);
                g.poll_and_wake();
            }
            Err(tcp::ConnectError::Unaddressable) => return EINVAL,
            Err(tcp::ConnectError::InvalidState) => return EISCONN,
        }
        nonblocking
    };

    if nonblocking {
        return EINPROGRESS;
    }

    net_wait(None, 0, |g| {
        let Some(obj) = g.socket_get(id) else {
            return Wait::Ready(EBADF);
        };
        let Some(h) = obj.handle else {
            return Wait::Ready(ENOTCONN);
        };
        let s = g.sockets.get::<tcp::Socket>(h);
        if s.may_send() {
            Wait::Ready(0)
        } else if !s.is_active() {
            Wait::Ready(ECONNREFUSED)
        } else {
            Wait::Block
        }
    })
}

/// Send `data` on socket `id`, honoring `nonblocking`. Returns the number
/// of bytes sent, or a negative errno. Shared by `sys_sendto` and each
/// iovec chunk of `sys_sendmsg`.
fn send_bytes(id: usize, data: &[u8], nonblocking: bool) -> i64 {
    if data.is_empty() {
        return 0;
    }
    let attempt = |g: &mut net::NetState| -> Wait {
        let Some(obj) = g.socket_get(id) else {
            return Wait::Ready(EBADF);
        };
        let Some(h) = obj.handle else {
            return Wait::Ready(ENOTCONN);
        };
        let s = g.sockets.get_mut::<tcp::Socket>(h);
        match s.send_slice(data) {
            Ok(0) => Wait::Block,
            Ok(n) => Wait::Ready(n as i64),
            Err(tcp::SendError::InvalidState) => Wait::Ready(EPIPE),
        }
    };

    let mut g = net::lock();
    match attempt(&mut g) {
        Wait::Ready(v) => {
            if v >= 0 {
                g.poll_and_wake();
            }
            drop(g);
            v
        }
        Wait::Block => {
            drop(g);
            if nonblocking {
                EAGAIN
            } else {
                net_wait(None, 0, attempt)
            }
        }
    }
}

/// Receive into `buf` from socket `id`, honoring `nonblocking`. Returns the
/// number of bytes read (0 on EOF), or a negative errno.
fn recv_bytes(id: usize, buf: &mut [u8], nonblocking: bool) -> i64 {
    if buf.is_empty() {
        return 0;
    }
    let mut attempt = |g: &mut net::NetState| -> Wait {
        g.poll_and_wake();
        let Some(obj) = g.socket_get(id) else {
            return Wait::Ready(EBADF);
        };
        let Some(h) = obj.handle else {
            return Wait::Ready(ENOTCONN);
        };
        let s = g.sockets.get_mut::<tcp::Socket>(h);
        match s.recv_slice(buf) {
            Ok(0) => {
                if socket::peer_closed(s.state()) {
                    Wait::Ready(0)
                } else {
                    Wait::Block
                }
            }
            Ok(n) => Wait::Ready(n as i64),
            Err(tcp::RecvError::Finished) => Wait::Ready(0),
            Err(tcp::RecvError::InvalidState) => Wait::Ready(ENOTCONN),
        }
    };

    if nonblocking {
        let mut g = net::lock();
        match attempt(&mut g) {
            Wait::Ready(v) => v,
            Wait::Block => EAGAIN,
        }
    } else {
        net_wait(None, 0, attempt)
    }
}

/// `dest_addr`/`addrlen` (args 4/5) are ignored: Phase 1 only supports
/// already-connected TCP sockets, matching the acceptance test's usage.
pub fn sys_sendto(args: &SyscallArgs) -> SyscallResult {
    let id = match socket_fd(args.arg0 as i64) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let buf_ptr = args.arg1;
    let len = (args.arg2 as usize).min(u32::MAX as usize);
    if len > 0 && buf_ptr == 0 {
        return EFAULT;
    }
    let nonblocking = match net::lock().socket_get(id) {
        Some(obj) => obj.nonblocking,
        None => return EBADF,
    };
    if len == 0 {
        return 0;
    }
    // SAFETY: single-address-space model; the syscall ABI guarantees
    // buf_ptr..+len is a valid, readable user buffer.
    let data = unsafe { core::slice::from_raw_parts(buf_ptr as *const u8, len) };
    send_bytes(id, data, nonblocking)
}

/// `src_addr`/`addrlen` (args 4/5) are ignored, same as `sys_sendto`.
pub fn sys_recvfrom(args: &SyscallArgs) -> SyscallResult {
    let id = match socket_fd(args.arg0 as i64) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let buf_ptr = args.arg1;
    let len = (args.arg2 as usize).min(u32::MAX as usize);
    if len > 0 && buf_ptr == 0 {
        return EFAULT;
    }
    let nonblocking = match net::lock().socket_get(id) {
        Some(obj) => obj.nonblocking,
        None => return EBADF,
    };
    if len == 0 {
        return 0;
    }
    // SAFETY: single-address-space model; the syscall ABI guarantees
    // buf_ptr..+len is a valid, writable user buffer.
    let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, len) };
    recv_bytes(id, buf, nonblocking)
}

pub fn sys_sendmsg(args: &SyscallArgs) -> SyscallResult {
    let id = match socket_fd(args.arg0 as i64) {
        Ok(id) => id,
        Err(e) => return e,
    };
    if args.arg1 == 0 {
        return EFAULT;
    }
    let nonblocking = match net::lock().socket_get(id) {
        Some(obj) => obj.nonblocking,
        None => return EBADF,
    };
    let (iov_ptr, iovcnt) = read_msghdr_iov(args.arg1);

    let mut total: i64 = 0;
    for i in 0..iovcnt {
        let (base, len) = read_iovec(iov_ptr, i);
        if len == 0 {
            continue;
        }
        // SAFETY: single-address-space model; iovec entries are valid per
        // the syscall ABI.
        let data = unsafe { core::slice::from_raw_parts(base as *const u8, len as usize) };
        // Only the first chunk may legitimately block; once something has
        // been sent, treat further backpressure as the end of this gather.
        let n = send_bytes(id, data, nonblocking || total > 0);
        if n < 0 {
            return if total > 0 { total } else { n };
        }
        total += n;
        if (n as usize) < data.len() {
            break;
        }
    }
    total
}

pub fn sys_recvmsg(args: &SyscallArgs) -> SyscallResult {
    let id = match socket_fd(args.arg0 as i64) {
        Ok(id) => id,
        Err(e) => return e,
    };
    if args.arg1 == 0 {
        return EFAULT;
    }
    let nonblocking = match net::lock().socket_get(id) {
        Some(obj) => obj.nonblocking,
        None => return EBADF,
    };
    let (iov_ptr, iovcnt) = read_msghdr_iov(args.arg1);

    let mut total: i64 = 0;
    for i in 0..iovcnt {
        let (base, len) = read_iovec(iov_ptr, i);
        if len == 0 {
            continue;
        }
        // SAFETY: single-address-space model; iovec entries are valid per
        // the syscall ABI.
        let buf = unsafe { core::slice::from_raw_parts_mut(base as *mut u8, len as usize) };
        let n = recv_bytes(id, buf, nonblocking || total > 0);
        if n < 0 {
            return if total > 0 { total } else { n };
        }
        if n == 0 {
            break;
        }
        total += n;
        if (n as usize) < buf.len() {
            break;
        }
    }
    total
}

pub fn sys_shutdown(args: &SyscallArgs) -> SyscallResult {
    let id = match socket_fd(args.arg0 as i64) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let mut g = net::lock();
    let Some(obj) = g.socket_get(id) else {
        return EBADF;
    };
    let Some(h) = obj.handle else {
        return ENOTCONN;
    };
    // SHUT_RD has no smoltcp equivalent; Phase 1 only closes the send half
    // (covers SHUT_WR and SHUT_RDWR, which is all mysqld/the test need).
    g.sockets.get_mut::<tcp::Socket>(h).close();
    g.poll_and_wake();
    0
}

pub fn sys_getsockname(args: &SyscallArgs) -> SyscallResult {
    let id = match socket_fd(args.arg0 as i64) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let g = net::lock();
    let Some(obj) = g.socket_get(id) else {
        return EBADF;
    };
    if obj.domain == socket::AF_UNIX {
        let addr_ptr = args.arg1;
        let addrlen_ptr = args.arg2;
        if addr_ptr != 0 && addrlen_ptr != 0 {
            let cap = unsafe { core::ptr::read_unaligned(addrlen_ptr as *const u32) };
            if cap >= 2 {
                unsafe {
                    core::ptr::write_unaligned(addr_ptr as *mut u16, socket::AF_UNIX);
                    core::ptr::write_unaligned(addrlen_ptr as *mut u32, 2);
                }
            }
        }
        return 0;
    }
    let ep = match obj.handle {
        Some(h) => g.sockets.get::<tcp::Socket>(h).local_endpoint(),
        None => obj.local,
    }
    .unwrap_or(IpEndpoint::new(IpAddress::v4(0, 0, 0, 0), 0));
    drop(g);
    match net::write_sockaddr(args.arg1, args.arg2, ep) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

pub fn sys_getpeername(args: &SyscallArgs) -> SyscallResult {
    let id = match socket_fd(args.arg0 as i64) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let g = net::lock();
    let Some(obj) = g.socket_get(id) else {
        return EBADF;
    };
    let Some(h) = obj.handle else {
        return ENOTCONN;
    };
    let Some(ep) = g.sockets.get::<tcp::Socket>(h).remote_endpoint() else {
        return ENOTCONN;
    };
    drop(g);
    match net::write_sockaddr(args.arg1, args.arg2, ep) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

/// Phase 1 accepts every option as a best-effort no-op (`SO_REUSEADDR`,
/// `TCP_NODELAY`, ...) — none of them change behavior over the loopback
/// stack. Only the fd itself is validated.
pub fn sys_setsockopt(args: &SyscallArgs) -> SyscallResult {
    match socket_fd(args.arg0 as i64) {
        Ok(_) => 0,
        Err(e) => e,
    }
}

/// Every option (including `SOL_SOCKET`/`SO_ERROR`) reports success with a
/// zeroed best-effort value in Phase 1 — `SO_ERROR == 0` is what matters
/// (it is how a non-blocking `connect()` completion is normally checked).
pub fn sys_getsockopt(args: &SyscallArgs) -> SyscallResult {
    if let Err(e) = socket_fd(args.arg0 as i64) {
        return e;
    }
    let val_ptr = args.arg3;
    let len_ptr = args.arg4;

    if val_ptr == 0 || len_ptr == 0 {
        return 0;
    }
    // SAFETY: single-address-space model; `len_ptr` is a valid user
    // `socklen_t*` per the syscall ABI.
    let cap = unsafe { core::ptr::read_unaligned(len_ptr as *const u32) };
    if cap < 4 {
        return 0;
    }
    // SAFETY: single-address-space model; `val_ptr`/`len_ptr` point at a
    // valid, writable `int`/`socklen_t` per the syscall ABI, `cap` checked.
    unsafe {
        core::ptr::write_unaligned(val_ptr as *mut i32, 0);
        core::ptr::write_unaligned(len_ptr as *mut u32, 4);
    }
    0
}

/// `read(fd)` on a socket: byte-stream receive, used by `sys_read`/`sys_readv`
/// (Rust std and glibc use plain read/write on connected sockets). Semantics
/// match `sys_recvfrom` with no address out-params.
pub(crate) fn sock_read(id: usize, buf_ptr: u64, count: usize) -> i64 {
    if count > 0 && buf_ptr == 0 {
        return EFAULT;
    }
    let nonblocking = match net::lock().socket_get(id) {
        Some(obj) => obj.nonblocking,
        None => return EBADF,
    };
    if count == 0 {
        return 0;
    }
    // SAFETY: single-address-space model; the syscall ABI guarantees
    // buf_ptr..+count is a valid, writable user buffer.
    let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, count) };
    recv_bytes(id, buf, nonblocking)
}

/// `write(fd)` on a socket: byte-stream send, used by `sys_write`/`sys_writev`.
/// Semantics match `sys_sendto` with a NULL destination.
pub(crate) fn sock_write(id: usize, buf_ptr: u64, count: usize) -> i64 {
    if count > 0 && buf_ptr == 0 {
        return EFAULT;
    }
    let nonblocking = match net::lock().socket_get(id) {
        Some(obj) => obj.nonblocking,
        None => return EBADF,
    };
    if count == 0 {
        return 0;
    }
    // SAFETY: single-address-space model; the syscall ABI guarantees
    // buf_ptr..+count is a valid, readable user buffer.
    let data = unsafe { core::slice::from_raw_parts(buf_ptr as *const u8, count) };
    send_bytes(id, data, nonblocking)
}
