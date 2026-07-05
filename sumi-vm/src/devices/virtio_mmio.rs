use sumi_abi::virtio::*;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

pub trait VirtioBackend {
    fn device_id(&self) -> u32;
    fn num_queues(&self) -> usize;
    fn process_queue(
        &mut self,
        queue_idx: usize,
        queue: &VirtqueueState,
        mem: &GuestMemoryMmap<()>,
    );

    /// 64-bit device feature bitmap. Defaults to "no features offered"
    /// (matches every backend before virtio-net).
    fn device_features(&self) -> u64 {
        0
    }

    /// Read a 32-bit word from the device-specific config space at
    /// `offset` (already relative to `VIRTIO_MMIO_CONFIG`). Defaults to
    /// zero (no config space).
    fn read_config(&self, _offset: usize) -> u32 {
        0
    }
}

#[derive(Default)]
pub struct VirtqueueState {
    pub num: u32,
    pub ready: bool,
    pub desc_addr: u64,
    pub avail_addr: u64,
    pub used_addr: u64,
}

impl VirtqueueState {
    pub fn new() -> Self {
        Self::default()
    }
}

pub fn read_desc(queue: &VirtqueueState, idx: u16, mem: &GuestMemoryMmap<()>) -> VirtqDesc {
    let addr = queue.desc_addr + idx as u64 * 16;
    let mut buf = [0u8; 16];
    mem.read_slice(&mut buf, GuestAddress(addr)).unwrap();
    VirtqDesc {
        addr: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
        len: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
        flags: u16::from_le_bytes(buf[12..14].try_into().unwrap()),
        next: u16::from_le_bytes(buf[14..16].try_into().unwrap()),
    }
}

pub fn read_avail_idx(queue: &VirtqueueState, mem: &GuestMemoryMmap<()>) -> u16 {
    let mut buf = [0u8; 4];
    mem.read_slice(&mut buf, GuestAddress(queue.avail_addr))
        .unwrap();
    u16::from_le_bytes([buf[2], buf[3]])
}

pub fn read_avail_head(
    queue: &VirtqueueState,
    last_avail_idx: u16,
    mem: &GuestMemoryMmap<()>,
) -> u16 {
    let ring_idx = (last_avail_idx % QUEUE_SIZE) as u64;
    let mut head_buf = [0u8; 2];
    mem.read_slice(
        &mut head_buf,
        GuestAddress(queue.avail_addr + 4 + ring_idx * 2),
    )
    .unwrap();
    u16::from_le_bytes(head_buf)
}

pub fn post_used(queue: &VirtqueueState, head: u16, bytes_used: u32, mem: &GuestMemoryMmap<()>) {
    let mut used_buf = [0u8; 4];
    mem.read_slice(&mut used_buf, GuestAddress(queue.used_addr))
        .unwrap();
    let used_idx = u16::from_le_bytes([used_buf[2], used_buf[3]]);

    let entry_addr = queue.used_addr + 4 + (used_idx % QUEUE_SIZE) as u64 * 8;
    mem.write_slice(&(head as u32).to_le_bytes(), GuestAddress(entry_addr))
        .unwrap();
    mem.write_slice(&bytes_used.to_le_bytes(), GuestAddress(entry_addr + 4))
        .unwrap();

    let new_used_idx = used_idx.wrapping_add(1);
    mem.write_slice(
        &new_used_idx.to_le_bytes(),
        GuestAddress(queue.used_addr + 2),
    )
    .unwrap();
}

pub struct VirtioMmioDevice {
    status: u32,
    device_features_sel: u32,
    driver_features: u64,
    driver_features_sel: u32,
    queue_sel: u32,
    queues: Vec<VirtqueueState>,
    backend: Box<dyn VirtioBackend>,
}

impl VirtioMmioDevice {
    pub fn new(backend: Box<dyn VirtioBackend>) -> Self {
        let num_queues = backend.num_queues();
        Self {
            status: 0,
            device_features_sel: 0,
            driver_features: 0,
            driver_features_sel: 0,
            queue_sel: 0,
            queues: (0..num_queues).map(|_| VirtqueueState::new()).collect(),
            backend,
        }
    }

    fn selected_queue(&self) -> Option<&VirtqueueState> {
        self.queues.get(self.queue_sel as usize)
    }

    fn selected_queue_mut(&mut self) -> Option<&mut VirtqueueState> {
        self.queues.get_mut(self.queue_sel as usize)
    }

    pub fn mmio_read(&self, offset: usize) -> u32 {
        match offset {
            VIRTIO_MMIO_MAGIC => VIRTIO_MMIO_MAGIC_VALUE,
            VIRTIO_MMIO_VERSION => 2,
            VIRTIO_MMIO_DEVICE_ID => self.backend.device_id(),
            VIRTIO_MMIO_VENDOR_ID => SUMI_VENDOR_ID,
            VIRTIO_MMIO_DEVICE_FEATURES => {
                // device_features_sel selects the low (0) or high (1) 32
                // bits of the 64-bit feature bitmap; mask to those two
                // values so an out-of-spec selector can't shift by >= 64.
                let shift = (self.device_features_sel & 1) * 32;
                (self.backend.device_features() >> shift) as u32
            }
            VIRTIO_MMIO_QUEUE_NUM_MAX => QUEUE_SIZE as u32,
            VIRTIO_MMIO_QUEUE_READY => self.selected_queue().map_or(0, |q| q.ready as u32),
            VIRTIO_MMIO_STATUS => self.status,
            VIRTIO_MMIO_INTERRUPT_STATUS => 0,
            _ if offset >= VIRTIO_MMIO_CONFIG => {
                self.backend.read_config(offset - VIRTIO_MMIO_CONFIG)
            }
            _ => 0,
        }
    }

    pub fn mmio_write(&mut self, offset: usize, value: u32, mem: &GuestMemoryMmap<()>) {
        match offset {
            VIRTIO_MMIO_DEVICE_FEATURES_SEL => self.device_features_sel = value,
            VIRTIO_MMIO_DRIVER_FEATURES => {
                if self.driver_features_sel == 0 {
                    self.driver_features =
                        (self.driver_features & 0xFFFF_FFFF_0000_0000) | value as u64;
                } else {
                    self.driver_features =
                        (self.driver_features & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32);
                }
            }
            VIRTIO_MMIO_DRIVER_FEATURES_SEL => self.driver_features_sel = value,
            VIRTIO_MMIO_QUEUE_SEL => self.queue_sel = value,
            VIRTIO_MMIO_QUEUE_NUM => {
                if let Some(q) = self.selected_queue_mut() {
                    q.num = value;
                }
            }
            VIRTIO_MMIO_QUEUE_READY => {
                if let Some(q) = self.selected_queue_mut() {
                    q.ready = value != 0;
                }
            }
            VIRTIO_MMIO_QUEUE_NOTIFY => {
                let queue_idx = value as usize;
                if queue_idx < self.queues.len() && self.queues[queue_idx].ready {
                    self.backend
                        .process_queue(queue_idx, &self.queues[queue_idx], mem);
                }
            }
            VIRTIO_MMIO_INTERRUPT_ACK => {}
            VIRTIO_MMIO_STATUS => self.status = value,
            VIRTIO_MMIO_QUEUE_DESC_LOW => {
                if let Some(q) = self.selected_queue_mut() {
                    set_low(&mut q.desc_addr, value);
                }
            }
            VIRTIO_MMIO_QUEUE_DESC_HIGH => {
                if let Some(q) = self.selected_queue_mut() {
                    set_high(&mut q.desc_addr, value);
                }
            }
            VIRTIO_MMIO_QUEUE_AVAIL_LOW => {
                if let Some(q) = self.selected_queue_mut() {
                    set_low(&mut q.avail_addr, value);
                }
            }
            VIRTIO_MMIO_QUEUE_AVAIL_HIGH => {
                if let Some(q) = self.selected_queue_mut() {
                    set_high(&mut q.avail_addr, value);
                }
            }
            VIRTIO_MMIO_QUEUE_USED_LOW => {
                if let Some(q) = self.selected_queue_mut() {
                    set_low(&mut q.used_addr, value);
                }
            }
            VIRTIO_MMIO_QUEUE_USED_HIGH => {
                if let Some(q) = self.selected_queue_mut() {
                    set_high(&mut q.used_addr, value);
                }
            }
            _ => {}
        }
    }
}

fn set_low(addr: &mut u64, value: u32) {
    *addr = (*addr & !0xFFFF_FFFF) | value as u64;
}

fn set_high(addr: &mut u64, value: u32) {
    *addr = (*addr & 0xFFFF_FFFF) | ((value as u64) << 32);
}

#[cfg(test)]
#[path = "virtio_mmio_test.rs"]
mod virtio_mmio_test;
