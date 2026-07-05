use std::env;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_HOST_PORT: u16 = 19101;
const DEFAULT_GUEST_PORT: u16 = 9101;
const DEFAULT_PAYLOAD_BYTES: usize = 4096;
const DEFAULT_ROUNDS: usize = 1000;
const DEFAULT_WARMUP_ROUNDS: usize = 32;
const DEFAULT_VCPUS: usize = 1;
const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 15_000;
const DEFAULT_CHILD_TIMEOUT_MS: u64 = 30_000;
const GUEST_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const LOOPBACK_IP: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);

#[derive(Clone)]
struct Config {
    host_port: u16,
    guest_port: u16,
    payload_bytes: usize,
    rounds: usize,
    warmup_rounds: usize,
    vcpus: usize,
    vm: PathBuf,
    kernel: PathBuf,
    guest_bin: PathBuf,
    bind_ip: Ipv4Addr,
    port: u16,
    packets: usize,
    connect_timeout: Duration,
    child_timeout: Duration,
}

impl Config {
    fn new() -> Self {
        Self {
            host_port: DEFAULT_HOST_PORT,
            guest_port: DEFAULT_GUEST_PORT,
            payload_bytes: DEFAULT_PAYLOAD_BYTES,
            rounds: DEFAULT_ROUNDS,
            warmup_rounds: DEFAULT_WARMUP_ROUNDS,
            vcpus: DEFAULT_VCPUS,
            vm: PathBuf::from("target/debug/sumi-vm"),
            kernel: PathBuf::from("target/x86_64-unknown-none/release/sumi-kernel"),
            guest_bin: PathBuf::new(),
            bind_ip: GUEST_IP,
            port: DEFAULT_GUEST_PORT,
            packets: DEFAULT_ROUNDS + DEFAULT_WARMUP_ROUNDS,
            connect_timeout: Duration::from_millis(DEFAULT_CONNECT_TIMEOUT_MS),
            child_timeout: Duration::from_millis(DEFAULT_CHILD_TIMEOUT_MS),
        }
    }

    fn ensure_guest_bin(&mut self) -> Result<(), String> {
        if self.guest_bin.as_os_str().is_empty() {
            self.guest_bin = env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        }
        Ok(())
    }

    fn measured_packets(&self) -> usize {
        self.rounds
    }

    fn total_packets(&self) -> usize {
        self.rounds + self.warmup_rounds
    }
}

struct Args {
    items: Vec<String>,
    pos: usize,
}

impl Args {
    fn new(items: Vec<String>) -> Self {
        Self { items, pos: 0 }
    }

    fn next(&mut self) -> Option<String> {
        let value = self.items.get(self.pos).cloned();
        if value.is_some() {
            self.pos += 1;
        }
        value
    }

    fn next_value(&mut self, name: &str) -> Result<String, String> {
        self.next()
            .ok_or_else(|| format!("{name} requires a value"))
    }

    fn parse_common(
        &mut self,
        cfg: &mut Config,
        allow_server_options: bool,
        derive_packets: bool,
    ) -> Result<(), String> {
        while let Some(arg) = self.next() {
            match arg.as_str() {
                "--host-port" => cfg.host_port = parse_u16(&self.next_value(&arg)?, &arg)?,
                "--guest-port" => cfg.guest_port = parse_u16(&self.next_value(&arg)?, &arg)?,
                "--payload-bytes" => {
                    cfg.payload_bytes = parse_usize(&self.next_value(&arg)?, &arg)?.max(1);
                }
                "--rounds" => cfg.rounds = parse_usize(&self.next_value(&arg)?, &arg)?.max(1),
                "--warmup-rounds" => {
                    cfg.warmup_rounds = parse_usize(&self.next_value(&arg)?, &arg)?.max(1);
                }
                "--vcpus" => cfg.vcpus = parse_usize(&self.next_value(&arg)?, &arg)?.max(1),
                "--vm" => cfg.vm = PathBuf::from(self.next_value(&arg)?),
                "--kernel" => cfg.kernel = PathBuf::from(self.next_value(&arg)?),
                "--guest-bin" => cfg.guest_bin = PathBuf::from(self.next_value(&arg)?),
                "--connect-timeout-ms" => {
                    cfg.connect_timeout =
                        Duration::from_millis(parse_u64(&self.next_value(&arg)?, &arg)?.max(1));
                }
                "--child-timeout-ms" => {
                    cfg.child_timeout =
                        Duration::from_millis(parse_u64(&self.next_value(&arg)?, &arg)?.max(1));
                }
                "--bind-ip" if allow_server_options => {
                    cfg.bind_ip = parse_ipv4(&self.next_value(&arg)?, &arg)?;
                }
                "--port" if allow_server_options => {
                    cfg.port = parse_u16(&self.next_value(&arg)?, &arg)?;
                }
                "--packets" if allow_server_options => {
                    cfg.packets = parse_usize(&self.next_value(&arg)?, &arg)?.max(1);
                }
                "--help" | "-h" => return Err(usage()),
                _ => return Err(format!("unknown argument {arg:?}\n\n{}", usage())),
            }
        }
        if derive_packets {
            cfg.packets = cfg.total_packets();
        }
        Ok(())
    }
}

struct BenchReport {
    elapsed: Duration,
    packets: usize,
    payload_bytes: usize,
}

struct Client {
    stream: TcpStream,
    buf: Vec<u8>,
    read_len: usize,
    write_len: usize,
}

enum ClientStep {
    Idle,
    Progress,
    PacketDone,
    Closed,
}

impl BenchReport {
    fn bytes_echoed(&self) -> u64 {
        (self.packets * self.payload_bytes * 2) as u64
    }

    fn print(&self, label: &str) {
        let secs = self.elapsed.as_secs_f64();
        let mean_us = secs * 1_000_000.0 / self.packets as f64;
        let mib = self.bytes_echoed() as f64 / (1024.0 * 1024.0);
        println!("{label}");
        println!("  packets: {}", self.packets);
        println!("  payload: {} B", self.payload_bytes);
        println!("  elapsed: {:.6} s", secs);
        println!("  mean roundtrip: {:.3} us", mean_us);
        println!("  echo throughput: {:.2} MiB/s", mib / secs);
    }
}

fn parse_u16(raw: &str, name: &str) -> Result<u16, String> {
    raw.parse::<u16>()
        .map_err(|e| format!("invalid {name} value {raw:?}: {e}"))
        .and_then(|value| {
            if value == 0 {
                Err(format!("{name} must be nonzero"))
            } else {
                Ok(value)
            }
        })
}

fn parse_usize(raw: &str, name: &str) -> Result<usize, String> {
    raw.parse::<usize>()
        .map_err(|e| format!("invalid {name} value {raw:?}: {e}"))
}

fn parse_u64(raw: &str, name: &str) -> Result<u64, String> {
    raw.parse::<u64>()
        .map_err(|e| format!("invalid {name} value {raw:?}: {e}"))
}

fn parse_ipv4(raw: &str, name: &str) -> Result<Ipv4Addr, String> {
    raw.parse::<Ipv4Addr>()
        .map_err(|e| format!("invalid {name} value {raw:?}: {e}"))
}

fn payload(bytes: usize) -> Vec<u8> {
    (0..bytes)
        .map(|i| (i.wrapping_mul(31).wrapping_add(7) & 0xff) as u8)
        .collect()
}

fn echo_once(stream: &mut TcpStream, send: &[u8], recv: &mut [u8]) -> io::Result<()> {
    stream.write_all(send)?;
    stream.read_exact(recv)?;
    if recv != send {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "echo payload mismatch",
        ));
    }
    Ok(())
}

fn connect_and_prime(cfg: &Config, data: &[u8], recv: &mut [u8]) -> Result<TcpStream, String> {
    let deadline = Instant::now() + cfg.connect_timeout;
    let addr = SocketAddr::new(IpAddr::V4(LOOPBACK_IP), cfg.host_port);

    loop {
        let last_error = match TcpStream::connect(addr) {
            Ok(mut stream) => {
                let _ = stream.set_nodelay(true);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                match echo_once(&mut stream, data, recv) {
                    Ok(()) => return Ok(stream),
                    Err(e) => format!("first echo failed: {e}"),
                }
            }
            Err(e) => format!("connect failed: {e}"),
        };

        if Instant::now() >= deadline {
            return Err(format!(
                "host client could not establish an echo stream within {:?}: {last_error}",
                cfg.connect_timeout
            ));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn run_client(cfg: &Config) -> Result<BenchReport, String> {
    let data = payload(cfg.payload_bytes);
    let mut recv = vec![0u8; cfg.payload_bytes];
    let mut stream = connect_and_prime(cfg, &data, &mut recv)?;

    // The priming packet is intentionally part of warmup. This keeps the
    // measured window free of hostfwd listener startup, guest boot, ARP, and
    // the first gateway->guest connect.
    let mut completed = 1usize;
    while completed < cfg.warmup_rounds {
        echo_once(&mut stream, &data, &mut recv).map_err(|e| format!("warmup echo: {e}"))?;
        completed += 1;
    }

    let start = Instant::now();
    for _ in 0..cfg.measured_packets() {
        echo_once(&mut stream, &data, &mut recv).map_err(|e| format!("measured echo: {e}"))?;
        completed += 1;
    }

    debug_assert_eq!(completed, cfg.total_packets());
    Ok(BenchReport {
        elapsed: start.elapsed(),
        packets: cfg.measured_packets(),
        payload_bytes: cfg.payload_bytes,
    })
}

fn run_server(cfg: &Config) -> Result<(), String> {
    let bind = SocketAddr::new(IpAddr::V4(cfg.bind_ip), cfg.port);
    let listener = TcpListener::bind(bind).map_err(|e| format!("bind {bind}: {e}"))?;
    set_nonblocking_fd(&listener, "listener")?;
    let epfd = epoll_create()?;
    epoll_add(epfd, listener.as_raw_fd(), libc::EPOLLIN as u32)?;

    let mut remaining = cfg.packets;
    let mut clients: Vec<Client> = Vec::new();
    let mut events = vec![empty_epoll_event(); 64];

    while remaining > 0 {
        let n = epoll_wait(epfd, &mut events, 1000)?;
        if n == 0 {
            continue;
        }

        for event in events.iter().take(n) {
            let fd = event.u64 as RawFd;
            if fd == listener.as_raw_fd() {
                loop {
                    match listener.accept() {
                        Ok((stream, _peer)) => {
                            let _ = stream.set_nodelay(true);
                            set_nonblocking_fd(&stream, "stream")?;
                            let fd = stream.as_raw_fd();
                            epoll_add(epfd, fd, libc::EPOLLIN as u32)?;
                            clients.push(Client {
                                stream,
                                buf: vec![0u8; cfg.payload_bytes],
                                read_len: 0,
                                write_len: 0,
                            });
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) if transient_stream_error(&e) => break,
                        Err(e) => return Err(format!("accept: {e}")),
                    }
                }
                continue;
            }

            let Some(i) = clients
                .iter()
                .position(|client| client.stream.as_raw_fd() == fd)
            else {
                continue;
            };

            match service_client(&mut clients[i], cfg.payload_bytes)? {
                ClientStep::Idle | ClientStep::Progress => {
                    epoll_mod(epfd, fd, client_interest(&clients[i], cfg.payload_bytes))?;
                }
                ClientStep::PacketDone => {
                    remaining -= 1;
                    if remaining == 0 {
                        break;
                    }
                    epoll_mod(epfd, fd, libc::EPOLLIN as u32)?;
                }
                ClientStep::Closed => {
                    let _ = epoll_del(epfd, fd);
                    clients.swap_remove(i);
                }
            }
        }
    }

    // SAFETY: `epfd` was returned by epoll_create1 and is owned here.
    unsafe {
        libc::close(epfd);
    }
    Ok(())
}

fn service_client(client: &mut Client, payload_bytes: usize) -> Result<ClientStep, String> {
    if client.read_len < payload_bytes {
        match client.stream.read(&mut client.buf[client.read_len..]) {
            Ok(0) => return Ok(ClientStep::Closed),
            Ok(n) => {
                client.read_len += n;
                if client.read_len < payload_bytes {
                    return Ok(ClientStep::Progress);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(ClientStep::Idle),
            Err(e) if transient_stream_error(&e) => return Ok(ClientStep::Closed),
            Err(e) => return Err(format!("read request: {e}")),
        }
    }

    if client.write_len < payload_bytes {
        match client
            .stream
            .write(&client.buf[client.write_len..payload_bytes])
        {
            Ok(0) => return Ok(ClientStep::Idle),
            Ok(n) => {
                client.write_len += n;
                if client.write_len < payload_bytes {
                    return Ok(ClientStep::Progress);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(ClientStep::Idle),
            Err(e) if transient_stream_error(&e) => return Ok(ClientStep::Closed),
            Err(e) => return Err(format!("write echo: {e}")),
        }
    }

    client.read_len = 0;
    client.write_len = 0;
    Ok(ClientStep::PacketDone)
}

fn client_interest(client: &Client, payload_bytes: usize) -> u32 {
    if client.read_len < payload_bytes {
        libc::EPOLLIN as u32
    } else {
        libc::EPOLLOUT as u32
    }
}

fn empty_epoll_event() -> libc::epoll_event {
    libc::epoll_event { events: 0, u64: 0 }
}

fn epoll_create() -> Result<RawFd, String> {
    // SAFETY: epoll_create1 has no pointer arguments; flags=0 is valid.
    let fd = unsafe { libc::epoll_create1(0) };
    if fd < 0 {
        Err(format!("epoll_create1: {}", io::Error::last_os_error()))
    } else {
        Ok(fd)
    }
}

fn epoll_ctl(epfd: RawFd, op: i32, fd: RawFd, events: u32) -> Result<(), String> {
    let mut event = libc::epoll_event {
        events,
        u64: fd as u64,
    };
    // SAFETY: `epfd` and `fd` are live fds owned by this process. `event`
    // points to initialized stack memory for ADD/MOD; DEL ignores it on
    // Linux, and passing a valid pointer is accepted by both host and guest.
    let rc = unsafe { libc::epoll_ctl(epfd, op, fd, &mut event) };
    if rc < 0 {
        Err(format!(
            "epoll_ctl op {op} fd {fd}: {}",
            io::Error::last_os_error()
        ))
    } else {
        Ok(())
    }
}

fn epoll_add(epfd: RawFd, fd: RawFd, events: u32) -> Result<(), String> {
    epoll_ctl(epfd, libc::EPOLL_CTL_ADD, fd, events)
}

fn epoll_mod(epfd: RawFd, fd: RawFd, events: u32) -> Result<(), String> {
    epoll_ctl(epfd, libc::EPOLL_CTL_MOD, fd, events)
}

fn epoll_del(epfd: RawFd, fd: RawFd) -> Result<(), String> {
    epoll_ctl(epfd, libc::EPOLL_CTL_DEL, fd, 0)
}

fn epoll_wait(
    epfd: RawFd,
    events: &mut [libc::epoll_event],
    timeout_ms: i32,
) -> Result<usize, String> {
    // SAFETY: `events` is a valid mutable buffer of `epoll_event`s; epoll_wait
    // writes at most `events.len()` entries into it.
    let n = unsafe { libc::epoll_wait(epfd, events.as_mut_ptr(), events.len() as i32, timeout_ms) };
    if n < 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::Interrupted {
            Ok(0)
        } else {
            Err(format!("epoll_wait: {e}"))
        }
    } else {
        Ok(n as usize)
    }
}

fn set_nonblocking_fd<F: AsRawFd>(fd: &F, label: &str) -> Result<(), String> {
    let raw = fd.as_raw_fd();
    // SAFETY: `raw` is borrowed from a live Rust fd owner; F_GETFL does not
    // mutate memory and returns either the current flags or -1 with errno.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags < 0 {
        return Err(format!(
            "fcntl(F_GETFL) for {label}: {}",
            io::Error::last_os_error()
        ));
    }
    // SAFETY: `raw` is still a valid fd, and F_SETFL with O_NONBLOCK only
    // updates fd status flags in the kernel.
    let rc = unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if rc < 0 {
        return Err(format!(
            "fcntl(F_SETFL O_NONBLOCK) for {label}: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn transient_stream_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::NotConnected
            | io::ErrorKind::BrokenPipe
    )
}

fn spawn_reader<R>(mut reader: R) -> thread::JoinHandle<String>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut out = String::new();
        let _ = reader.read_to_string(&mut out);
        out
    })
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            child.kill()?;
            let _ = child.wait();
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn finish_child(
    mut child: Child,
    stdout: thread::JoinHandle<String>,
    stderr: thread::JoinHandle<String>,
    timeout: Duration,
    label: &str,
) -> Result<(), String> {
    let status = wait_with_timeout(&mut child, timeout)
        .map_err(|e| format!("wait for {label}: {e}"))?
        .ok_or_else(|| format!("{label} did not exit within {timeout:?}"))?;
    let out = stdout.join().unwrap_or_default();
    let err = stderr.join().unwrap_or_default();
    if status.success() {
        return Ok(());
    }
    Err(format!(
        "{label} exited with {status}\nstdout:\n{}\nstderr:\n{}",
        tail(&out),
        tail(&err)
    ))
}

fn tail(s: &str) -> &str {
    const MAX: usize = 4096;
    if s.len() <= MAX {
        s
    } else {
        &s[s.len() - MAX..]
    }
}

fn spawn_host_server(cfg: &Config) -> Result<Child, String> {
    let exe = env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    Command::new(exe)
        .arg("server")
        .arg("--bind-ip")
        .arg(LOOPBACK_IP.to_string())
        .arg("--port")
        .arg(cfg.host_port.to_string())
        .arg("--payload-bytes")
        .arg(cfg.payload_bytes.to_string())
        .arg("--packets")
        .arg(cfg.total_packets().to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn host server: {e}"))
}

fn spawn_vm_server(cfg: &Config) -> Result<Child, String> {
    Command::new(&cfg.vm)
        .arg("run")
        .arg(&cfg.kernel)
        .arg("--share")
        .arg("/")
        .arg("--vcpus")
        .arg(cfg.vcpus.to_string())
        .arg("--run")
        .arg(&cfg.guest_bin)
        .arg("--hostfwd")
        .arg(format!(
            "tcp:127.0.0.1:{}-10.0.2.15:{}",
            cfg.host_port, cfg.guest_port
        ))
        .arg("--")
        .arg("server")
        .arg("--bind-ip")
        .arg(GUEST_IP.to_string())
        .arg("--port")
        .arg(cfg.guest_port.to_string())
        .arg("--payload-bytes")
        .arg(cfg.payload_bytes.to_string())
        .arg("--packets")
        .arg(cfg.total_packets().to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn sumi-vm: {e}"))
}

fn run_with_child(mut child: Child, cfg: &Config, label: &str) -> Result<(), String> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("{label} stdout was not piped"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("{label} stderr was not piped"))?;
    let stdout_handle = spawn_reader(stdout);
    let stderr_handle = spawn_reader(stderr);

    let report = run_client(cfg);
    let finish = finish_child(
        child,
        stdout_handle,
        stderr_handle,
        cfg.child_timeout,
        label,
    );
    match (report, finish) {
        (Ok(report), Ok(())) => {
            report.print(label);
            Ok(())
        }
        (Err(client), Ok(())) => Err(client),
        (Ok(_), Err(child)) => Err(child),
        (Err(client), Err(child)) => Err(format!("{client}\n\n{child}")),
    }
}

fn usage() -> String {
    format!(
        "\
Usage:
  hostfwd_tcp host-baseline [options]
  hostfwd_tcp hostfwd [options]
  hostfwd_tcp server [server options]

Modes:
  host-baseline  Run the echo server as a host child process.
  hostfwd        Run the echo server inside sumi-vm behind --hostfwd.
  server         Echo-server side used by the two harness modes.

Common options:
  --host-port PORT          Host/client TCP port (default {DEFAULT_HOST_PORT})
  --guest-port PORT         Guest TCP port for hostfwd (default {DEFAULT_GUEST_PORT})
  --payload-bytes N         Echo payload bytes per packet (default {DEFAULT_PAYLOAD_BYTES})
  --rounds N                Measured echo packets (default {DEFAULT_ROUNDS})
  --warmup-rounds N         Unmeasured warmup packets, min 1 (default {DEFAULT_WARMUP_ROUNDS})
  --connect-timeout-ms N    Host client startup timeout (default {DEFAULT_CONNECT_TIMEOUT_MS})
  --child-timeout-ms N      Child shutdown timeout (default {DEFAULT_CHILD_TIMEOUT_MS})

hostfwd-only options:
  --vm PATH                 sumi-vm binary (default target/debug/sumi-vm)
  --kernel PATH             kernel ELF (default target/x86_64-unknown-none/release/sumi-kernel)
  --guest-bin PATH          static benchmark ELF visible through --share /
  --vcpus N                 VM vCPUs (default {DEFAULT_VCPUS})

server-only options:
  --bind-ip IP              Server bind IP
  --port PORT               Server bind port
  --packets N               Total packets before exit
"
    )
}

fn run() -> Result<(), String> {
    let mut raw = env::args().skip(1).collect::<Vec<_>>();
    if raw.is_empty() || raw[0] == "--help" || raw[0] == "-h" {
        return Err(usage());
    }
    let mode = raw.remove(0);
    let mut cfg = Config::new();
    let mut args = Args::new(raw);

    match mode.as_str() {
        "host-baseline" => {
            args.parse_common(&mut cfg, false, true)?;
            let child = spawn_host_server(&cfg)?;
            run_with_child(child, &cfg, "host-baseline")
        }
        "hostfwd" => {
            args.parse_common(&mut cfg, false, true)?;
            cfg.ensure_guest_bin()?;
            let child = spawn_vm_server(&cfg)?;
            run_with_child(child, &cfg, "hostfwd")
        }
        "server" => {
            args.parse_common(&mut cfg, true, false)?;
            run_server(&cfg)
        }
        _ => Err(format!("unknown mode {mode:?}\n\n{}", usage())),
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(2);
    }
}
