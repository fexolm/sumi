// TCP + epoll loopback echo, single process, non-blocking sockets.
//
// This is the Phase 1 networking milestone (see docs/networking-design.md):
// it exercises the socket syscall family and epoll end-to-end over the
// in-guest smoltcp loopback device, with no host networking involved.
//
// Flow (all fds non-blocking, driven by one epoll loop):
//   1. listener  = socket(); bind(127.0.0.1:PORT); listen()
//   2. client    = socket(); connect(127.0.0.1:PORT)   -> EINPROGRESS
//   3. epoll ADD listener(EPOLLIN), client(EPOLLOUT)
//   4. listener readable -> accept4() -> server fd; epoll ADD server(EPOLLIN)
//   5. client writable   -> sendto(MSG); epoll MOD client -> EPOLLIN
//   6. server readable   -> recvfrom(); sendto() the same bytes back (echo)
//   7. client readable   -> recvfrom(); compare with MSG -> pass!()
//
// A pass proves: socket/bind/listen/accept4/connect/sendto/recvfrom, the
// three epoll syscalls, non-blocking semantics (EINPROGRESS/EAGAIN), and the
// scheduler readiness wakeups that drive them.

#![no_std]
#![no_main]

include!("../common.rs");

// ── net syscall numbers ────────────────────────────────────────────────────
const SYS_CONNECT: u64 = 42;
const SYS_ACCEPT4: u64 = 288;
const SYS_SENDTO: u64 = 44;
const SYS_RECVFROM: u64 = 45;
const SYS_BIND: u64 = 49;
const SYS_LISTEN: u64 = 50;
const SYS_SETSOCKOPT: u64 = 54;
const SYS_SOCKET: u64 = 41;
const SYS_EPOLL_CREATE1: u64 = 291;
const SYS_EPOLL_CTL: u64 = 233;
const SYS_EPOLL_WAIT: u64 = 232;

// ── net constants ──────────────────────────────────────────────────────────
const AF_INET: u64 = 2;
const SOCK_STREAM: u64 = 1;
const SOCK_NONBLOCK: u64 = 0o4000; // 0x800
const SOL_SOCKET: u64 = 1;
const SO_REUSEADDR: u64 = 2;

const EPOLL_CTL_ADD: u64 = 1;
const EPOLL_CTL_DEL: u64 = 2;
const EPOLL_CTL_MOD: u64 = 3;

const EPOLLIN: u32 = 0x001;
const EPOLLOUT: u32 = 0x004;

const EINPROGRESS: i64 = -115;
const EAGAIN: i64 = -11;

const PORT: u16 = 3456;

// `struct epoll_event` is packed on x86_64: 4-byte events + 8-byte data.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct EpollEvent {
    events: u32,
    data: u64,
}

// ── thin wrappers ──────────────────────────────────────────────────────────
fn sys_socket(domain: u64, ty: u64, proto: u64) -> i64 {
    unsafe { syscall3(SYS_SOCKET, domain, ty, proto) }
}

fn sys_bind(fd: i64, addr: *const u8, len: u64) -> i64 {
    unsafe { syscall3(SYS_BIND, fd as u64, addr as u64, len) }
}

fn sys_listen(fd: i64, backlog: u64) -> i64 {
    unsafe { syscall2(SYS_LISTEN, fd as u64, backlog) }
}

fn sys_connect(fd: i64, addr: *const u8, len: u64) -> i64 {
    unsafe { syscall3(SYS_CONNECT, fd as u64, addr as u64, len) }
}

fn sys_accept4(fd: i64, addr: *mut u8, len: *mut u32, flags: u64) -> i64 {
    unsafe { syscall4(SYS_ACCEPT4, fd as u64, addr as u64, len as u64, flags) }
}

fn sys_sendto(fd: i64, buf: &[u8]) -> i64 {
    unsafe {
        syscall6(
            SYS_SENDTO,
            fd as u64,
            buf.as_ptr() as u64,
            buf.len() as u64,
            0, // flags
            0, // dest_addr (NULL: connected socket)
            0, // addrlen
        )
    }
}

fn sys_recvfrom(fd: i64, buf: &mut [u8]) -> i64 {
    unsafe {
        syscall6(
            SYS_RECVFROM,
            fd as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            0,
            0,
            0,
        )
    }
}

fn sys_setsockopt(fd: i64, level: u64, opt: u64, val: *const u8, len: u64) -> i64 {
    unsafe { syscall5(SYS_SETSOCKOPT, fd as u64, level, opt, val as u64, len) }
}

fn sys_epoll_create1(flags: u64) -> i64 {
    unsafe { syscall1(SYS_EPOLL_CREATE1, flags) }
}

fn sys_epoll_ctl(epfd: i64, op: u64, fd: i64, ev: *const EpollEvent) -> i64 {
    unsafe { syscall4(SYS_EPOLL_CTL, epfd as u64, op, fd as u64, ev as u64) }
}

fn sys_epoll_wait(epfd: i64, events: *mut EpollEvent, maxevents: u64, timeout: i64) -> i64 {
    unsafe { syscall4(SYS_EPOLL_WAIT, epfd as u64, events as u64, maxevents, timeout as u64) }
}

/// Build a 16-byte `sockaddr_in` for 127.0.0.1:port.
fn loopback_sockaddr(port: u16) -> [u8; 16] {
    let mut sa = [0u8; 16];
    sa[0..2].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
    sa[2..4].copy_from_slice(&port.to_be_bytes()); // port, network order
    sa[4..8].copy_from_slice(&[127, 0, 0, 1]); // s_addr, network order
    sa
}

fn epoll_add(epfd: i64, fd: i64, events: u32) {
    let ev = EpollEvent { events, data: fd as u64 };
    check!(sys_epoll_ctl(epfd, EPOLL_CTL_ADD, fd, &ev) == 0, b"epoll_ctl ADD");
}

fn epoll_mod(epfd: i64, fd: i64, events: u32) {
    let ev = EpollEvent { events, data: fd as u64 };
    check!(sys_epoll_ctl(epfd, EPOLL_CTL_MOD, fd, &ev) == 0, b"epoll_ctl MOD");
}

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    const MSG: &[u8] = b"hello sumi net stack";

    let addr = loopback_sockaddr(PORT);

    // 1. Listening socket.
    let listener = sys_socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    check!(listener >= 0, b"socket(listener)");
    let one: u32 = 1;
    // SO_REUSEADDR is best-effort; do not fail the test on ENOPROTOOPT.
    let _ = sys_setsockopt(listener, SOL_SOCKET, SO_REUSEADDR, &one as *const u32 as *const u8, 4);
    check!(sys_bind(listener, addr.as_ptr(), 16) == 0, b"bind");
    check!(sys_listen(listener, 16) == 0, b"listen");

    // 2. Client socket + non-blocking connect.
    let client = sys_socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    check!(client >= 0, b"socket(client)");
    let cr = sys_connect(client, addr.as_ptr(), 16);
    check!(cr == 0 || cr == EINPROGRESS, b"connect (0 or EINPROGRESS)");

    // 3. epoll.
    let epfd = sys_epoll_create1(0);
    check!(epfd >= 0, b"epoll_create1");
    epoll_add(epfd, listener, EPOLLIN);
    epoll_add(epfd, client, EPOLLOUT);

    let mut server: i64 = -1;
    let mut sent = false;
    let mut echoed = false;

    let mut events = [EpollEvent { events: 0, data: 0 }; 8];

    // Bounded event loop; each step creates readiness for the next, so this
    // converges well before the cap. The cap only guards against a hang.
    for _ in 0..64 {
        let n = sys_epoll_wait(epfd, events.as_mut_ptr(), 8, 1000);
        check!(n >= 0, b"epoll_wait");
        if n == 0 {
            continue; // timeout tick; keep going until the cap.
        }
        for e in events.iter().take(n as usize) {
            let fd = e.data as i64;
            let ready = e.events;

            if fd == listener && (ready & EPOLLIN) != 0 {
                server = sys_accept4(listener, core::ptr::null_mut(), core::ptr::null_mut(), SOCK_NONBLOCK);
                check!(server >= 0, b"accept4");
                epoll_add(epfd, server, EPOLLIN);
            } else if fd == client && (ready & EPOLLOUT) != 0 && !sent {
                let w = sys_sendto(client, MSG);
                check!(w == MSG.len() as i64, b"client sendto");
                sent = true;
                epoll_mod(epfd, client, EPOLLIN); // now wait for the echo
            } else if fd == server && (ready & EPOLLIN) != 0 {
                let mut buf = [0u8; 64];
                let r = sys_recvfrom(server, &mut buf);
                check!(r == MSG.len() as i64, b"server recvfrom len");
                check!(&buf[..r as usize] == MSG, b"server payload matches");
                let w = sys_sendto(server, &buf[..r as usize]);
                check!(w == r, b"server echo sendto");
                echoed = true;
            } else if fd == client && (ready & EPOLLIN) != 0 && echoed {
                let mut buf = [0u8; 64];
                let r = sys_recvfrom(client, &mut buf);
                if r == EAGAIN {
                    continue; // echo not landed yet; loop again.
                }
                check!(r == MSG.len() as i64, b"client recvfrom len");
                check!(&buf[..r as usize] == MSG, b"echo round-trips intact");
                // Success: full accept + bidirectional echo over epoll.
                let _ = EPOLL_CTL_DEL; // silence unused const warning
                pass!();
            }
        }
    }

    check!(false, b"event loop did not complete the echo round-trip");
    0
}
