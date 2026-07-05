#![no_std]
#![no_main]

include!("../common.rs");

const AF_UNIX: u64 = 1;
const SOCK_STREAM: u64 = 1;
const SYS_SOCKET: u64 = 41;
const SYS_BIND: u64 = 49;
const SYS_LISTEN: u64 = 50;
const SYS_GETSOCKNAME: u64 = 51;

#[inline]
fn sys_socket(domain: u64, ty: u64, proto: u64) -> i64 {
    unsafe { syscall3(SYS_SOCKET, domain, ty, proto) }
}

#[inline]
fn sys_bind(fd: i64, addr: *const u8, len: u64) -> i64 {
    unsafe { syscall3(SYS_BIND, fd as u64, addr as u64, len) }
}

#[inline]
fn sys_listen(fd: i64, backlog: u64) -> i64 {
    unsafe { syscall2(SYS_LISTEN, fd as u64, backlog) }
}

#[inline]
fn sys_getsockname(fd: i64, addr: *mut u8, len: *mut u32) -> i64 {
    unsafe { syscall3(SYS_GETSOCKNAME, fd as u64, addr as u64, len as u64) }
}

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let fd = sys_socket(AF_UNIX, SOCK_STREAM, 0);
    check!(fd >= 0);

    let mut sa = [0u8; 110];
    sa[0..2].copy_from_slice(&(AF_UNIX as u16).to_ne_bytes());
    let path = b"/tmp/sumi_int_mysql.sock\0";
    let mut i = 0;
    while i < path.len() {
        sa[2 + i] = path[i];
        i += 1;
    }

    check_eq!(sys_bind(fd, sa.as_ptr(), sa.len() as u64), 0);
    check_eq!(sys_listen(fd, 16), 0);

    let mut out = [0u8; 110];
    let mut out_len = out.len() as u32;
    check_eq!(sys_getsockname(fd, out.as_mut_ptr(), &mut out_len), 0);
    check_eq!(u16::from_ne_bytes([out[0], out[1]]) as u64, AF_UNIX);
    check!(out_len >= 2);

    check_eq!(sys_close(fd), 0);
    pass!();
}
