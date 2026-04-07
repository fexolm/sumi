#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let path = b"/tmp/sumi_int_iov.txt\0";
    let fd = sys_open(path, O_RDWR | O_CREAT | O_TRUNC, 0o644);
    check!(fd >= 0);

    let s1 = b"foo:";
    let s2 = b"bar:";
    let s3 = b"baz!";
    let iov = [
        Iovec {
            iov_base: s1.as_ptr() as *mut u8,
            iov_len: s1.len(),
        },
        Iovec {
            iov_base: s2.as_ptr() as *mut u8,
            iov_len: s2.len(),
        },
        Iovec {
            iov_base: s3.as_ptr() as *mut u8,
            iov_len: s3.len(),
        },
    ];
    let n = sys_writev(fd, iov.as_ptr(), iov.len());
    check_eq!(n, 12);

    // Read back via readv.
    check_eq!(sys_lseek(fd, 0, SEEK_SET), 0);
    let mut b1 = [0u8; 4];
    let mut b2 = [0u8; 4];
    let mut b3 = [0u8; 4];
    let riov = [
        Iovec {
            iov_base: b1.as_mut_ptr(),
            iov_len: b1.len(),
        },
        Iovec {
            iov_base: b2.as_mut_ptr(),
            iov_len: b2.len(),
        },
        Iovec {
            iov_base: b3.as_mut_ptr(),
            iov_len: b3.len(),
        },
    ];
    let n = sys_readv(fd, riov.as_ptr(), riov.len());
    check_eq!(n, 12);
    check_eq!(b1[0] as i64, b'f' as i64);
    check_eq!(b2[0] as i64, b'b' as i64);
    check_eq!(b3[3] as i64, b'!' as i64);

    check_eq!(sys_close(fd), 0);
    pass!();
}
