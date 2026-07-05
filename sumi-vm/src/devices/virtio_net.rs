//! Host-side virtio-net backend: bridges the guest's RX/TX virtqueues to a
//! pair of frame queues (`GatewayChannel`) that the host userspace gateway
//! (`crate::net::gateway`) drains and fills with a real smoltcp `Interface`.
//!
//! See `docs/networking-design.md` Phase 2 and the top-level networking
//! plan's risk section for the two invariants this file must uphold:
//! - Every guest<->host Ethernet frame is wrapped in a fixed 12-byte
//!   `virtio_net_hdr` (VERSION_1); it is stripped on RX into the guest and
//!   prepended (zeroed) on TX out of the guest — an off-by-N here corrupts
//!   every frame.
//! - This backend runs inside the vCPU thread's `DeviceRegistry` mutex (via
//!   `process_queue`, called from the MMIO-notify VM exit). It must never
//!   block: it only ever briefly locks the `GatewayChannel` queues, and
//!   never calls into the gateway's smoltcp `Interface` directly. Lock
//!   order is `DeviceRegistry -> GatewayChannel`, never the reverse.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

use sumi_abi::virtio::*;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use super::virtio_mmio::{
    VirtioBackend, VirtqueueState, post_used, read_avail_head, read_avail_idx, read_desc,
};

/// Frame queues shared between the vCPU thread (via `VirtioNetBackend`,
/// under `DeviceRegistry`'s lock) and the host gateway thread. Arc-shared so
/// both sides can outlive whichever creates it first.
pub struct GatewayChannel {
    /// Ethernet frames the guest transmitted, awaiting gateway pickup.
    pub guest_to_host: Mutex<VecDeque<Vec<u8>>>,
    /// Ethernet frames the gateway produced, awaiting an RX descriptor.
    pub host_to_guest: Mutex<VecDeque<Vec<u8>>>,
    /// Paired with `tx_ready`: lets the gateway thread block with a bounded
    /// timeout instead of busy-polling, while still waking immediately when
    /// the guest transmits (see `notify`).
    pub signal: Condvar,
    pub tx_ready: Mutex<bool>,
}

impl GatewayChannel {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            guest_to_host: Mutex::new(VecDeque::new()),
            host_to_guest: Mutex::new(VecDeque::new()),
            signal: Condvar::new(),
            tx_ready: Mutex::new(false),
        })
    }

    /// Wake the gateway thread out of its bounded wait. Called after
    /// pushing a frame onto `guest_to_host` so guest-initiated traffic
    /// isn't held up by the poll-delay timeout.
    pub fn notify(&self) {
        // A poisoned mutex only happens if the gateway thread panicked;
        // there is no meaningful recovery, so propagate via unwrap() same
        // as every other lock in this backend.
        let mut ready = self.tx_ready.lock().unwrap();
        *ready = true;
        self.signal.notify_one();
    }
}

pub struct VirtioNetBackend {
    chan: Arc<GatewayChannel>,
    /// Per-queue avail-ring cursor: index 0 = RX, 1 = TX.
    last_avail_idx: [u16; 2],
    /// The guest's permanent MAC, exposed via config space (`VIRTIO_NET_F_MAC`).
    mac: [u8; 6],
}

impl VirtioNetBackend {
    pub fn new(chan: Arc<GatewayChannel>, mac: [u8; 6]) -> Self {
        Self {
            chan,
            last_avail_idx: [0; 2],
            mac,
        }
    }

    /// Queue 1 = TX: guest-transmitted frames. Strip the 12-byte
    /// `virtio_net_hdr`, forward the Ethernet payload to the gateway.
    fn process_tx(&mut self, queue: &VirtqueueState, mem: &GuestMemoryMmap<()>) {
        let avail_idx = read_avail_idx(queue, mem);

        while self.last_avail_idx[1] != avail_idx {
            let head = read_avail_head(queue, self.last_avail_idx[1], mem);

            let mut data: Vec<u8> = Vec::new();
            let mut total_len = 0u32;
            let mut idx = head;
            // Bounded at QUEUE_SIZE to guard against a malformed/cyclic chain.
            for _ in 0..QUEUE_SIZE {
                let desc = read_desc(queue, idx, mem);
                let len = desc.len as usize;
                if len > 0 {
                    let start = data.len();
                    data.resize(start + len, 0);
                    mem.read_slice(&mut data[start..], GuestAddress(desc.addr))
                        .unwrap();
                }
                total_len += desc.len;
                if desc.flags & VIRTQ_DESC_F_NEXT == 0 {
                    break;
                }
                idx = desc.next;
            }

            if data.len() >= VIRTIO_NET_HDR_LEN {
                let frame = data.split_off(VIRTIO_NET_HDR_LEN);
                self.chan.guest_to_host.lock().unwrap().push_back(frame);
                self.chan.notify();
            }

            post_used(queue, head, total_len, mem);
            self.last_avail_idx[1] = self.last_avail_idx[1].wrapping_add(1);
        }
    }

    /// Queue 0 = RX: fill posted guest buffers from frames the gateway
    /// produced. Stops as soon as either side runs out — surplus posted
    /// buffers are left for the next notify (see the RX pull handshake
    /// note in `docs/networking-design.md`).
    fn process_rx(&mut self, queue: &VirtqueueState, mem: &GuestMemoryMmap<()>) {
        let avail_idx = read_avail_idx(queue, mem);
        let mut frames = self.chan.host_to_guest.lock().unwrap();

        while !frames.is_empty() && self.last_avail_idx[0] != avail_idx {
            let head = read_avail_head(queue, self.last_avail_idx[0], mem);
            let desc = read_desc(queue, head, mem);
            let frame = frames.pop_front().unwrap();

            let hdr = VirtioNetHdr {
                num_buffers: 1,
                ..Default::default()
            };
            // SAFETY: VirtioNetHdr is #[repr(C)] with a compile-time-asserted
            // 12-byte size and no padding; reading it as a byte slice of
            // that length is valid.
            let hdr_bytes: &[u8] = unsafe {
                core::slice::from_raw_parts(&hdr as *const _ as *const u8, VIRTIO_NET_HDR_LEN)
            };
            mem.write_slice(hdr_bytes, GuestAddress(desc.addr)).unwrap();

            let avail_payload = (desc.len as usize).saturating_sub(VIRTIO_NET_HDR_LEN);
            let n = frame.len().min(avail_payload);
            if n > 0 {
                mem.write_slice(
                    &frame[..n],
                    GuestAddress(desc.addr + VIRTIO_NET_HDR_LEN as u64),
                )
                .unwrap();
            }

            post_used(queue, head, (VIRTIO_NET_HDR_LEN + n) as u32, mem);
            self.last_avail_idx[0] = self.last_avail_idx[0].wrapping_add(1);
        }
    }
}

impl VirtioBackend for VirtioNetBackend {
    fn device_id(&self) -> u32 {
        VIRTIO_DEVICE_NET
    }

    fn num_queues(&self) -> usize {
        2
    }

    fn device_features(&self) -> u64 {
        VIRTIO_NET_F_MAC | VIRTIO_F_VERSION_1
    }

    fn read_config(&self, offset: usize) -> u32 {
        // MAC occupies config bytes 0..6; pad to 8 bytes so a 4-byte-aligned
        // read at offset 4 returns the trailing 2 MAC bytes plus zero pad.
        let mut buf = [0u8; 8];
        buf[0..6].copy_from_slice(&self.mac);
        match buf.get(offset..offset + 4) {
            Some(word) => u32::from_le_bytes(word.try_into().unwrap()),
            None => 0,
        }
    }

    fn process_queue(
        &mut self,
        queue_idx: usize,
        queue: &VirtqueueState,
        mem: &GuestMemoryMmap<()>,
    ) {
        match queue_idx {
            0 => self.process_rx(queue, mem),
            1 => self.process_tx(queue, mem),
            _ => {}
        }
    }
}

#[cfg(test)]
#[path = "virtio_net_test.rs"]
mod virtio_net_test;
