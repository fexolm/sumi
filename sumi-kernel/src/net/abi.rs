//! User-boundary parsing: `sockaddr_in` and `struct epoll_event`.
//!
//! Like `read_iovec` in `syscall/handlers/io.rs`, these functions deref user
//! pointers directly — valid in sumi's single-address-space model where the
//! kernel and every user program share one page table.

use smoltcp::wire::{IpAddress, IpEndpoint};

use crate::net::socket::AF_INET;
use crate::syscall::errno::{EAFNOSUPPORT, EFAULT, EINVAL};

/// `struct epoll_event` is packed on x86_64: a 4-byte `events` immediately
/// followed by an 8-byte `data`, with no alignment padding (12 bytes total).
#[repr(C, packed)]
pub struct EpollEvent {
    pub events: u32,
    pub data: u64,
}

/// Parse a `sockaddr_in` at `uaddr` (must be at least 16 bytes) into an
/// `IpEndpoint`. Per the Linux ABI, `sa_family` is native-endian; port and
/// address are network- (big-) endian.
pub fn parse_sockaddr(uaddr: u64, ulen: u32) -> Result<IpEndpoint, i64> {
    if uaddr == 0 {
        return Err(EFAULT);
    }
    if ulen < 16 {
        return Err(EINVAL);
    }
    // SAFETY: sumi's single-address-space model maps every user pointer
    // into kernel-visible memory (same precondition as `read_iovec` in
    // handlers/io.rs); the caller-supplied `ulen` was checked above.
    let bytes = unsafe { core::slice::from_raw_parts(uaddr as *const u8, 16) };
    let family = u16::from_ne_bytes([bytes[0], bytes[1]]);
    if family != AF_INET {
        return Err(EAFNOSUPPORT);
    }
    let port = u16::from_be_bytes([bytes[2], bytes[3]]);
    let addr = IpAddress::v4(bytes[4], bytes[5], bytes[6], bytes[7]);
    Ok(IpEndpoint::new(addr, port))
}

/// Write `ep` back to user memory as a `sockaddr_in`. `uaddr == 0` is a
/// silent no-op (matches `accept4(fd, NULL, NULL, flags)`). `ulen_ptr == 0`
/// skips the length-out write. Always writes the full 16-byte structure
/// (Linux truncates to the caller's buffer size, but Phase 1 callers always
/// pass a 16-byte `sockaddr_in` buffer).
pub fn write_sockaddr(uaddr: u64, ulen_ptr: u64, ep: IpEndpoint) -> Result<(), i64> {
    if uaddr == 0 {
        return Ok(());
    }
    let IpAddress::Ipv4(v4) = ep.addr else {
        return Err(EAFNOSUPPORT);
    };
    let mut sa = [0u8; 16];
    sa[0..2].copy_from_slice(&AF_INET.to_ne_bytes());
    sa[2..4].copy_from_slice(&ep.port.to_be_bytes());
    sa[4..8].copy_from_slice(&v4.octets());

    // SAFETY: single-address-space model; `uaddr` is a valid, writable
    // 16-byte user buffer per the syscall ABI contract (sockaddr_in).
    unsafe {
        core::ptr::copy_nonoverlapping(sa.as_ptr(), uaddr as *mut u8, 16);
    }
    if ulen_ptr != 0 {
        // SAFETY: `ulen_ptr` is a valid, writable user `socklen_t*`.
        unsafe {
            core::ptr::write_unaligned(ulen_ptr as *mut u32, 16);
        }
    }
    Ok(())
}

/// Read a `struct epoll_event` from user memory.
///
/// # Safety
/// `p` must point to a valid, mapped 12-byte `epoll_event`. The struct is
/// packed, so this reads each field with `read_unaligned` rather than
/// dereferencing a `*const EpollEvent` (which would require 8-byte
/// alignment for `data`).
pub unsafe fn read_epoll_event(p: u64) -> EpollEvent {
    // SAFETY: caller upholds the pointer-validity precondition above.
    unsafe {
        let events = core::ptr::read_unaligned(p as *const u32);
        let data = core::ptr::read_unaligned((p + 4) as *const u64);
        EpollEvent { events, data }
    }
}

/// Write a `struct epoll_event` to user memory.
///
/// # Safety
/// `p` must point to a valid, mapped, writable 12-byte buffer. See
/// `read_epoll_event` for why this uses `write_unaligned` field-by-field.
pub unsafe fn write_epoll_event(p: u64, ev: &EpollEvent) {
    // SAFETY: caller upholds the pointer-validity precondition above.
    unsafe {
        core::ptr::write_unaligned(p as *mut u32, ev.events);
        core::ptr::write_unaligned((p + 4) as *mut u64, ev.data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sockaddr_round_trips_loopback() {
        let mut sa = [0u8; 16];
        sa[0..2].copy_from_slice(&AF_INET.to_ne_bytes());
        sa[2..4].copy_from_slice(&3456u16.to_be_bytes());
        sa[4..8].copy_from_slice(&[127, 0, 0, 1]);

        let ep = parse_sockaddr(sa.as_ptr() as u64, 16).expect("valid sockaddr_in");
        assert_eq!(ep.port, 3456);
        assert_eq!(ep.addr, IpAddress::v4(127, 0, 0, 1));
    }

    #[test]
    fn parse_sockaddr_rejects_short_len() {
        let sa = [0u8; 16];
        assert_eq!(parse_sockaddr(sa.as_ptr() as u64, 8), Err(EINVAL));
    }

    #[test]
    fn parse_sockaddr_rejects_bad_family() {
        let mut sa = [0u8; 16];
        sa[0..2].copy_from_slice(&99u16.to_ne_bytes());
        assert_eq!(parse_sockaddr(sa.as_ptr() as u64, 16), Err(EAFNOSUPPORT));
    }

    #[test]
    fn parse_sockaddr_rejects_null() {
        assert_eq!(parse_sockaddr(0, 16), Err(EFAULT));
    }

    #[test]
    fn write_sockaddr_round_trips() {
        let mut sa = [0xFFu8; 16];
        let mut len: u32 = 16;
        let ep = IpEndpoint::new(IpAddress::v4(10, 0, 2, 15), 8080);
        write_sockaddr(sa.as_mut_ptr() as u64, &mut len as *mut u32 as u64, ep)
            .expect("write_sockaddr");
        assert_eq!(len, 16);
        let round = parse_sockaddr(sa.as_ptr() as u64, 16).unwrap();
        assert_eq!(round.port, 8080);
        assert_eq!(round.addr, IpAddress::v4(10, 0, 2, 15));
    }

    #[test]
    fn epoll_event_round_trips_at_unaligned_offset() {
        // 12-byte events + a 1-byte pad forces the next struct to start at
        // an address not 8-aligned for `data` — exactly the layout the
        // kernel sees for `events[1]`, `events[2]`, ... in an array.
        let mut buf = [0u8; 32];
        let p = buf.as_mut_ptr() as u64 + 1;
        let ev = EpollEvent {
            events: 0xDEADBEEF,
            data: 0x0123_4567_89AB_CDEF,
        };
        // SAFETY: `buf` is a 32-byte local array; `p` + 12 bytes stays in bounds.
        unsafe { write_epoll_event(p, &ev) };
        // SAFETY: same buffer, just written above.
        let back = unsafe { read_epoll_event(p) };
        let (events, data) = (back.events, back.data); // copy out of the packed struct first
        assert_eq!(events, 0xDEADBEEF);
        assert_eq!(data, 0x0123_4567_89AB_CDEF);
    }
}
