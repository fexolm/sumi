//! `VirtioNetDevice`: smoltcp `Device` over the virtio-net RX/TX
//! virtqueues (see `docs/networking-design.md` Phase 2 and the driver init
//! sequence in `drivers::virtio::console`, which this mirrors).
//!
//! Every frame crossing the guest<->host boundary carries a fixed 12-byte
//! `virtio_net_hdr` (VERSION_1 — see `sumi_abi::virtio::VirtioNetHdr`): it
//! is stripped on receive and prepended (zeroed) on transmit.
//!
//! RX is a *pull* design: the host only fills posted RX buffers while
//! servicing a `QUEUE_NOTIFY` MMIO write, and there is no host->guest IRQ
//! (see the module doc comment in `net::mod` and the design doc). The guest
//! therefore kicks the RX queue once per `poll_and_wake` via `PollDevice`,
//! not once per `receive()` call — `receive()` only drains what the host
//! already deposited.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use smoltcp::phy::{
    Device, DeviceCapabilities, Medium, RxToken as SmolRxToken, TxToken as SmolTxToken,
};
use smoltcp::time::Instant;

use sumi_abi::{
    address::{DirectMap, PhysicalAddr, VirtualAddr},
    arch::layout::VIRTIO_NET_MMIO,
    virtio::*,
};

use crate::drivers::virtio::{mmio, virtqueue::Virtqueue};
use crate::memory::alloc::kmalloc::KernelAllocator;

const RX_COUNT: usize = 64;
const RX_BUF_SIZE: usize = 2048;
const TX_BUF_SIZE: usize = 2048;
const ETH_HDR_LEN: usize = 14;
const ETH_DST_OFF: usize = 0;
const ETH_TYPE_OFF: usize = 12;
const ETH_TYPE_IPV4: u16 = 0x0800;
const ETH_TYPE_ARP: u16 = 0x0806;
const ETH_BROADCAST: [u8; 6] = [0xff; 6];
const IPV4_DST_OFF: usize = ETH_HDR_LEN + 16;
const ARP_TARGET_PROTO_OFF: usize = ETH_HDR_LEN + 24;
const LOOPBACK_NET: u8 = 127;

/// Guest-side virtio-net driver. Owns two virtqueues (0 = RX, 1 = TX), a
/// contiguous RX buffer pool, and a single reused TX buffer + descriptor
/// (safe because `VirtioTxToken` borrows `&mut VirtioNetDevice` and
/// `consume()` spins for the TX completion synchronously before returning).
pub struct VirtioNetDevice {
    rxq: Virtqueue,
    txq: Virtqueue,
    mmio_base: VirtualAddr,
    /// Descriptor indices posted for the RX pool, used only to validate
    /// completions from the host against a known set (see `receive()`).
    rx_desc: [u16; RX_COUNT],
    tx_desc: u16,
    tx_buf: PhysicalAddr,
    /// `direct_map_offset + physical_addr == virtual_addr`, cached at init
    /// so `receive`/`transmit` don't need to carry a `DirectMap` reference.
    dm_offset: usize,
    mac: [u8; 6],
    /// Software self-loopback queue: every transmitted frame is copied
    /// here in addition to being sent out over the TX virtqueue, and
    /// `receive()` drains it ahead of the RX virtqueue.
    ///
    /// This has no counterpart in the plan's "drop 127.0.0.1 in
    /// production, single Ethernet iface doesn't self-deliver" design —
    /// it exists solely so guest-local traffic (127.0.0.1, ARP-to-self)
    /// keeps working exactly as it did over smoltcp's `Loopback` in Phase
    /// 1, which `tcp_epoll_loopback` (an existing, must-not-regress
    /// integration test) exercises. It is safe unconditionally: smoltcp's
    /// own ingress path drops any frame whose destination address the
    /// interface does not own (`Interface::has_ip_addr`), so looping back
    /// real gateway-bound traffic (10.0.2.0/24) is inert — those frames
    /// are silently discarded on the loop side and still genuinely
    /// delivered on the real virtqueue side.
    loopback: VecDeque<Vec<u8>>,
}

impl VirtioNetDevice {
    /// Probe and initialize the virtio-net device. Returns `None` if it is
    /// absent, or if the host does not offer the exact feature set this
    /// driver understands (`VIRTIO_F_VERSION_1 | VIRTIO_NET_F_MAC`, and
    /// nothing else — in particular not `VIRTIO_NET_F_MRG_RXBUF`, which
    /// this driver never negotiates).
    pub fn init<DM: DirectMap>(kalloc: &KernelAllocator<DM>) -> Option<Self> {
        let dm = kalloc.direct_map();
        let dm_offset = dm.p2v(PhysicalAddr::new(0)).as_usize();
        let mmio_base = VIRTIO_NET_MMIO.to_virtual(dm);

        // SAFETY: mmio_base is the virtual address of the virtio-net MMIO
        // register region, mapped through the direct map (mirrors
        // drivers::virtio::console's probe of its own fixed MMIO address).
        unsafe {
            if mmio::read32(mmio_base, VIRTIO_MMIO_MAGIC) != VIRTIO_MMIO_MAGIC_VALUE {
                return None;
            }
            if mmio::read32(mmio_base, VIRTIO_MMIO_VERSION) != 2 {
                return None;
            }
            if mmio::read32(mmio_base, VIRTIO_MMIO_DEVICE_ID) != VIRTIO_DEVICE_NET {
                return None;
            }

            // Reset, then Acknowledge + Driver.
            mmio::write32(mmio_base, VIRTIO_MMIO_STATUS, 0);
            mmio::write32(mmio_base, VIRTIO_MMIO_STATUS, VIRTIO_STATUS_ACKNOWLEDGE);
            mmio::write32(
                mmio_base,
                VIRTIO_MMIO_STATUS,
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER,
            );

            // Negotiate exactly VERSION_1 | MAC. Reject the device if it
            // doesn't offer both (the fixed 12-byte header assumption
            // requires VERSION_1; MAC lets us learn our own address).
            mmio::write32(mmio_base, VIRTIO_MMIO_DEVICE_FEATURES_SEL, 0);
            let feat_lo = mmio::read32(mmio_base, VIRTIO_MMIO_DEVICE_FEATURES);
            mmio::write32(mmio_base, VIRTIO_MMIO_DEVICE_FEATURES_SEL, 1);
            let feat_hi = mmio::read32(mmio_base, VIRTIO_MMIO_DEVICE_FEATURES);
            let offered = (feat_lo as u64) | ((feat_hi as u64) << 32);
            if offered & VIRTIO_F_VERSION_1 == 0 || offered & VIRTIO_NET_F_MAC == 0 {
                return None;
            }
            let accept = VIRTIO_F_VERSION_1 | VIRTIO_NET_F_MAC;
            mmio::write32(mmio_base, VIRTIO_MMIO_DRIVER_FEATURES_SEL, 0);
            mmio::write32(mmio_base, VIRTIO_MMIO_DRIVER_FEATURES, accept as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_DRIVER_FEATURES_SEL, 1);
            mmio::write32(
                mmio_base,
                VIRTIO_MMIO_DRIVER_FEATURES,
                (accept >> 32) as u32,
            );

            mmio::write32(
                mmio_base,
                VIRTIO_MMIO_STATUS,
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK,
            );
            if mmio::read32(mmio_base, VIRTIO_MMIO_STATUS) & VIRTIO_STATUS_FEATURES_OK == 0 {
                return None;
            }
        }

        // Queue 0 = RX.
        let mut rxq = Virtqueue::new(kalloc).ok()?;
        // SAFETY: queue register setup using the validated MMIO base
        // (mirrors drivers::virtio::console's queue setup).
        unsafe {
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_SEL, 0);
            if mmio::read32(mmio_base, VIRTIO_MMIO_QUEUE_NUM_MAX) < QUEUE_SIZE as u32 {
                return None;
            }
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_NUM, QUEUE_SIZE as u32);
            let dp = rxq.desc_phys().as_u64();
            let ap = rxq.avail_phys().as_u64();
            let up = rxq.used_phys().as_u64();
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_DESC_LOW, dp as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_DESC_HIGH, (dp >> 32) as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_AVAIL_LOW, ap as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_AVAIL_HIGH, (ap >> 32) as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_USED_LOW, up as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_USED_HIGH, (up >> 32) as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_READY, 1);
        }

        // Queue 1 = TX.
        let mut txq = Virtqueue::new(kalloc).ok()?;
        // SAFETY: same validated MMIO base as above.
        unsafe {
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_SEL, 1);
            if mmio::read32(mmio_base, VIRTIO_MMIO_QUEUE_NUM_MAX) < QUEUE_SIZE as u32 {
                return None;
            }
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_NUM, QUEUE_SIZE as u32);
            let dp = txq.desc_phys().as_u64();
            let ap = txq.avail_phys().as_u64();
            let up = txq.used_phys().as_u64();
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_DESC_LOW, dp as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_DESC_HIGH, (dp >> 32) as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_AVAIL_LOW, ap as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_AVAIL_HIGH, (ap >> 32) as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_USED_LOW, up as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_USED_HIGH, (up >> 32) as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_READY, 1);
        }

        // RX buffer pool: RX_COUNT * RX_BUF_SIZE contiguous bytes, one
        // buffer posted per descriptor.
        let rx_pool = kalloc.calloc(RX_COUNT * RX_BUF_SIZE).ok()?;
        let tx_buf = kalloc.calloc(TX_BUF_SIZE).ok()?;

        let mut rx_desc = [0u16; RX_COUNT];
        for (i, slot) in rx_desc.iter_mut().enumerate() {
            let head = rxq.alloc_desc()?;
            let desc = rxq.desc_mut(head);
            desc.addr = rx_pool.add(i * RX_BUF_SIZE).as_u64();
            desc.len = RX_BUF_SIZE as u32;
            desc.flags = VIRTQ_DESC_F_WRITE;
            rxq.submit(head);
            *slot = head;
        }

        let tx_desc = txq.alloc_desc()?;

        // SAFETY: DRIVER_OK signals the device that initialization is
        // complete (mirrors drivers::virtio::console).
        unsafe {
            mmio::write32(
                mmio_base,
                VIRTIO_MMIO_STATUS,
                VIRTIO_STATUS_ACKNOWLEDGE
                    | VIRTIO_STATUS_DRIVER
                    | VIRTIO_STATUS_FEATURES_OK
                    | VIRTIO_STATUS_DRIVER_OK,
            );
        }

        // Read the permanent MAC from config space (VIRTIO_NET_F_MAC was
        // accepted above): two 4-byte registers, only the low 6 bytes used.
        let mut mac = [0u8; 6];
        // SAFETY: mmio_base is the validated virtio-net MMIO region.
        unsafe {
            let lo = mmio::read32(mmio_base, VIRTIO_MMIO_CONFIG).to_le_bytes();
            let hi = mmio::read32(mmio_base, VIRTIO_MMIO_CONFIG + 4).to_le_bytes();
            mac[0..4].copy_from_slice(&lo);
            mac[4..6].copy_from_slice(&hi[0..2]);
        }

        Some(Self {
            rxq,
            txq,
            mmio_base,
            rx_desc,
            tx_desc,
            tx_buf,
            dm_offset,
            mac,
            loopback: VecDeque::new(),
        })
    }

    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    fn is_local_ip(ip: [u8; 4]) -> bool {
        ip[0] == LOOPBACK_NET || ip == sumi_abi::net::GUEST_IP
    }

    fn is_self_directed_frame(&self, frame: &[u8]) -> bool {
        if frame.len() < ETH_HDR_LEN {
            return false;
        }

        let dst = &frame[ETH_DST_OFF..ETH_DST_OFF + 6];
        if dst == self.mac {
            return true;
        }

        let eth_type = u16::from_be_bytes([frame[ETH_TYPE_OFF], frame[ETH_TYPE_OFF + 1]]);
        match eth_type {
            ETH_TYPE_IPV4 if frame.len() >= IPV4_DST_OFF + 4 => {
                let dst_ip = [
                    frame[IPV4_DST_OFF],
                    frame[IPV4_DST_OFF + 1],
                    frame[IPV4_DST_OFF + 2],
                    frame[IPV4_DST_OFF + 3],
                ];
                Self::is_local_ip(dst_ip)
            }
            ETH_TYPE_ARP if dst == ETH_BROADCAST && frame.len() >= ARP_TARGET_PROTO_OFF + 4 => {
                let target_ip = [
                    frame[ARP_TARGET_PROTO_OFF],
                    frame[ARP_TARGET_PROTO_OFF + 1],
                    frame[ARP_TARGET_PROTO_OFF + 2],
                    frame[ARP_TARGET_PROTO_OFF + 3],
                ];
                Self::is_local_ip(target_ip)
            }
            _ => false,
        }
    }

    fn to_virt(&self, paddr: u64) -> VirtualAddr {
        VirtualAddr::new(self.dm_offset + paddr as usize)
    }

    /// Ring the RX queue's doorbell. Called once per `poll_and_wake` (via
    /// `PollDevice::pre_poll`), not once per `receive()` — see the module
    /// doc comment.
    pub fn kick_rx(&self) {
        // SAFETY: writing QUEUE_NOTIFY on the validated virtio-net MMIO
        // region triggers a synchronous VM exit; safe even from IF=0
        // interrupt context (mirrors the console driver's notify writes,
        // which the timer ISR already does transitively via `net::poll`).
        unsafe { mmio::write32(self.mmio_base, VIRTIO_MMIO_QUEUE_NOTIFY, 0) };
    }
}

/// Marker for devices that need a per-`poll_and_wake` housekeeping hook.
/// `VirtioNetDevice` rings its RX doorbell here; `Loopback` needs nothing
/// (transmit already delivers synchronously).
pub trait PollDevice {
    fn pre_poll(&mut self) {}
}

impl PollDevice for VirtioNetDevice {
    fn pre_poll(&mut self) {
        self.kick_rx();
    }
}

impl PollDevice for smoltcp::phy::Loopback {}

pub struct VirtioRxToken {
    frame: Vec<u8>,
}

impl SmolRxToken for VirtioRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.frame)
    }
}

pub struct VirtioTxToken<'a> {
    dev: &'a mut VirtioNetDevice,
}

impl<'a> SmolTxToken for VirtioTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let dev = self.dev;
        let available = TX_BUF_SIZE - VIRTIO_NET_HDR_LEN;
        // smoltcp is expected to never request more than the interface's
        // advertised MTU (1514, well under TX_BUF_SIZE); clamp defensively
        // rather than write past tx_buf if that invariant is ever violated.
        debug_assert!(len <= available, "TX frame exceeds tx_buf capacity");
        let payload_len = len.min(available);

        let tx_virt = dev.to_virt(dev.tx_buf.as_u64());
        // SAFETY: tx_buf is a TX_BUF_SIZE-byte DMA buffer allocated at
        // init; VIRTIO_NET_HDR_LEN (12) < TX_BUF_SIZE.
        unsafe { core::ptr::write_bytes(tx_virt.as_ptr::<u8>(), 0, VIRTIO_NET_HDR_LEN) };

        // SAFETY: payload_ptr..+payload_len lies within tx_buf's
        // TX_BUF_SIZE bytes (payload_len <= available, checked above).
        let buf = unsafe {
            core::slice::from_raw_parts_mut(
                tx_virt.as_ptr::<u8>().add(VIRTIO_NET_HDR_LEN),
                payload_len,
            )
        };
        let result = f(buf);

        // Self-loopback copy — see the `loopback` field doc comment.
        let local_only = dev.is_self_directed_frame(buf);
        dev.loopback.push_back(buf.to_vec());
        if local_only {
            return result;
        }

        let total = (VIRTIO_NET_HDR_LEN + payload_len) as u32;
        let tx_buf_addr = dev.tx_buf.as_u64();
        let desc = dev.txq.desc_mut(dev.tx_desc);
        desc.addr = tx_buf_addr;
        desc.len = total;
        desc.flags = 0;
        dev.txq.submit(dev.tx_desc);

        // SAFETY: writing QUEUE_NOTIFY triggers a synchronous VM exit
        // (mirrors drivers::virtio::console's transmit notify).
        unsafe { mmio::write32(dev.mmio_base, VIRTIO_MMIO_QUEUE_NOTIFY, 1) };

        // Spin for the completion (synchronous, like the console driver's
        // TX path) and then simply reuse the same fixed descriptor next
        // time — never freed back to the queue's free list, since this
        // driver is its only user and always resubmits it in place.
        loop {
            if let Some(elem) = dev.txq.complete() {
                debug_assert_eq!(elem.id as u16, dev.tx_desc);
                break;
            }
            core::hint::spin_loop();
        }

        result
    }
}

impl Device for VirtioNetDevice {
    type RxToken<'a> = VirtioRxToken;
    type TxToken<'a> = VirtioTxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // Drain the self-loopback queue first — see the `loopback` field
        // doc comment. Independent of the RX virtqueue/descriptor pool
        // entirely, so this never competes with real gateway traffic for
        // RX buffers.
        if let Some(frame) = self.loopback.pop_front() {
            return Some((VirtioRxToken { frame }, VirtioTxToken { dev: self }));
        }

        let elem = self.rxq.complete()?;
        let id = elem.id as u16;
        debug_assert!(
            self.rx_desc.contains(&id),
            "RX completion for an unknown descriptor id"
        );

        let desc_addr = self.rxq.desc_mut(id).addr;
        let buf_virt = self.to_virt(desc_addr);
        let len = (elem.len as usize).min(RX_BUF_SIZE);
        let payload_len = len.saturating_sub(VIRTIO_NET_HDR_LEN);

        let mut frame = alloc::vec![0u8; payload_len];
        if payload_len > 0 {
            // SAFETY: buf_virt is an RX pool buffer the host DMA'd `len`
            // (clamped to RX_BUF_SIZE) bytes into before this MMIO notify
            // returned; the virtio-net header occupies the first 12 bytes.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    buf_virt.as_ptr::<u8>().add(VIRTIO_NET_HDR_LEN),
                    frame.as_mut_ptr(),
                    payload_len,
                );
            }
        }

        // Repost the same descriptor immediately — it's still valid (addr,
        // len, flags unchanged since init) and the buffer has been copied
        // out already.
        self.rxq.submit(id);

        Some((VirtioRxToken { frame }, VirtioTxToken { dev: self }))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(VirtioTxToken { dev: self })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1514;
        caps.medium = Medium::Ethernet;
        caps
    }
}
