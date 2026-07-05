#![no_std]
#![no_main]

include!("../common.rs");

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let mut tms = Tms {
        tms_utime: -1,
        tms_stime: -1,
        tms_cutime: -1,
        tms_cstime: -1,
    };
    let ticks0 = sys_times(&mut tms);
    check!(ticks0 >= 0);
    check_eq!(tms.tms_utime, 0);
    check_eq!(tms.tms_stime, 0);
    check_eq!(tms.tms_cutime, 0);
    check_eq!(tms.tms_cstime, 0);

    let ticks1 = sys_times(core::ptr::null_mut());
    check!(ticks1 >= ticks0);

    let mut usage = Rusage {
        ru_utime: Timeval {
            tv_sec: -1,
            tv_usec: -1,
        },
        ru_stime: Timeval {
            tv_sec: -1,
            tv_usec: -1,
        },
        ru_rest: [-1; 14],
    };
    check_eq!(sys_getrusage(0, &mut usage), 0);
    check!(usage.ru_utime.tv_sec >= 0);
    check!(usage.ru_utime.tv_usec >= 0);
    check!(usage.ru_utime.tv_usec < 1_000_000);
    check_eq!(sys_getrusage(99, &mut usage), -22);
    check_eq!(sys_getrusage(0, core::ptr::null_mut()), -14);

    let mut cpu = u32::MAX;
    let mut node = u32::MAX;
    check_eq!(sys_getcpu(&mut cpu, &mut node), 0);
    check_eq!(cpu, 0);
    check_eq!(node, 0);

    let mut mode = -1;
    let mut nodemask = 0u64;
    check_eq!(
        sys_get_mempolicy(&mut mode, &mut nodemask, 64, 0, 0),
        0
    );
    check_eq!(mode, 0);
    check_eq!(nodemask, 1);

    check_eq!(
        sys_get_mempolicy(core::ptr::null_mut(), core::ptr::null_mut(), 0, 0, 0),
        0
    );
    check_eq!(
        sys_get_mempolicy(core::ptr::null_mut(), core::ptr::null_mut(), 0, 0, 1 << 63),
        -22
    );

    pass!();
}
