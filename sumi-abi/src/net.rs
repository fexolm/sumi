//! Fixed network parameters shared between the guest virtio-net driver and
//! the host userspace gateway (see `docs/networking-design.md` Phase 2).

/// Guest static IPv4 address, `10.0.2.15/24` — matches the QEMU user-net
/// ("slirp") convention this design mirrors.
pub const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
pub const GUEST_PREFIX: u8 = 24;

/// Host gateway IPv4 address, also the guest's default route.
pub const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];

/// Locally-administered Ethernet addresses (the `02:` prefix's low bit
/// pattern marks them as such) for the two ends of the virtio-net link.
pub const GUEST_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x15];
pub const GATEWAY_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];
