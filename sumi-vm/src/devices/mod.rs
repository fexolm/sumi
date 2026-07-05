pub mod virtio_console;
pub mod virtio_fs;
pub mod virtio_mmio;
pub mod virtio_net;

use std::sync::Arc;

use sumi_abi::arch::layout::{
    VIRTIO_CONSOLE_MMIO, VIRTIO_MMIO_BASE, VIRTIO_MMIO_STRIDE, VIRTIO_NET_MMIO,
};
use vm_memory::GuestMemoryMmap;

use self::virtio_console::VirtioConsoleBackend;
use self::virtio_fs::VirtioFs;
use self::virtio_mmio::VirtioMmioDevice;
use self::virtio_net::{GatewayChannel, VirtioNetBackend};

pub struct DeviceRegistry {
    devices: Vec<(u64, VirtioMmioDevice)>,
}

// SAFETY: DeviceRegistry is only accessed through a Mutex; the raw pointer inside
// VirtioMmioDevice/VirtioFs points to the DAX window which lives for the VM lifetime.
unsafe impl Send for DeviceRegistry {}
unsafe impl Sync for DeviceRegistry {}

impl DeviceRegistry {
    pub fn new(
        share_dir: Option<&std::path::Path>,
        dax_host_ptr: *mut u8,
        net_chan: Arc<GatewayChannel>,
    ) -> Self {
        let mut devices = Vec::new();

        if let Some(dir) = share_dir {
            let fs_device = VirtioMmioDevice::new(Box::new(VirtioFs::new(dir, dax_host_ptr)));
            devices.push((VIRTIO_MMIO_BASE.as_u64(), fs_device));
        }

        // Console is always present
        let console_device = VirtioMmioDevice::new(Box::new(VirtioConsoleBackend::new()));
        devices.push((VIRTIO_CONSOLE_MMIO.as_u64(), console_device));

        // Net is always present too — the guest driver simply finds no
        // device there when the gateway has nothing to talk to.
        let net_backend = VirtioNetBackend::new(net_chan, sumi_abi::net::GUEST_MAC);
        let net_device = VirtioMmioDevice::new(Box::new(net_backend));
        devices.push((VIRTIO_NET_MMIO.as_u64(), net_device));

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
