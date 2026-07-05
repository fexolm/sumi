// Host -> guest TCP port forwarding (Phase 2b networking, see
// docs/networking-design.md). Binds a TCP echo server on the guest's own
// virtio-net address (10.0.2.15:GUEST_PORT) using the same non-blocking
// socket + epoll/accept4 path Phase 1's tcp_epoll_loopback.rs exercises —
// the guest side of host->guest forwarding needs no special-casing, it's
// just a normal listener. The host test launcher (test_launcher.rs) starts
// sumi-vm with `--hostfwd tcp:127.0.0.1:HOST_PORT-10.0.2.15:GUEST_PORT`,
// connects a real `std::net::TcpStream` to the forwarded host port, and
// verifies the echoed round-trip; this program provides the guest half.
//
// Flow:
//   1. listener = socket(); bind(10.0.2.15:GUEST_PORT); listen()
//   2. epoll ADD listener(EPOLLIN)
//   3. listener readable -> accept4() -> server fd; epoll ADD server(EPOLLIN)
//   4. server readable -> recvfrom(); if 0 bytes (client closed) -> pass!();
//      else echo the same bytes back and keep going.
//
// Bounded by a fixed number of epoll_wait iterations (each with a timeout)
// so a bug that prevents the host connection from ever arriving fails the
// test instead of hanging until the harness's own 30s kill-timeout.

#![no_std]
#![no_main]

include!("../common.rs");

// ── net syscall numbers ────────────────────────────────────────────────────
const SYS_ACCEPT4: u64 = 288;
const SYS_SENDTO: u64 = 44;
const SYS_RECVFROM: u64 = 45;
const SYS_BIND: u64 = 49;
const SYS_LISTEN: u64 = 50;
const SYS_SOCKET: u64 = 41;
const SYS_EPOLL_CREATE1: u64 = 291;
const SYS_EPOLL_CTL: u64 = 233;
const SYS_EPOLL_WAIT: u64 = 232;

// ── net constants ──────────────────────────────────────────────────────────
const AF_INET: u64 = 2;
const SOCK_STREAM: u64 = 1;
const SOCK_NONBLOCK: u64 = 0o4000; // 0x800

const EPOLL_CTL_ADD: u64 = 1;
const EPOLLIN: u32 = 0x001;

// Must match the host test's guest-side forward target
// (test_launcher.rs::hostfwd_echo) and sumi_abi::net::GUEST_IP.
const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
const GUEST_PORT: u16 = 9100;

// `struct epoll_event` is packed on x86_64: 4-byte events + 8-byte data.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct EpollEvent {
    events: u32,
    data: u64,
}

fn sys_socket(domain: u64, ty: u64, proto: u64) -> i64 {
    unsafe { syscall3(SYS_SOCKET, domain, ty, proto) }
}

fn sys_bind(fd: i64, addr: *const u8, len: u64) -> i64 {
    unsafe { syscall3(SYS_BIND, fd as u64, addr as u64, len) }
}

fn sys_listen(fd: i64, backlog: u64) -> i64 {
    unsafe { syscall2(SYS_LISTEN, fd as u64, backlog) }
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
            0,
            0,
            0,
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

fn sys_epoll_create1(flags: u64) -> i64 {
    unsafe { syscall1(SYS_EPOLL_CREATE1, flags) }
}

fn sys_epoll_ctl(epfd: i64, op: u64, fd: i64, ev: *const EpollEvent) -> i64 {
    unsafe { syscall4(SYS_EPOLL_CTL, epfd as u64, op, fd as u64, ev as u64) }
}

fn sys_epoll_wait(epfd: i64, events: *mut EpollEvent, maxevents: u64, timeout: i64) -> i64 {
    unsafe { syscall4(SYS_EPOLL_WAIT, epfd as u64, events as u64, maxevents, timeout as u64) }
}

/// Build a 16-byte `sockaddr_in` for `GUEST_IP:port`.
fn guest_sockaddr(port: u16) -> [u8; 16] {
    let mut sa = [0u8; 16];
    sa[0..2].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
    sa[2..4].copy_from_slice(&port.to_be_bytes());
    sa[4..8].copy_from_slice(&GUEST_IP);
    sa
}

fn epoll_add(epfd: i64, fd: i64, events: u32) {
    let ev = EpollEvent { events, data: fd as u64 };
    check!(sys_epoll_ctl(epfd, EPOLL_CTL_ADD, fd, &ev) == 0, b"epoll_ctl ADD");
}

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let addr = guest_sockaddr(GUEST_PORT);

    let listener = sys_socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    check!(listener >= 0, b"socket(listener)");
    check!(sys_bind(listener, addr.as_ptr(), 16) == 0, b"bind");
    check!(sys_listen(listener, 1) == 0, b"listen");

    let epfd = sys_epoll_create1(0);
    check!(epfd >= 0, b"epoll_create1");
    epoll_add(epfd, listener, EPOLLIN);

    let mut server: i64 = -1;
    let mut events = [EpollEvent { events: 0, data: 0 }; 4];

    // Each epoll_wait waits up to 100ms; 200 iterations = up to 20s, safely
    // under the harness's 30s kill-timeout while still generous for the
    // host client's own retry-connect loop.
    for _ in 0..200 {
        let n = sys_epoll_wait(epfd, events.as_mut_ptr(), 4, 100);
        check!(n >= 0, b"epoll_wait");

        for e in events.iter().take(n as usize) {
            let fd = e.data as i64;
            let ready = e.events;

            if fd == listener && (ready & EPOLLIN) != 0 {
                server = sys_accept4(
                    listener,
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                    SOCK_NONBLOCK,
                );
                if server >= 0 {
                    epoll_add(epfd, server, EPOLLIN);
                }
            } else if fd == server && (ready & EPOLLIN) != 0 {
                let mut buf = [0u8; 256];
                let r = sys_recvfrom(server, &mut buf);
                if r == 0 {
                    // Host client closed after receiving its echo: done.
                    pass!();
                } else if r > 0 {
                    let n = r as usize;
                    let mut sent = 0usize;
                    while sent < n {
                        let w = sys_sendto(server, &buf[sent..n]);
                        check!(w > 0, b"echo sendto");
                        sent += w as usize;
                    }
                }
                // r < 0 (EAGAIN): nothing ready yet; epoll will tell us again.
            }
        }
    }

    check!(false, b"hostfwd echo server: no client interaction within budget");
    0
}
