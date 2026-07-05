//! Host userspace gateway: a real smoltcp `Interface` running on the host,
//! bridged to the guest's virtio-net device through `GatewayChannel`'s
//! frame queues (see `devices::virtio_net`). Stands in for a TAP device in
//! an environment with no `CAP_NET_ADMIN` (see `docs/networking-design.md`
//! Phase 2) — no root, no host->guest IRQ.
//!
//! Phase 2a: the gateway answers ARP for its own address (`GATEWAY_IP`) and
//! runs a single TCP echo peer on `10.0.2.2:ECHO_PORT`, proving
//! guest-initiated TCP works end to end over the virtio-net transport.
//!
//! Phase 2b: the gateway also honors `--hostfwd tcp:HOST_IP:HOST_PORT-
//! GUEST_IP:GUEST_PORT` rules (`HostForward`): for each rule it binds a
//! non-blocking `std::net::TcpListener` on the host and, per accepted
//! connection, opens a smoltcp TCP socket into the guest and bridges bytes
//! both ways (see `Bridge`/`service_bridge`). This is what lets a real host
//! client reach a guest server (e.g. mysqld) with no root and no IRQ.
//!
//! This thread never locks `DeviceRegistry` — it only ever touches
//! `GatewayChannel`'s two frame queues, briefly, through the `GatewayDevice`
//! `Device` impl below. Lock order is `DeviceRegistry -> GatewayChannel`,
//! enforced by never importing `DeviceRegistry` here.

use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpListenEndpoint, Ipv4Address};

use crate::devices::virtio_net::GatewayChannel;

/// Port the 2a echo peer listens on at `10.0.2.2` (see
/// `sumi_abi::net::GATEWAY_IP`). Must match the literal the integration
/// test program (`sumi-integration-tests/data/syscalls/tcp_virtio_echo.rs`)
/// connects to.
pub const ECHO_PORT: u16 = 7777;

const TCP_BUF_SIZE: usize = 64 * 1024;

/// Upper bound on how long a gateway loop iteration sleeps with nothing to
/// do. Bounds how quickly TCP timers (retransmit, delayed ACK) fire while
/// idle, and how quickly a new `--hostfwd` `accept()` is noticed.
const MAX_POLL_DELAY: Duration = Duration::from_millis(1);

/// Ephemeral source port range the gateway uses for its own outbound
/// connections into the guest (one per `--hostfwd` accept). Mirrors the
/// guest's own `net::stack::EPHEMERAL_LO/HI` range; kept as an independent
/// local constant since the gateway is a separate host binary.
const EPHEMERAL_LO: u16 = 49152;
const EPHEMERAL_HI: u16 = 65535;

/// A parsed `--hostfwd tcp:HOST_IP:HOST_PORT-GUEST_IP:GUEST_PORT` rule:
/// bind `host_ip:host_port` on the host and forward each accepted
/// connection to `guest_ip:guest_port` inside the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostForward {
    pub host_ip: Ipv4Addr,
    pub host_port: u16,
    pub guest_ip: Ipv4Address,
    pub guest_port: u16,
}

/// Why a `--hostfwd` argument failed to parse. Never panics — every
/// malformed input reaches here instead.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid --hostfwd {input:?}: {reason}")]
pub struct HostForwardParseError {
    input: String,
    reason: &'static str,
}

impl FromStr for HostForward {
    type Err = HostForwardParseError;

    /// Format: `tcp:HOST_IP:HOST_PORT-GUEST_IP:GUEST_PORT`, e.g.
    /// `tcp:127.0.0.1:3307-10.0.2.15:3306`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let fail = |reason: &'static str| HostForwardParseError {
            input: s.to_string(),
            reason,
        };

        let rest = s.strip_prefix("tcp:").ok_or_else(|| fail("must start with \"tcp:\""))?;
        let (host_part, guest_part) = rest
            .split_once('-')
            .ok_or_else(|| fail("missing '-' between host and guest endpoints"))?;
        let (host_ip_str, host_port_str) = host_part
            .rsplit_once(':')
            .ok_or_else(|| fail("host endpoint must be IP:PORT"))?;
        let (guest_ip_str, guest_port_str) = guest_part
            .rsplit_once(':')
            .ok_or_else(|| fail("guest endpoint must be IP:PORT"))?;

        let host_ip: Ipv4Addr = host_ip_str.parse().map_err(|_| fail("invalid host IPv4 address"))?;
        let host_port: u16 = host_port_str.parse().map_err(|_| fail("invalid host port"))?;
        let guest_ip: Ipv4Addr = guest_ip_str.parse().map_err(|_| fail("invalid guest IPv4 address"))?;
        let guest_port: u16 = guest_port_str.parse().map_err(|_| fail("invalid guest port"))?;

        if host_port == 0 || guest_port == 0 {
            return Err(fail("port must be nonzero"));
        }

        let o = guest_ip.octets();
        Ok(HostForward {
            host_ip,
            host_port,
            guest_ip: Ipv4Address::new(o[0], o[1], o[2], o[3]),
            guest_port,
        })
    }
}

/// Handle to the running gateway thread.
pub struct GatewayHandle {
    join: JoinHandle<()>,
    stop: Arc<AtomicBool>,
}

impl GatewayHandle {
    /// Signal the gateway thread to exit and wait for it. The thread is
    /// never parked longer than `MAX_POLL_DELAY`, so this returns promptly
    /// after the store below becomes visible.
    pub fn shutdown(self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.join.join();
    }
}

/// smoltcp `Device` over `GatewayChannel`'s frame queues — the mirror image
/// of the guest's `VirtioNetDevice`: `receive` pops what the guest
/// transmitted, `transmit` pushes what the guest will receive next.
struct GatewayDevice {
    chan: Arc<GatewayChannel>,
}

struct GatewayRxToken {
    frame: Vec<u8>,
}

struct GatewayTxToken<'a> {
    chan: &'a GatewayChannel,
}

impl Device for GatewayDevice {
    type RxToken<'a> = GatewayRxToken;
    type TxToken<'a> = GatewayTxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let frame = self.chan.guest_to_host.lock().unwrap().pop_front()?;
        Some((
            GatewayRxToken { frame },
            GatewayTxToken { chan: &self.chan },
        ))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(GatewayTxToken { chan: &self.chan })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1514;
        caps.medium = Medium::Ethernet;
        caps
    }
}

impl smoltcp::phy::RxToken for GatewayRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.frame)
    }
}

impl<'a> smoltcp::phy::TxToken for GatewayTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let result = f(&mut buf);
        self.chan.host_to_guest.lock().unwrap().push_back(buf);
        result
    }
}

fn new_tcp_socket<'a>() -> tcp::Socket<'a> {
    let rx = tcp::SocketBuffer::new(vec![0u8; TCP_BUF_SIZE]);
    let tx = tcp::SocketBuffer::new(vec![0u8; TCP_BUF_SIZE]);
    tcp::Socket::new(rx, tx)
}

fn ipv4(bytes: [u8; 4]) -> Ipv4Address {
    Ipv4Address::new(bytes[0], bytes[1], bytes[2], bytes[3])
}

/// States in which the peer has closed its send half — mirrors
/// `sumi_kernel::net::socket::peer_closed` (duplicated rather than shared:
/// the guest and gateway are different crates/binaries).
fn peer_closed(state: tcp::State) -> bool {
    matches!(
        state,
        tcp::State::CloseWait
            | tcp::State::LastAck
            | tcp::State::Closing
            | tcp::State::Closed
            | tcp::State::TimeWait
    )
}

/// One `--hostfwd` rule bound to a live, non-blocking host listener.
struct BoundForward {
    fwd: HostForward,
    listener: TcpListener,
}

/// Buffered bytes flowing one direction of a `Bridge`, tracked with an
/// offset so a partial `write`/`send_slice` never has to reallocate or
/// drop the unwritten tail — the next `service_bridge` call just retries
/// `remaining()`.
#[derive(Default)]
struct PendingBuf {
    data: Vec<u8>,
    off: usize,
}

impl PendingBuf {
    fn is_empty(&self) -> bool {
        self.off >= self.data.len()
    }

    /// Replace the contents. Only called when `is_empty()` — the caller
    /// only pulls more input once the previous chunk fully drained.
    fn set(&mut self, bytes: &[u8]) {
        self.data.clear();
        self.data.extend_from_slice(bytes);
        self.off = 0;
    }

    fn remaining(&self) -> &[u8] {
        &self.data[self.off..]
    }

    fn advance(&mut self, n: usize) {
        self.off += n;
    }
}

/// One active `--hostfwd` connection: a non-blocking host `TcpStream`
/// bridged to a smoltcp socket connected into the guest.
struct Bridge {
    stream: TcpStream,
    handle: SocketHandle,
    /// Guest -> host: popped from the smoltcp socket, waiting on `stream`.
    to_host: PendingBuf,
    /// Host -> guest: read from `stream`, waiting on the smoltcp socket.
    to_guest: PendingBuf,
    /// `stream`'s read half hit EOF/error; the guest socket has been
    /// half-closed (`close()`) as a result. Set at most once.
    host_read_closed: bool,
    /// The guest sent FIN (`peer_closed`); `stream`'s write half has been
    /// shut down as a result. Set at most once.
    guest_close_forwarded: bool,
}

/// Spawn the gateway thread. `chan` is the only channel to the virtio-net
/// backend running inside the vCPU thread's `DeviceRegistry` lock. `fwds`
/// is the (possibly empty) set of `--hostfwd` rules to bind and forward.
pub fn spawn(chan: Arc<GatewayChannel>, fwds: Vec<HostForward>) -> GatewayHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let join = thread::spawn(move || gateway_main(chan, thread_stop, fwds));
    GatewayHandle { join, stop }
}

fn gateway_main(chan: Arc<GatewayChannel>, stop: Arc<AtomicBool>, fwds: Vec<HostForward>) {
    let mut device = GatewayDevice {
        chan: Arc::clone(&chan),
    };

    let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(
        sumi_abi::net::GATEWAY_MAC,
    )));
    config.random_seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

    let mut iface = Interface::new(config, &mut device, Instant::now());
    iface.update_ip_addrs(|addrs| {
        let _ = addrs.push(IpCidr::new(
            IpAddress::from(ipv4(sumi_abi::net::GATEWAY_IP)),
            sumi_abi::net::GUEST_PREFIX,
        ));
    });

    let mut sockets: SocketSet<'static> = SocketSet::new(Vec::new());
    let mut echo_socket = new_tcp_socket();
    let _ = echo_socket.listen(IpListenEndpoint::from(ECHO_PORT));
    let echo_handle = sockets.add(echo_socket);

    let mut listeners = Vec::with_capacity(fwds.len());
    for fwd in fwds {
        match TcpListener::bind((fwd.host_ip, fwd.host_port)) {
            Ok(listener) => match listener.set_nonblocking(true) {
                Ok(()) => listeners.push(BoundForward { fwd, listener }),
                Err(e) => eprintln!(
                    "[gateway] --hostfwd {}:{}: set_nonblocking failed: {e}",
                    fwd.host_ip, fwd.host_port
                ),
            },
            Err(e) => eprintln!(
                "[gateway] --hostfwd bind {}:{} failed: {e}",
                fwd.host_ip, fwd.host_port
            ),
        }
    }

    let mut next_ephemeral = EPHEMERAL_LO;
    let mut bridges: Vec<Bridge> = Vec::new();

    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();
        let _ = iface.poll(now, &mut device, &mut sockets);

        service_echo_peer(&mut sockets, echo_handle);

        accept_new_connections(
            &listeners,
            &mut iface,
            &mut sockets,
            &mut next_ephemeral,
            &mut bridges,
        );

        bridges.retain_mut(|bridge| {
            let keep = service_bridge(&mut sockets, bridge);
            if !keep {
                sockets.remove(bridge.handle);
            }
            keep
        });

        let timeout = iface
            .poll_delay(now, &sockets)
            .map(|d| Duration::from_micros(d.total_micros()).min(MAX_POLL_DELAY))
            .unwrap_or(MAX_POLL_DELAY);

        // Park until either the backend notifies us (guest transmitted a
        // frame) or `timeout` elapses (drives smoltcp's own timers, lets a
        // new `--hostfwd` accept get noticed promptly, and lets us
        // re-check `stop`). A poisoned mutex can only happen if this
        // thread itself panicked, so unwrap() matches the rest of the file.
        let mut ready = chan.tx_ready.lock().unwrap();
        if !*ready {
            let (guard, _) = chan.signal.wait_timeout(ready, timeout).unwrap();
            ready = guard;
        }
        *ready = false;
    }
}

/// Echo whatever bytes are pending, and re-arm the listener once the
/// current connection fully closes — mirrors the accept-refill pattern the
/// guest's own listener pool uses (see `sumi-kernel/src/net/socket.rs`).
fn service_echo_peer(sockets: &mut SocketSet<'static>, handle: SocketHandle) {
    let socket = sockets.get_mut::<tcp::Socket>(handle);
    while socket.can_recv() {
        let mut buf = [0u8; 4096];
        match socket.recv_slice(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let _ = socket.send_slice(&buf[..n]);
            }
            Err(_) => break,
        }
    }
    if !socket.is_open() {
        let _ = socket.listen(IpListenEndpoint::from(ECHO_PORT));
    }
}

/// Drain pending `accept()`s on every `--hostfwd` listener. For each one,
/// open a smoltcp socket connected into the guest and start a `Bridge`.
/// Never panics: a bind/connect failure is logged and the host connection
/// is simply dropped (the client observes a reset), never crashing the
/// gateway thread.
fn accept_new_connections(
    listeners: &[BoundForward],
    iface: &mut Interface,
    sockets: &mut SocketSet<'static>,
    next_ephemeral: &mut u16,
    bridges: &mut Vec<Bridge>,
) {
    for bound in listeners {
        loop {
            let (stream, _peer) = match bound.listener.accept() {
                Ok(pair) => pair,
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => {
                    eprintln!(
                        "[gateway] --hostfwd accept on {}:{} failed: {e}",
                        bound.fwd.host_ip, bound.fwd.host_port
                    );
                    break;
                }
            };
            if let Err(e) = stream.set_nonblocking(true) {
                eprintln!("[gateway] --hostfwd: set_nonblocking on accepted stream failed: {e}");
                continue;
            }

            let local_port = *next_ephemeral;
            *next_ephemeral = if local_port == EPHEMERAL_HI {
                EPHEMERAL_LO
            } else {
                local_port + 1
            };

            let mut sock = new_tcp_socket();
            match sock.connect(
                iface.context(),
                (IpAddress::from(bound.fwd.guest_ip), bound.fwd.guest_port),
                local_port,
            ) {
                Ok(()) => {
                    let handle = sockets.add(sock);
                    bridges.push(Bridge {
                        stream,
                        handle,
                        to_host: PendingBuf::default(),
                        to_guest: PendingBuf::default(),
                        host_read_closed: false,
                        guest_close_forwarded: false,
                    });
                }
                Err(e) => {
                    eprintln!(
                        "[gateway] --hostfwd connect to guest {}:{} failed: {e:?}",
                        bound.fwd.guest_ip, bound.fwd.guest_port
                    );
                    // `stream` drops here; the host client observes a reset.
                }
            }
        }
    }
}

/// Shuttle bytes for one bridge in both directions, honoring backpressure
/// (a partially-consumed side is retried next call, never dropped) and
/// half-close in both directions (host EOF -> `socket.close()`; guest FIN
/// -> `stream.shutdown(Write)`). Returns `false` once both directions are
/// fully closed and flushed — the caller then removes the smoltcp socket.
fn service_bridge(sockets: &mut SocketSet<'static>, bridge: &mut Bridge) -> bool {
    let socket = sockets.get_mut::<tcp::Socket>(bridge.handle);

    // Guest -> host.
    if bridge.to_host.is_empty() && socket.can_recv() {
        let mut buf = [0u8; 4096];
        if let Ok(n) = socket.recv_slice(&mut buf)
            && n > 0
        {
            bridge.to_host.set(&buf[..n]);
        }
    }
    if !bridge.to_host.is_empty() {
        match bridge.stream.write(bridge.to_host.remaining()) {
            Ok(n) if n > 0 => bridge.to_host.advance(n),
            Ok(_) => {}
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => {}
            // A hard write error (broken pipe, reset, ...) means the host
            // peer is gone; tear the bridge down outright.
            Err(_) => return false,
        }
    }

    // Host -> guest. Stop pulling more from `stream` once we've already
    // observed its EOF (`host_read_closed`) — there is nothing left to
    // read and the guest socket has already been half-closed below.
    if bridge.to_guest.is_empty() && !bridge.host_read_closed {
        let mut buf = [0u8; 4096];
        match bridge.stream.read(&mut buf) {
            Ok(0) => {
                bridge.host_read_closed = true;
                socket.close(); // half-close: FIN to the guest, keep receiving.
            }
            Ok(n) => bridge.to_guest.set(&buf[..n]),
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(_) => {
                bridge.host_read_closed = true;
                socket.close();
            }
        }
    }
    if !bridge.to_guest.is_empty()
        && let Ok(n) = socket.send_slice(bridge.to_guest.remaining())
        && n > 0
    {
        bridge.to_guest.advance(n);
    }

    // Guest closed its send half -> mirror it as a write-shutdown to the
    // host (EOF on the host's next `read()`), once.
    if !bridge.guest_close_forwarded && peer_closed(socket.state()) {
        bridge.guest_close_forwarded = true;
        let _ = bridge.stream.shutdown(Shutdown::Write);
    }

    let guest_done = !socket.is_active() && bridge.to_guest.is_empty();
    let host_done = bridge.host_read_closed && bridge.to_host.is_empty();
    !(guest_done && host_done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_hostfwd() {
        let fwd: HostForward = "tcp:127.0.0.1:3307-10.0.2.15:3306".parse().unwrap();
        assert_eq!(fwd.host_ip, Ipv4Addr::new(127, 0, 0, 1));
        assert_eq!(fwd.host_port, 3307);
        assert_eq!(fwd.guest_ip, Ipv4Address::new(10, 0, 2, 15));
        assert_eq!(fwd.guest_port, 3306);
    }

    #[test]
    fn rejects_missing_scheme() {
        let err = "127.0.0.1:80-10.0.2.15:80".parse::<HostForward>().unwrap_err();
        assert!(err.to_string().contains("tcp:"));
    }

    #[test]
    fn rejects_missing_dash() {
        assert!("tcp:127.0.0.1:80_10.0.2.15:80".parse::<HostForward>().is_err());
    }

    #[test]
    fn rejects_missing_port() {
        assert!("tcp:127.0.0.1-10.0.2.15:80".parse::<HostForward>().is_err());
        assert!("tcp:127.0.0.1:80-10.0.2.15".parse::<HostForward>().is_err());
    }

    #[test]
    fn rejects_invalid_ip() {
        assert!("tcp:not-an-ip:80-10.0.2.15:80".parse::<HostForward>().is_err());
    }

    #[test]
    fn rejects_non_numeric_port() {
        assert!("tcp:127.0.0.1:abc-10.0.2.15:80".parse::<HostForward>().is_err());
    }

    #[test]
    fn rejects_zero_ports() {
        assert!("tcp:127.0.0.1:0-10.0.2.15:80".parse::<HostForward>().is_err());
        assert!("tcp:127.0.0.1:80-10.0.2.15:0".parse::<HostForward>().is_err());
    }

    #[test]
    fn error_message_is_descriptive() {
        let err = "tcp:bad-10.0.2.15:80".parse::<HostForward>().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("tcp:bad-10.0.2.15:80"), "{msg}");
    }
}
