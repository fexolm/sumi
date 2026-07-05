//! VirtIO split virtqueue types (virtio spec v1.2, section 2.7).

/// Number of descriptors per queue.
pub const QUEUE_SIZE: u16 = 256;

/// Descriptor flags.
pub const VIRTQ_DESC_F_NEXT: u16 = 1;
pub const VIRTQ_DESC_F_WRITE: u16 = 2;

/// Available ring flags.
pub const VIRTQ_AVAIL_F_NO_INTERRUPT: u16 = 1;

/// VirtIO device status bits.
pub const VIRTIO_STATUS_ACKNOWLEDGE: u32 = 1;
pub const VIRTIO_STATUS_DRIVER: u32 = 2;
pub const VIRTIO_STATUS_DRIVER_OK: u32 = 4;
pub const VIRTIO_STATUS_FEATURES_OK: u32 = 8;

/// VirtIO MMIO register offsets.
pub const VIRTIO_MMIO_MAGIC: usize = 0x000;
pub const VIRTIO_MMIO_VERSION: usize = 0x004;
pub const VIRTIO_MMIO_DEVICE_ID: usize = 0x008;
pub const VIRTIO_MMIO_VENDOR_ID: usize = 0x00C;
pub const VIRTIO_MMIO_DEVICE_FEATURES: usize = 0x010;
pub const VIRTIO_MMIO_DEVICE_FEATURES_SEL: usize = 0x014;
pub const VIRTIO_MMIO_DRIVER_FEATURES: usize = 0x020;
pub const VIRTIO_MMIO_DRIVER_FEATURES_SEL: usize = 0x024;
pub const VIRTIO_MMIO_QUEUE_SEL: usize = 0x030;
pub const VIRTIO_MMIO_QUEUE_NUM_MAX: usize = 0x034;
pub const VIRTIO_MMIO_QUEUE_NUM: usize = 0x038;
pub const VIRTIO_MMIO_QUEUE_READY: usize = 0x044;
pub const VIRTIO_MMIO_QUEUE_NOTIFY: usize = 0x050;
pub const VIRTIO_MMIO_INTERRUPT_STATUS: usize = 0x060;
pub const VIRTIO_MMIO_INTERRUPT_ACK: usize = 0x064;
pub const VIRTIO_MMIO_STATUS: usize = 0x070;
pub const VIRTIO_MMIO_QUEUE_DESC_LOW: usize = 0x080;
pub const VIRTIO_MMIO_QUEUE_DESC_HIGH: usize = 0x084;
pub const VIRTIO_MMIO_QUEUE_AVAIL_LOW: usize = 0x090;
pub const VIRTIO_MMIO_QUEUE_AVAIL_HIGH: usize = 0x094;
pub const VIRTIO_MMIO_QUEUE_USED_LOW: usize = 0x0A0;
pub const VIRTIO_MMIO_QUEUE_USED_HIGH: usize = 0x0A4;
pub const VIRTIO_MMIO_CONFIG: usize = 0x100;

/// Expected magic value for VirtIO MMIO devices.
pub const VIRTIO_MMIO_MAGIC_VALUE: u32 = 0x74726976;

/// VirtIO device ID for filesystem.
pub const VIRTIO_DEVICE_FS: u32 = 26;

/// VirtIO device ID for console.
pub const VIRTIO_DEVICE_CONSOLE: u32 = 3;

/// VirtIO device ID for network cards.
pub const VIRTIO_DEVICE_NET: u32 = 1;

/// Device offers a permanent MAC address in config space (`VirtioNetHdr`'s
/// wire format is independent of this bit; it only gates whether `mac` in
/// the device config region is meaningful).
pub const VIRTIO_NET_F_MAC: u64 = 1 << 5;
/// Device supports merging multiple RX buffers into one packet. Not offered
/// by sumi's backend — the driver always uses one fixed-size RX buffer per
/// packet, so the 12-byte `virtio_net_hdr` header is always exactly 12
/// bytes (`num_buffers` is present but unused).
pub const VIRTIO_NET_F_MRG_RXBUF: u64 = 1 << 15;
/// Bit 32 of the feature space: negotiate the VirtIO 1.0+ ("modern") device
/// model. Required so both sides agree the header is the fixed 12-byte
/// `VirtioNetHdr` (legacy virtio-net without VERSION_1 can omit
/// `num_buffers` when MRG_RXBUF is off, shrinking the header to 10 bytes).
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;

/// Fixed size of the `virtio_net_hdr` prepended to every guest<->host
/// Ethernet frame (VERSION_1 device model — see `VIRTIO_F_VERSION_1`).
pub const VIRTIO_NET_HDR_LEN: usize = 12;

/// `struct virtio_net_hdr` (virtio spec v1.2, section 5.1.6.1), VERSION_1
/// layout (always 12 bytes, regardless of `VIRTIO_NET_F_MRG_RXBUF`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct VirtioNetHdr {
    pub flags: u8,
    pub gso_type: u8,
    pub hdr_len: u16,
    pub gso_size: u16,
    pub csum_start: u16,
    pub csum_offset: u16,
    pub num_buffers: u16,
}

const _: () = assert!(
    core::mem::size_of::<VirtioNetHdr>() == VIRTIO_NET_HDR_LEN,
    "VirtioNetHdr must match the wire format's fixed 12-byte VERSION_1 header",
);

/// Vendor ID for sumi.
pub const SUMI_VENDOR_ID: u32 = 0x554D4953;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct VirtqDesc {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct VirtqUsedElem {
    pub id: u32,
    pub len: u32,
}

#[repr(C)]
pub struct VirtqAvail {
    pub flags: u16,
    pub idx: u16,
    pub ring: [u16; QUEUE_SIZE as usize],
}

#[repr(C)]
pub struct VirtqUsed {
    pub flags: u16,
    pub idx: u16,
    pub ring: [VirtqUsedElem; QUEUE_SIZE as usize],
}
