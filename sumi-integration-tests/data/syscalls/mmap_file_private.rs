#![no_std]
#![no_main]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    // Create a file and write known content. The kernel must keep the
    // fd's cached size in sync with writes so the mmap below can find
    // the bytes we just wrote without us having to close and re-open.
    let path = b"/tmp/sumi_int_mmap_priv.txt\0";
    let fd = sys_open(path, O_RDWR | O_CREAT | O_TRUNC, 0o644);
    check!(fd >= 0);
    let payload = [b'M'; 4096];
    check_eq!(sys_write(fd, &payload), 4096);

    // MAP_PRIVATE | PROT_READ|PROT_WRITE → private copy backed by physical pages.
    let len = 4096u64;
    let addr = sys_mmap(0, len, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    check!(addr > 0);

    let p = addr as *const u8;
    unsafe {
        check_eq!(*p, b'M');
        check_eq!(*p.add(4095), b'M');
    }

    // Modifying the private copy must NOT affect the underlying file.
    unsafe {
        *(addr as *mut u8) = b'!';
    }
    let mut buf = [0u8; 1];
    check_eq!(sys_pread64(fd, &mut buf, 0), 1);
    check_eq!(buf[0] as i64, b'M' as i64);

    check_eq!(sys_munmap(addr as u64, len), 0);
    check_eq!(sys_close(fd), 0);
    pass!();
}
