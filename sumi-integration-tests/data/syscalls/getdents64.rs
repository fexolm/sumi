#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

const O_DIRECTORY: u64 = 0o200000;

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let fd = sys_open(b"/tmp\0", O_RDONLY | O_DIRECTORY, 0);
    check!(fd >= 0);

    // We don't pin a specific entry — different hosts have different /tmp
    // contents. We just verify the syscall returns at least one record and
    // each record has a sane reclen.
    let mut buf = [0u8; 4096];
    let n = sys_getdents64(fd, &mut buf);
    check!(n > 0);

    // The Linux dirent64 layout: u64 d_ino, i64 d_off, u16 d_reclen, u8 d_type,
    // then a NUL-terminated d_name padded so the next reclen is 8-aligned.
    // Minimum possible reclen is 24 (19 byte header + 1 char name + 1 NUL,
    // padded to 8). We check ≥ that and that the chain walks cleanly.
    let mut off = 0usize;
    let mut entries = 0;
    while off < n as usize {
        let entry = unsafe { &*(buf.as_ptr().add(off) as *const LinuxDirent64) };
        check!(entry.d_reclen >= 24);
        check!((entry.d_reclen as usize) % 8 == 0);
        off += entry.d_reclen as usize;
        entries += 1;
        check!(entries < 1024);
    }
    check!(entries > 0);

    check_eq!(sys_close(fd), 0);
    pass!();
}
