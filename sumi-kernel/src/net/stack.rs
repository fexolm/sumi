//! smoltcp `Interface` + `SocketSet` construction, IP config, the
//! monotonic-clock shim, and the fixed constants Phase 1 needs.

use alloc::vec::Vec;

use smoltcp::iface::{Config, Interface, SocketSet};
#[cfg(test)]
use smoltcp::phy::{Loopback, Medium};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, Ipv4Address};

use super::device::VirtioNetDevice;

/// Guest MAC for the Phase 1 Ethernet loopback interface (ARP resolves this
/// address against itself — see `docs/networking-design.md` R2).
#[cfg(test)]
const GUEST_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];

/// Ephemeral port range for outbound `connect()`.
pub const EPHEMERAL_LO: u16 = 49152;
pub const EPHEMERAL_HI: u16 = 65535;

/// TCP socket buffer sizes.
pub const TCP_RX: usize = 256 * 1024;
pub const TCP_TX: usize = 256 * 1024;

/// smoltcp monotonic clock shim: guest monotonic nanoseconds -> smoltcp
/// microseconds. `Instant` is a signed microsecond count, which comfortably
/// holds the guest uptime.
pub fn now() -> Instant {
    Instant::from_micros((crate::time::monotonic_ns() / 1_000) as i64)
}

/// Derive a 64-bit seed for smoltcp's TCP sequence-number/ephemeral-port
/// randomness from the boot-time RNG seed. Falls back to the current clock
/// if `RNG_SEED` has not been published yet (should not happen post-boot;
/// `net::init()` runs after `read_boot_info()`).
fn seed_from_rng() -> u64 {
    match crate::RNG_SEED.get() {
        Some(seed) => u64::from_le_bytes(seed[0..8].try_into().unwrap()),
        None => crate::time::monotonic_ns(),
    }
}

/// Build the Phase 1 loopback stack (host unit tests only — production
/// runs over `build_virtio` below): an Ethernet `Loopback` device, a
/// smoltcp `Interface` configured with 127.0.0.1/8 (the actual Phase 1
/// traffic path) and 10.0.2.15/24 (reserved for the Phase 2 virtio-net
/// path), and an empty `SocketSet`.
#[cfg(test)]
pub fn build_loopback() -> (Interface, Loopback, SocketSet<'static>) {
    let mut device = Loopback::new(Medium::Ethernet);
    let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(GUEST_MAC)));
    config.random_seed = seed_from_rng();

    let mut iface = Interface::new(config, &mut device, now());
    iface.update_ip_addrs(|addrs| {
        let _ = addrs.push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8));
        let _ = addrs.push(IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24));
    });

    let sockets = SocketSet::new(Vec::new());
    (iface, device, sockets)
}

/// Build the Phase 2 virtio-net stack: a smoltcp `Interface` over the
/// already-initialized `VirtioNetDevice`, configured with the guest's
/// static `10.0.2.15/24` address and a default route via the gateway
/// (`10.0.2.2`), plus `127.0.0.1/8` so Phase 1's self-connect pattern
/// (`tcp_epoll_loopback`) keeps working: `VirtioNetDevice`'s software
/// self-loopback queue (see its `loopback` field doc comment) makes this
/// address self-deliver exactly like it did over smoltcp's `Loopback`.
pub fn build_virtio(device: &mut VirtioNetDevice) -> (Interface, SocketSet<'static>) {
    let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(device.mac())));
    config.random_seed = seed_from_rng();

    let mut iface = Interface::new(config, device, now());
    iface.update_ip_addrs(|addrs| {
        let _ = addrs.push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8));
        let guest_ip = sumi_abi::net::GUEST_IP;
        let _ = addrs.push(IpCidr::new(
            IpAddress::v4(guest_ip[0], guest_ip[1], guest_ip[2], guest_ip[3]),
            sumi_abi::net::GUEST_PREFIX,
        ));
    });
    let gateway_ip = sumi_abi::net::GATEWAY_IP;
    let _ = iface.routes_mut().add_default_ipv4_route(Ipv4Address::new(
        gateway_ip[0],
        gateway_ip[1],
        gateway_ip[2],
        gateway_ip[3],
    ));

    let sockets = SocketSet::new(Vec::new());
    (iface, sockets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_configures_loopback_address() {
        let (iface, _device, _sockets) = build_loopback();
        let has_loopback = iface
            .ip_addrs()
            .iter()
            .any(|cidr| cidr.address() == IpAddress::v4(127, 0, 0, 1) && cidr.prefix_len() == 8);
        assert!(has_loopback, "127.0.0.1/8 must be configured");
    }

    #[test]
    fn now_is_monotonic() {
        let a = now();
        let b = now();
        assert!(b >= a);
    }
}
