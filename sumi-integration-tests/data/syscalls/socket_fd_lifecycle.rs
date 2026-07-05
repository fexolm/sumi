// Socket/epoll fd lifecycle edge cases:
// - duplicated socket/epoll descriptors keep their underlying net objects
//   alive until the last fd closes;
// - fcntl(F_SETFL, O_NONBLOCK) updates socket behavior, not just fd flags;
// - zero-length socket I/O accepts a null buffer and returns 0.

#![no_std]
#![no_main]

include!("../common.rs");

const SYS_SOCKET: u64 = 41;
const SYS_ACCEPT4: u64 = 288;
const SYS_SENDTO: u64 = 44;
const SYS_RECVFROM: u64 = 45;
const SYS_BIND: u64 = 49;
const SYS_LISTEN: u64 = 50;
const SYS_EPOLL_CREATE1: u64 = 291;
const SYS_EPOLL_WAIT: u64 = 232;

const AF_INET: u64 = 2;
const SOCK_STREAM: u64 = 1;
const SOCK_NONBLOCK: u64 = 0o4000;
const O_NONBLOCK: u64 = 0o4000;

const F_GETFL: u64 = 3;
const F_SETFL: u64 = 4;

const EAGAIN: i64 = -11;
const ENOTCONN: i64 = -107;

const PORT: u16 = 3461;

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

fn sys_sendto_raw(fd: i64, ptr: u64, len: u64) -> i64 {
    unsafe { syscall6(SYS_SENDTO, fd as u64, ptr, len, 0, 0, 0) }
}

fn sys_recvfrom_raw(fd: i64, ptr: u64, len: u64) -> i64 {
    unsafe { syscall6(SYS_RECVFROM, fd as u64, ptr, len, 0, 0, 0) }
}

fn sys_epoll_create1(flags: u64) -> i64 {
    unsafe { syscall1(SYS_EPOLL_CREATE1, flags) }
}

fn sys_epoll_wait(epfd: i64, events: *mut EpollEvent, maxevents: u64, timeout: i64) -> i64 {
    unsafe { syscall4(SYS_EPOLL_WAIT, epfd as u64, events as u64, maxevents, timeout as u64) }
}

fn loopback_sockaddr(port: u16) -> [u8; 16] {
    let mut sa = [0u8; 16];
    sa[0..2].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
    sa[2..4].copy_from_slice(&port.to_be_bytes());
    sa[4..8].copy_from_slice(&[127, 0, 0, 1]);
    sa
}

fn socket_fd_survives_dup_close() {
    let s = sys_socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    check!(s >= 0, b"socket");
    let duped = sys_dup(s);
    check!(duped >= 0, b"dup socket");

    check_eq!(sys_close(s), 0);

    let msg = [1u8];
    check_eq!(
        sys_sendto_raw(duped, msg.as_ptr() as u64, msg.len() as u64),
        ENOTCONN
    );
    check_eq!(sys_close(duped), 0);
}

fn socket_fd_survives_dup2_eviction() {
    let s = sys_socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    check!(s >= 0, b"socket s");
    let duped = sys_dup(s);
    check!(duped >= 0, b"dup socket");
    let victim = sys_socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    check!(victim >= 0, b"socket victim");

    check_eq!(sys_dup2(s, victim), victim);
    check_eq!(sys_close(s), 0);

    let msg = [1u8];
    check_eq!(
        sys_sendto_raw(duped, msg.as_ptr() as u64, msg.len() as u64),
        ENOTCONN
    );
    check_eq!(
        sys_sendto_raw(victim, msg.as_ptr() as u64, msg.len() as u64),
        ENOTCONN
    );
    check_eq!(sys_close(duped), 0);
    check_eq!(sys_close(victim), 0);
}

fn epoll_fd_survives_dup_close() {
    let epfd = sys_epoll_create1(0);
    check!(epfd >= 0, b"epoll_create1");
    let duped = sys_dup(epfd);
    check!(duped >= 0, b"dup epoll");

    check_eq!(sys_close(epfd), 0);

    let mut events = [EpollEvent { events: 0, data: 0 }; 1];
    check_eq!(sys_epoll_wait(duped, events.as_mut_ptr(), 1, 0), 0);
    check_eq!(sys_close(duped), 0);
}

fn fcntl_setfl_nonblock_reaches_socket() {
    let listener = sys_socket(AF_INET, SOCK_STREAM, 0);
    check!(listener >= 0, b"socket listener");

    let addr = loopback_sockaddr(PORT);
    check_eq!(sys_bind(listener, addr.as_ptr(), 16), 0);
    check_eq!(sys_listen(listener, 1), 0);

    let old_flags = sys_fcntl(listener, F_GETFL, 0);
    check!(old_flags >= 0, b"F_GETFL");
    check_eq!(old_flags & O_NONBLOCK as i64, 0);
    check_eq!(sys_fcntl(listener, F_SETFL, old_flags as u64 | O_NONBLOCK), 0);

    let new_flags = sys_fcntl(listener, F_GETFL, 0);
    check!(new_flags >= 0, b"F_GETFL after F_SETFL");
    check!((new_flags & O_NONBLOCK as i64) != 0, b"O_NONBLOCK flag stored");

    let r = sys_accept4(listener, core::ptr::null_mut(), core::ptr::null_mut(), 0);
    check_eq!(r, EAGAIN);
    check_eq!(sys_close(listener), 0);
}

fn zero_len_socket_io_accepts_null_buffer() {
    let s = sys_socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    check!(s >= 0, b"socket zero len");

    check_eq!(sys_sendto_raw(s, 0, 0), 0);
    check_eq!(sys_recvfrom_raw(s, 0, 0), 0);
    check_eq!(sys_close(s), 0);
}

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    socket_fd_survives_dup_close();
    socket_fd_survives_dup2_eviction();
    epoll_fd_survives_dup_close();
    fcntl_setfl_nonblock_reaches_socket();
    zero_len_socket_io_accepts_null_buffer();
    pass!();
}
