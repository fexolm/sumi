#![no_std]
#![no_main]

include!("../common.rs");

const EAGAIN: i64 = -11;
const EINVAL: i64 = -22;
const POLLIN: i16 = 0x0001;
const POLLOUT: i16 = 0x0004;
const POLLERR: i16 = 0x0008;
const POLLHUP: i16 = 0x0010;
const O_NONBLOCK_LOCAL: u64 = 0o4000;
const O_CLOEXEC_LOCAL: u64 = 0o2000000;
const PIPE_CAPACITY: usize = 65536;

fn poll_one(fd: i32, events: i16) -> (i64, i16) {
    let mut pfd = PollFd {
        fd,
        events,
        revents: -1,
    };
    let n = sys_poll(&mut pfd as *mut PollFd, 1, 0);
    (n, pfd.revents)
}

#[unsafe(no_mangle)]
pub extern "C" fn sumi_main() -> i32 {
    let mut bad = [0i32; 2];
    check_eq!(sys_pipe2(&mut bad, 0x4000_0000), EINVAL);

    let mut fds = [0i32; 2];
    check_eq!(sys_pipe2(&mut fds, O_NONBLOCK_LOCAL | O_CLOEXEC_LOCAL), 0);
    let read_fd = fds[0];
    let write_fd = fds[1];

    let (n, revents) = poll_one(read_fd, POLLIN);
    check_eq!(n, 0);
    check_eq!(revents, 0);

    let (n, revents) = poll_one(write_fd, POLLOUT);
    check_eq!(n, 1);
    check!((revents & POLLOUT) != 0);

    let mut one = [0u8; 1];
    check_eq!(sys_read(read_fd as i64, &mut one), EAGAIN);

    let chunk = [b'x'; 4096];
    for _ in 0..15 {
        check_eq!(sys_write(write_fd as i64, &chunk), chunk.len() as i64);
    }
    let tail = [b'y'; 4093];
    check_eq!(sys_write(write_fd as i64, &tail), tail.len() as i64);

    let (n, revents) = poll_one(write_fd, POLLOUT);
    check_eq!(n, 0);
    check_eq!(revents, 0);

    check_eq!(sys_write(write_fd as i64, b"hello"), EAGAIN);
    check_eq!(sys_close(write_fd as i64), 0);

    let mut buf = [0u8; PIPE_CAPACITY];
    check_eq!(
        sys_read(read_fd as i64, &mut buf),
        (PIPE_CAPACITY - 3) as i64
    );
    check_eq!(sys_read(read_fd as i64, &mut one), 0);
    check_eq!(sys_close(read_fd as i64), 0);

    let mut hup = [0i32; 2];
    check_eq!(sys_pipe(&mut hup), 0);
    check_eq!(sys_close(hup[1] as i64), 0);
    let (n, revents) = poll_one(hup[0], POLLIN);
    check_eq!(n, 1);
    check!((revents & POLLHUP) != 0);
    check_eq!(sys_close(hup[0] as i64), 0);

    let mut err = [0i32; 2];
    check_eq!(sys_pipe(&mut err), 0);
    check_eq!(sys_close(err[0] as i64), 0);
    let (n, revents) = poll_one(err[1], POLLOUT);
    check_eq!(n, 1);
    check!((revents & POLLERR) != 0);
    check_eq!(sys_close(err[1] as i64), 0);

    pass!();
}
