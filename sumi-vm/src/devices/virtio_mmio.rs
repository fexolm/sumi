use sumi_abi::virtio::*;
use vm_memory::GuestMemoryMmap;

use super::virtio_fs::VirtioFs;

pub struct VirtqueueState {
    pub num: u32,
    pub ready: bool,
    pub desc_addr: u64,
    pub avail_addr: u64,
    pub used_addr: u64,
}

impl VirtqueueState {
    fn new() -> Self {
        Self {
            num: 0,
            ready: false,
            desc_addr: 0,
            avail_addr: 0,
            used_addr: 0,
        }
    }
}

pub struct VirtioMmioDevice {
    status: u32,
    device_features_sel: u32,
    driver_features: u64,
    driver_features_sel: u32,
    queue_sel: u32,
    queues: [VirtqueueState; 2],
    backend: VirtioFs,
}

impl VirtioMmioDevice {
    pub fn new_fs(share_dir: &std::path::Path) -> Self {
        Self {
            status: 0,
            device_features_sel: 0,
            driver_features: 0,
            driver_features_sel: 0,
            queue_sel: 0,
            queues: [VirtqueueState::new(), VirtqueueState::new()],
            backend: VirtioFs::new(share_dir),
        }
    }

    pub fn mmio_read(&self, offset: usize) -> u32 {
        match offset {
            VIRTIO_MMIO_MAGIC => VIRTIO_MMIO_MAGIC_VALUE,
            VIRTIO_MMIO_VERSION => 2,
            VIRTIO_MMIO_DEVICE_ID => VIRTIO_DEVICE_FS,
            VIRTIO_MMIO_VENDOR_ID => SUMI_VENDOR_ID,
            VIRTIO_MMIO_DEVICE_FEATURES => {
                // No features for now
                0
            }
            VIRTIO_MMIO_QUEUE_NUM_MAX => QUEUE_SIZE as u32,
            VIRTIO_MMIO_QUEUE_READY => {
                let q = self.queue_sel as usize;
                if q < 2 {
                    self.queues[q].ready as u32
                } else {
                    0
                }
            }
            VIRTIO_MMIO_STATUS => self.status,
            VIRTIO_MMIO_INTERRUPT_STATUS => 0,
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
                let q = self.queue_sel as usize;
                if q < 2 {
                    self.queues[q].num = value;
                }
            }
            VIRTIO_MMIO_QUEUE_READY => {
                let q = self.queue_sel as usize;
                if q < 2 {
                    self.queues[q].ready = value != 0;
                }
            }
            VIRTIO_MMIO_QUEUE_NOTIFY => {
                let queue_idx = value as usize;
                if queue_idx < 2 && self.queues[queue_idx].ready {
                    self.backend.process_queue(&self.queues[queue_idx], mem);
                }
            }
            VIRTIO_MMIO_INTERRUPT_ACK => {}
            VIRTIO_MMIO_STATUS => self.status = value,
            VIRTIO_MMIO_QUEUE_DESC_LOW => {
                let q = self.queue_sel as usize;
                if q < 2 {
                    self.queues[q].desc_addr =
                        (self.queues[q].desc_addr & !0xFFFF_FFFF) | value as u64;
                }
            }
            VIRTIO_MMIO_QUEUE_DESC_HIGH => {
                let q = self.queue_sel as usize;
                if q < 2 {
                    self.queues[q].desc_addr =
                        (self.queues[q].desc_addr & 0xFFFF_FFFF) | ((value as u64) << 32);
                }
            }
            VIRTIO_MMIO_QUEUE_AVAIL_LOW => {
                let q = self.queue_sel as usize;
                if q < 2 {
                    self.queues[q].avail_addr =
                        (self.queues[q].avail_addr & !0xFFFF_FFFF) | value as u64;
                }
            }
            VIRTIO_MMIO_QUEUE_AVAIL_HIGH => {
                let q = self.queue_sel as usize;
                if q < 2 {
                    self.queues[q].avail_addr =
                        (self.queues[q].avail_addr & 0xFFFF_FFFF) | ((value as u64) << 32);
                }
            }
            VIRTIO_MMIO_QUEUE_USED_LOW => {
                let q = self.queue_sel as usize;
                if q < 2 {
                    self.queues[q].used_addr =
                        (self.queues[q].used_addr & !0xFFFF_FFFF) | value as u64;
                }
            }
            VIRTIO_MMIO_QUEUE_USED_HIGH => {
                let q = self.queue_sel as usize;
                if q < 2 {
                    self.queues[q].used_addr =
                        (self.queues[q].used_addr & 0xFFFF_FFFF) | ((value as u64) << 32);
                }
            }
            _ => {}
        }
    }
}
