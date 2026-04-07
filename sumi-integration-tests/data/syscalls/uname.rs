#![no_std]
#![no_main]
#![allow(dead_code, unused_macros)]

include!("../common.rs");

#[repr(C)]
struct UtsName {
    sysname: [u8; 65],
    nodename: [u8; 65],
    release: [u8; 65],
    version: [u8; 65],
    machine: [u8; 65],
    domainname: [u8; 65],
}

fn cstr_eq(field: &[u8], expected: &[u8]) -> bool {
    let n = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    &field[..n] == expected
}

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let mut uts: UtsName = unsafe { core::mem::zeroed() };
    let r = sys_uname(&mut uts as *mut _ as *mut u8);
    check_eq!(r, 0);

    // We don't pin specific strings — exact values are an implementation
    // choice. We just verify the kernel actually populated the buffer so
    // a libc reading uname doesn't see garbage.
    check!(uts.sysname[0] != 0);
    check!(uts.machine[0] != 0);
    check!(uts.release[0] != 0);
    pass!();
}
