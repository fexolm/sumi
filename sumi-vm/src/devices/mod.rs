pub mod virtio_fs;
pub mod virtio_mmio;

use sumi_abi::arch::layout::{VIRTIO_MMIO_BASE, VIRTIO_MMIO_STRIDE};
use vm_memory::GuestMemoryMmap;

use self::virtio_mmio::VirtioMmioDevice;

pub struct DeviceRegistry {
    devices: Vec<(u64, VirtioMmioDevice)>,
}

// SAFETY: DeviceRegistry is only accessed through a Mutex; the raw pointer inside
// VirtioMmioDevice/VirtioFs points to the DAX window which lives for the VM lifetime.
unsafe impl Send for DeviceRegistry {}
unsafe impl Sync for DeviceRegistry {}

impl DeviceRegistry {
    pub fn new(share_dir: Option<&std::path::Path>, dax_host_ptr: *mut u8) -> Self {
        let mut devices = Vec::new();

        if let Some(dir) = share_dir {
            let fs_device = VirtioMmioDevice::new_fs(dir, dax_host_ptr);
            devices.push((VIRTIO_MMIO_BASE.as_u64(), fs_device));
        }

        Self { devices }
    }

    pub fn handle_mmio_read(&self, addr: u64, data: &mut [u8]) {
        for (base, device) in &self.devices {
            if addr >= *base && addr < *base + VIRTIO_MMIO_STRIDE as u64 {
                let offset = (addr - base) as usize;
                let val = device.mmio_read(offset);
                let bytes = val.to_le_bytes();
                let n = data.len().min(4);
                data[..n].copy_from_slice(&bytes[..n]);
                return;
            }
        }
        // Unknown MMIO region — return zeros
        data.fill(0);
    }

    pub fn handle_mmio_write(&mut self, addr: u64, data: &[u8], mem: &GuestMemoryMmap<()>) {
        for (base, device) in &mut self.devices {
            if addr >= *base && addr < *base + VIRTIO_MMIO_STRIDE as u64 {
                let offset = (addr - *base) as usize;
                let mut val_bytes = [0u8; 4];
                let n = data.len().min(4);
                val_bytes[..n].copy_from_slice(&data[..n]);
                let val = u32::from_le_bytes(val_bytes);
                device.mmio_write(offset, val, mem);
                return;
            }
        }
    }
}
