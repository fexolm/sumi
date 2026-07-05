//! Host userspace gateway: a real smoltcp `Interface` running on the host,
//! bridged to the guest's virtio-net device through `GatewayChannel`'s
//! frame queues (see `devices::virtio_net`). Stands in for a TAP device in
//! an environment with no `CAP_NET_ADMIN` (see `docs/networking-design.md`
//! Phase 2) — no root, no host->guest IRQ.
//!
//! Phase 2a: the gateway answers ARP for its own address (`GATEWAY_IP`) and
//! runs a single TCP echo peer on `10.0.2.2:ECHO_PORT`, proving
//! guest-initiated TCP works end to end over the virtio-net transport.
//! Host->guest port forwarding (`--hostfwd`) is a later phase.
//!
//! This thread never locks `DeviceRegistry` — it only ever touches
//! `GatewayChannel`'s two frame queues, briefly, through the `GatewayDevice`
//! `Device` impl below. Lock order is `DeviceRegistry -> GatewayChannel`,
//! enforced by never importing `DeviceRegistry` here.

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
/// idle, independent of `Interface::poll_delay`'s own estimate.
const MAX_POLL_DELAY: Duration = Duration::from_millis(1);

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

/// Spawn the gateway thread. `chan` is the only channel to the virtio-net
/// backend running inside the vCPU thread's `DeviceRegistry` lock.
pub fn spawn(chan: Arc<GatewayChannel>) -> GatewayHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let join = thread::spawn(move || gateway_main(chan, thread_stop));
    GatewayHandle { join, stop }
}

fn gateway_main(chan: Arc<GatewayChannel>, stop: Arc<AtomicBool>) {
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

    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();
        let _ = iface.poll(now, &mut device, &mut sockets);

        service_echo_peer(&mut sockets, echo_handle);

        let timeout = iface
            .poll_delay(now, &sockets)
            .map(|d| Duration::from_micros(d.total_micros()).min(MAX_POLL_DELAY))
            .unwrap_or(MAX_POLL_DELAY);

        // Park until either the backend notifies us (guest transmitted a
        // frame) or `timeout` elapses (drives smoltcp's own timers and lets
        // us re-check `stop`). A poisoned mutex can only happen if this
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
