// Virtio-net + userspace gateway echo (Phase 2a networking, see
// docs/networking-design.md). Proves guest-initiated TCP works end to end
// over the real virtio-net transport: the guest connects out to the host
// gateway's echo peer at 10.0.2.2:ECHO_PORT, sends a payload, and verifies
// the bytes come back unchanged.
//
// Unlike tcp_epoll_loopback.rs (Phase 1, in-guest loopback device, no host
// networking), this exercises: the virtio-net driver init/feature
// negotiation, the RX pull handshake (kicked once per timer tick), the host
// virtio-net backend, and the host gateway's own smoltcp interface +
// echo socket. Blocking sockets are used (not epoll) — blocking connect/
// send/recv already drive `net::wait::net_wait`, which polls the stack
// (and thus kicks the RX queue) on every iteration and re-checks after
// every timer-tick wakeup, so no application-level polling loop is needed.

#![no_std]
#![no_main]

include!("../common.rs");

const SYS_CONNECT: u64 = 42;
const SYS_SENDTO: u64 = 44;
const SYS_RECVFROM: u64 = 45;
const SYS_SOCKET: u64 = 41;

const AF_INET: u64 = 2;
const SOCK_STREAM: u64 = 1;

// Must match `sumi_vm::net::gateway::ECHO_PORT`.
const ECHO_PORT: u16 = 7777;
// Must match `sumi_abi::net::GATEWAY_IP`.
const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];

fn sys_socket(domain: u64, ty: u64, proto: u64) -> i64 {
    unsafe { syscall3(SYS_SOCKET, domain, ty, proto) }
}

fn sys_connect(fd: i64, addr: *const u8, len: u64) -> i64 {
    unsafe { syscall3(SYS_CONNECT, fd as u64, addr as u64, len) }
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

/// Build a 16-byte `sockaddr_in` for `GATEWAY_IP:port`.
fn gateway_sockaddr(port: u16) -> [u8; 16] {
    let mut sa = [0u8; 16];
    sa[0..2].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
    sa[2..4].copy_from_slice(&port.to_be_bytes());
    sa[4..8].copy_from_slice(&GATEWAY_IP);
    sa
}

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    const MSG: &[u8] = b"hello over virtio-net";

    let fd = sys_socket(AF_INET, SOCK_STREAM, 0);
    check!(fd >= 0, b"socket");

    let addr = gateway_sockaddr(ECHO_PORT);
    check!(
        sys_connect(fd, addr.as_ptr(), 16) == 0,
        b"connect to gateway echo peer"
    );

    let mut sent = 0usize;
    while sent < MSG.len() {
        let w = sys_sendto(fd, &MSG[sent..]);
        check!(w > 0, b"sendto");
        sent += w as usize;
    }

    let mut buf = [0u8; 64];
    let mut got = 0usize;
    while got < MSG.len() {
        let r = sys_recvfrom(fd, &mut buf[got..]);
        check!(r > 0, b"recvfrom");
        got += r as usize;
    }
    check!(&buf[..got] == MSG, b"echo round-trips intact over virtio-net");

    pass!();
}
