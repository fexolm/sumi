use sumi_abi::{
    address::{DirectMap, PhysicalAddr, VirtualAddr},
    arch::layout::{DIRECT_MAP_OFFSET, PAGE_SIZE, VIRTIO_CONSOLE_MMIO},
    virtio::*,
};

use crate::drivers::virtio::{mmio, virtqueue::Virtqueue};
use crate::memory::alloc::{kmalloc::KernelAllocator, palloc::PageAllocator};

pub struct VirtioConsole {
    transmitq: spin::Mutex<Virtqueue>,
    receiveq: spin::Mutex<Virtqueue>,
    mmio_base: VirtualAddr,
    tx_buf: PhysicalAddr,
    rx_buf: PhysicalAddr,
}

impl VirtioConsole {
    /// Probe and initialize the virtio-console device. Returns None if not present.
    pub fn init<DM: DirectMap>(
        kalloc: &KernelAllocator<DM>,
        palloc: &PageAllocator,
    ) -> Option<Self> {
        let dm = kalloc.direct_map();
        let mmio_base = VIRTIO_CONSOLE_MMIO.to_virtual(dm);

        // SAFETY: All MMIO accesses are to the virtio-console device register region
        // at a known physical address mapped through the direct map.
        unsafe {
            if mmio::read32(mmio_base, VIRTIO_MMIO_MAGIC) != VIRTIO_MMIO_MAGIC_VALUE {
                return None;
            }
            if mmio::read32(mmio_base, VIRTIO_MMIO_VERSION) != 2 {
                return None;
            }
            if mmio::read32(mmio_base, VIRTIO_MMIO_DEVICE_ID) != VIRTIO_DEVICE_CONSOLE {
                return None;
            }

            // Reset
            mmio::write32(mmio_base, VIRTIO_MMIO_STATUS, 0);

            // Acknowledge + Driver (no feature negotiation needed for basic console)
            mmio::write32(mmio_base, VIRTIO_MMIO_STATUS, VIRTIO_STATUS_ACKNOWLEDGE);
            mmio::write32(
                mmio_base,
                VIRTIO_MMIO_STATUS,
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER,
            );

            // Features OK (accept no features)
            mmio::write32(
                mmio_base,
                VIRTIO_MMIO_STATUS,
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK,
            );
            if mmio::read32(mmio_base, VIRTIO_MMIO_STATUS) & VIRTIO_STATUS_FEATURES_OK == 0 {
                return None;
            }
        }

        // Set up queue 0 (receiveq)
        let receiveq = Virtqueue::new(kalloc).ok()?;
        // SAFETY: Queue register setup using the validated MMIO base.
        unsafe {
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_SEL, 0);
            if mmio::read32(mmio_base, VIRTIO_MMIO_QUEUE_NUM_MAX) < QUEUE_SIZE as u32 {
                return None;
            }
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_NUM, QUEUE_SIZE as u32);

            let dp = receiveq.desc_phys().as_u64();
            let ap = receiveq.avail_phys().as_u64();
            let up = receiveq.used_phys().as_u64();
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_DESC_LOW, dp as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_DESC_HIGH, (dp >> 32) as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_AVAIL_LOW, ap as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_AVAIL_HIGH, (ap >> 32) as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_USED_LOW, up as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_USED_HIGH, (up >> 32) as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_READY, 1);
        }

        // Set up queue 1 (transmitq)
        let transmitq = Virtqueue::new(kalloc).ok()?;
        // SAFETY: Queue register setup using the validated MMIO base.
        unsafe {
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_SEL, 1);
            if mmio::read32(mmio_base, VIRTIO_MMIO_QUEUE_NUM_MAX) < QUEUE_SIZE as u32 {
                return None;
            }
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_NUM, QUEUE_SIZE as u32);

            let dp = transmitq.desc_phys().as_u64();
            let ap = transmitq.avail_phys().as_u64();
            let up = transmitq.used_phys().as_u64();
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_DESC_LOW, dp as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_DESC_HIGH, (dp >> 32) as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_AVAIL_LOW, ap as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_AVAIL_HIGH, (ap >> 32) as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_USED_LOW, up as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_USED_HIGH, (up >> 32) as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_READY, 1);
        }

        // Allocate one page each for TX and RX bounce buffers
        let tx_buf = palloc.alloc(1).ok()?;
        let rx_buf = palloc.alloc(1).ok()?;

        // SAFETY: Setting DRIVER_OK to signal initialization is complete.
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

        Some(Self {
            transmitq: spin::Mutex::new(transmitq),
            receiveq: spin::Mutex::new(receiveq),
            mmio_base,
            tx_buf,
            rx_buf,
        })
    }

    /// Write data to the console. Returns bytes written.
    pub fn write(&self, data: &[u8]) -> usize {
        let mut total = 0usize;
        let mut remaining = data;

        while !remaining.is_empty() {
            let chunk_len = remaining.len().min(PAGE_SIZE);
            let chunk = &remaining[..chunk_len];

            let mut queue = self.transmitq.lock();

            // Copy data into tx bounce buffer inside the lock so the buffer
            // is protected against concurrent writers.
            // SAFETY: tx_buf is a valid physical page; DIRECT_MAP_OFFSET converts it
            // to the kernel virtual address in the direct-map window.
            let tx_virt = VirtualAddr::new(DIRECT_MAP_OFFSET.as_usize() + self.tx_buf.as_usize());
            unsafe {
                core::ptr::copy_nonoverlapping(
                    chunk.as_ptr(),
                    tx_virt.as_ptr::<u8>(),
                    chunk_len,
                );
            }

            let head = match queue.alloc_desc() {
                Some(d) => d,
                None => break,
            };

            {
                let desc = queue.desc_mut(head);
                desc.addr = self.tx_buf.as_u64();
                desc.len = chunk_len as u32;
                desc.flags = 0; // device-readable, no NEXT
            }

            queue.submit(head);
            drop(queue);

            // Kick transmitq (queue index 1)
            // SAFETY: Writing QueueNotify triggers a synchronous VM exit.
            unsafe { mmio::write32(self.mmio_base, VIRTIO_MMIO_QUEUE_NOTIFY, 1) }

            // Poll for completion
            let mut queue = self.transmitq.lock();
            loop {
                if let Some(elem) = queue.complete() {
                    queue.free_chain(elem.id as u16);
                    break;
                }
                core::hint::spin_loop();
            }
            drop(queue);

            total += chunk_len;
            remaining = &remaining[chunk_len..];
        }

        total
    }

    /// Read data from the console. Returns bytes read.
    pub fn read(&self, buf: &mut [u8]) -> usize {
        if buf.is_empty() {
            return 0;
        }

        let read_len = buf.len().min(PAGE_SIZE);

        let mut queue = self.receiveq.lock();
        let head = match queue.alloc_desc() {
            Some(d) => d,
            None => return 0,
        };

        {
            let desc = queue.desc_mut(head);
            desc.addr = self.rx_buf.as_u64();
            desc.len = read_len as u32;
            desc.flags = VIRTQ_DESC_F_WRITE; // device-writable
        }

        queue.submit(head);
        drop(queue);

        // Kick receiveq (queue index 0)
        // SAFETY: Writing QueueNotify triggers a synchronous VM exit.
        unsafe { mmio::write32(self.mmio_base, VIRTIO_MMIO_QUEUE_NOTIFY, 0) }

        // Poll for completion
        let bytes_read;
        let mut queue = self.receiveq.lock();
        loop {
            if let Some(elem) = queue.complete() {
                bytes_read = elem.len as usize;
                queue.free_chain(elem.id as u16);
                break;
            }
            core::hint::spin_loop();
        }

        // Copy from rx bounce buffer inside the lock so the buffer is protected
        // against concurrent readers.
        // SAFETY: rx_buf is a valid physical page; DIRECT_MAP_OFFSET converts it
        // to the kernel virtual address in the direct-map window.
        let copy_len = bytes_read.min(read_len);
        if copy_len > 0 {
            let rx_virt =
                VirtualAddr::new(DIRECT_MAP_OFFSET.as_usize() + self.rx_buf.as_usize());
            unsafe {
                core::ptr::copy_nonoverlapping(
                    rx_virt.as_ptr::<u8>(),
                    buf.as_mut_ptr(),
                    copy_len,
                );
            }
        }
        drop(queue);

        copy_len
    }
}
