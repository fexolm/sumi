//! smoltcp `Interface` + `SocketSet` construction, IP config, the
//! monotonic-clock shim, and the fixed constants Phase 1 needs.

use alloc::vec::Vec;

use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{Loopback, Medium};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};

/// Guest MAC for the Phase 1 Ethernet loopback interface (ARP resolves this
/// address against itself — see `docs/networking-design.md` R2).
const GUEST_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];

/// Ephemeral port range for outbound `connect()`.
pub const EPHEMERAL_LO: u16 = 49152;
pub const EPHEMERAL_HI: u16 = 65535;

/// TCP socket buffer sizes.
pub const TCP_RX: usize = 64 * 1024;
pub const TCP_TX: usize = 64 * 1024;

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

/// Build the Phase 1 loopback stack: an Ethernet `Loopback` device, a
/// smoltcp `Interface` configured with 127.0.0.1/8 (the actual Phase 1
/// traffic path) and 10.0.2.15/24 (reserved for the Phase 2 virtio-net
/// path), and an empty `SocketSet`.
pub fn build() -> (Interface, Loopback, SocketSet<'static>) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_configures_loopback_address() {
        let (iface, _device, _sockets) = build();
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
