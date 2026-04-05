use sumi_abi::virtio::*;
use vm_memory::GuestMemoryMmap;

pub trait VirtioBackend {
    fn device_id(&self) -> u32;
    fn num_queues(&self) -> usize;
    fn process_queue(&mut self, queue_idx: usize, queue: &VirtqueueState, mem: &GuestMemoryMmap<()>);
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

    pub fn mmio_read(&self, offset: usize) -> u32 {
        match offset {
            VIRTIO_MMIO_MAGIC => VIRTIO_MMIO_MAGIC_VALUE,
            VIRTIO_MMIO_VERSION => 2,
            VIRTIO_MMIO_DEVICE_ID => self.backend.device_id(),
            VIRTIO_MMIO_VENDOR_ID => SUMI_VENDOR_ID,
            VIRTIO_MMIO_DEVICE_FEATURES => {
                // No features for now
                0
            }
            VIRTIO_MMIO_QUEUE_NUM_MAX => QUEUE_SIZE as u32,
            VIRTIO_MMIO_QUEUE_READY => {
                let q = self.queue_sel as usize;
                if q < self.queues.len() {
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
                if q < self.queues.len() {
                    self.queues[q].num = value;
                }
            }
            VIRTIO_MMIO_QUEUE_READY => {
                let q = self.queue_sel as usize;
                if q < self.queues.len() {
                    self.queues[q].ready = value != 0;
                }
            }
            VIRTIO_MMIO_QUEUE_NOTIFY => {
                let queue_idx = value as usize;
                if queue_idx < self.queues.len() && self.queues[queue_idx].ready {
                    self.backend.process_queue(queue_idx, &self.queues[queue_idx], mem);
                }
            }
            VIRTIO_MMIO_INTERRUPT_ACK => {}
            VIRTIO_MMIO_STATUS => self.status = value,
            VIRTIO_MMIO_QUEUE_DESC_LOW => {
                let q = self.queue_sel as usize;
                if q < self.queues.len() {
                    self.queues[q].desc_addr =
                        (self.queues[q].desc_addr & !0xFFFF_FFFF) | value as u64;
                }
            }
            VIRTIO_MMIO_QUEUE_DESC_HIGH => {
                let q = self.queue_sel as usize;
                if q < self.queues.len() {
                    self.queues[q].desc_addr =
                        (self.queues[q].desc_addr & 0xFFFF_FFFF) | ((value as u64) << 32);
                }
            }
            VIRTIO_MMIO_QUEUE_AVAIL_LOW => {
                let q = self.queue_sel as usize;
                if q < self.queues.len() {
                    self.queues[q].avail_addr =
                        (self.queues[q].avail_addr & !0xFFFF_FFFF) | value as u64;
                }
            }
            VIRTIO_MMIO_QUEUE_AVAIL_HIGH => {
                let q = self.queue_sel as usize;
                if q < self.queues.len() {
                    self.queues[q].avail_addr =
                        (self.queues[q].avail_addr & 0xFFFF_FFFF) | ((value as u64) << 32);
                }
            }
            VIRTIO_MMIO_QUEUE_USED_LOW => {
                let q = self.queue_sel as usize;
                if q < self.queues.len() {
                    self.queues[q].used_addr =
                        (self.queues[q].used_addr & !0xFFFF_FFFF) | value as u64;
                }
            }
            VIRTIO_MMIO_QUEUE_USED_HIGH => {
                let q = self.queue_sel as usize;
                if q < self.queues.len() {
                    self.queues[q].used_addr =
                        (self.queues[q].used_addr & 0xFFFF_FFFF) | ((value as u64) << 32);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use vm_memory::{GuestAddress, GuestMemoryMmap};

    // ── helpers ──────────────────────────────────────────────────────────────

    /// Create a 4 MB guest memory region starting at physical address 0.
    fn make_mem() -> GuestMemoryMmap<()> {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 4 << 20)]).unwrap()
    }

    /// Configure a single queue on `device` (queue `idx`) as ready.
    /// `desc_addr`, `avail_addr`, and `used_addr` are guest-physical addresses
    /// written through the MMIO registers, mirroring what the kernel driver does.
    fn configure_queue(
        device: &mut VirtioMmioDevice,
        mem: &GuestMemoryMmap<()>,
        idx: u32,
        desc_addr: u64,
        avail_addr: u64,
        used_addr: u64,
    ) {
        device.mmio_write(VIRTIO_MMIO_QUEUE_SEL, idx, mem);
        device.mmio_write(VIRTIO_MMIO_QUEUE_NUM, QUEUE_SIZE as u32, mem);
        device.mmio_write(VIRTIO_MMIO_QUEUE_DESC_LOW, desc_addr as u32, mem);
        device.mmio_write(VIRTIO_MMIO_QUEUE_DESC_HIGH, (desc_addr >> 32) as u32, mem);
        device.mmio_write(VIRTIO_MMIO_QUEUE_AVAIL_LOW, avail_addr as u32, mem);
        device.mmio_write(VIRTIO_MMIO_QUEUE_AVAIL_HIGH, (avail_addr >> 32) as u32, mem);
        device.mmio_write(VIRTIO_MMIO_QUEUE_USED_LOW, used_addr as u32, mem);
        device.mmio_write(VIRTIO_MMIO_QUEUE_USED_HIGH, (used_addr >> 32) as u32, mem);
        device.mmio_write(VIRTIO_MMIO_QUEUE_READY, 1, mem);
    }

    // ── mock backend (no call tracking needed) ───────────────────────────────

    struct MockBackend {
        device_id: u32,
        num_queues: usize,
    }

    impl MockBackend {
        fn new(device_id: u32, num_queues: usize) -> Self {
            Self { device_id, num_queues }
        }
    }

    impl VirtioBackend for MockBackend {
        fn device_id(&self) -> u32 {
            self.device_id
        }

        fn num_queues(&self) -> usize {
            self.num_queues
        }

        fn process_queue(
            &mut self,
            _queue_idx: usize,
            _queue: &VirtqueueState,
            _mem: &GuestMemoryMmap<()>,
        ) {
        }
    }

    fn make_device(num_queues: usize) -> VirtioMmioDevice {
        VirtioMmioDevice::new(Box::new(MockBackend::new(0xAB, num_queues)))
    }

    // ── tracking backend (records which queue indices were processed) ─────────

    struct TrackingBackend {
        device_id: u32,
        num_queues: usize,
        processed: Arc<Mutex<Vec<usize>>>,
    }

    impl TrackingBackend {
        fn new(device_id: u32, num_queues: usize) -> (Self, Arc<Mutex<Vec<usize>>>) {
            let log = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    device_id,
                    num_queues,
                    processed: Arc::clone(&log),
                },
                log,
            )
        }
    }

    impl VirtioBackend for TrackingBackend {
        fn device_id(&self) -> u32 {
            self.device_id
        }

        fn num_queues(&self) -> usize {
            self.num_queues
        }

        fn process_queue(
            &mut self,
            queue_idx: usize,
            _queue: &VirtqueueState,
            _mem: &GuestMemoryMmap<()>,
        ) {
            self.processed.lock().unwrap().push(queue_idx);
        }
    }

    fn make_tracking_device(num_queues: usize) -> (VirtioMmioDevice, Arc<Mutex<Vec<usize>>>) {
        let (backend, log) = TrackingBackend::new(42, num_queues);
        (VirtioMmioDevice::new(Box::new(backend)), log)
    }

    // ── MMIO read tests ───────────────────────────────────────────────────────

    #[test]
    fn magic_read_returns_virtio_magic_value() {
        let device = make_device(1);
        assert_eq!(
            device.mmio_read(VIRTIO_MMIO_MAGIC),
            VIRTIO_MMIO_MAGIC_VALUE,
            "MAGIC register must return the VirtIO magic value"
        );
    }

    #[test]
    fn version_read_returns_2() {
        let device = make_device(1);
        assert_eq!(
            device.mmio_read(VIRTIO_MMIO_VERSION),
            2,
            "VERSION register must return 2 (modern transport)"
        );
    }

    #[test]
    fn device_id_read_returns_backend_device_id() {
        let (backend, _) = TrackingBackend::new(VIRTIO_DEVICE_CONSOLE, 2);
        let device = VirtioMmioDevice::new(Box::new(backend));
        assert_eq!(
            device.mmio_read(VIRTIO_MMIO_DEVICE_ID),
            VIRTIO_DEVICE_CONSOLE,
            "DEVICE_ID must reflect the backend's device_id()"
        );
    }

    #[test]
    fn vendor_id_read_returns_sumi_vendor_id() {
        let device = make_device(1);
        assert_eq!(
            device.mmio_read(VIRTIO_MMIO_VENDOR_ID),
            SUMI_VENDOR_ID,
            "VENDOR_ID must return the sumi vendor constant"
        );
    }

    #[test]
    fn queue_num_max_read_returns_queue_size() {
        let device = make_device(1);
        assert_eq!(
            device.mmio_read(VIRTIO_MMIO_QUEUE_NUM_MAX),
            QUEUE_SIZE as u32,
            "QUEUE_NUM_MAX must return QUEUE_SIZE"
        );
    }

    #[test]
    fn unknown_mmio_offset_read_returns_zero() {
        let device = make_device(1);
        assert_eq!(
            device.mmio_read(0xFFFF),
            0,
            "unknown MMIO offset must return 0"
        );
    }

    // ── queue readiness tests ─────────────────────────────────────────────────

    #[test]
    fn queue_not_ready_by_default() {
        let mut device = make_device(2);
        let mem = make_mem();
        device.mmio_write(VIRTIO_MMIO_QUEUE_SEL, 0, &mem);
        assert_eq!(
            device.mmio_read(VIRTIO_MMIO_QUEUE_READY),
            0,
            "queue must not be ready before QUEUE_READY is written"
        );
    }

    #[test]
    fn queue_ready_after_write_one() {
        let mut device = make_device(2);
        let mem = make_mem();
        device.mmio_write(VIRTIO_MMIO_QUEUE_SEL, 0, &mem);
        device.mmio_write(VIRTIO_MMIO_QUEUE_READY, 1, &mem);
        assert_eq!(
            device.mmio_read(VIRTIO_MMIO_QUEUE_READY),
            1,
            "queue must be ready after writing 1 to QUEUE_READY"
        );
    }

    #[test]
    fn queue_ready_cleared_after_write_zero() {
        let mut device = make_device(2);
        let mem = make_mem();
        device.mmio_write(VIRTIO_MMIO_QUEUE_SEL, 0, &mem);
        device.mmio_write(VIRTIO_MMIO_QUEUE_READY, 1, &mem);
        device.mmio_write(VIRTIO_MMIO_QUEUE_READY, 0, &mem);
        assert_eq!(
            device.mmio_read(VIRTIO_MMIO_QUEUE_READY),
            0,
            "queue must not be ready after writing 0 to QUEUE_READY"
        );
    }

    #[test]
    fn out_of_bounds_queue_sel_ready_read_returns_zero() {
        let mut device = make_device(1);
        let mem = make_mem();
        // Select queue 99 which does not exist (device has only 1 queue).
        device.mmio_write(VIRTIO_MMIO_QUEUE_SEL, 99, &mem);
        assert_eq!(
            device.mmio_read(VIRTIO_MMIO_QUEUE_READY),
            0,
            "QUEUE_READY read with OOB queue_sel must return 0"
        );
    }

    // ── queue address assembly tests ──────────────────────────────────────────

    #[test]
    fn desc_64bit_address_assembled_from_two_32bit_writes() {
        let mut device = make_device(1);
        let mem = make_mem();
        let addr: u64 = 0x0000_0001_DEAD_BEEF;

        device.mmio_write(VIRTIO_MMIO_QUEUE_SEL, 0, &mem);
        device.mmio_write(VIRTIO_MMIO_QUEUE_DESC_LOW, addr as u32, &mem);
        device.mmio_write(VIRTIO_MMIO_QUEUE_DESC_HIGH, (addr >> 32) as u32, &mem);

        assert_eq!(
            device.queues[0].desc_addr,
            addr,
            "desc_addr must equal the 64-bit value assembled from LOW/HIGH writes"
        );
    }

    #[test]
    fn avail_64bit_address_assembled_from_two_32bit_writes() {
        let mut device = make_device(1);
        let mem = make_mem();
        let addr: u64 = 0x0000_0002_CAFE_BABE;

        device.mmio_write(VIRTIO_MMIO_QUEUE_SEL, 0, &mem);
        device.mmio_write(VIRTIO_MMIO_QUEUE_AVAIL_LOW, addr as u32, &mem);
        device.mmio_write(VIRTIO_MMIO_QUEUE_AVAIL_HIGH, (addr >> 32) as u32, &mem);

        assert_eq!(
            device.queues[0].avail_addr,
            addr,
            "avail_addr must equal the 64-bit value assembled from LOW/HIGH writes"
        );
    }

    #[test]
    fn used_64bit_address_assembled_from_two_32bit_writes() {
        let mut device = make_device(1);
        let mem = make_mem();
        let addr: u64 = 0x0000_0003_1234_5678;

        device.mmio_write(VIRTIO_MMIO_QUEUE_SEL, 0, &mem);
        device.mmio_write(VIRTIO_MMIO_QUEUE_USED_LOW, addr as u32, &mem);
        device.mmio_write(VIRTIO_MMIO_QUEUE_USED_HIGH, (addr >> 32) as u32, &mem);

        assert_eq!(
            device.queues[0].used_addr,
            addr,
            "used_addr must equal the 64-bit value assembled from LOW/HIGH writes"
        );
    }

    // ── queue notify / process_queue dispatch tests ───────────────────────────

    #[test]
    fn queue_notify_calls_process_queue_when_queue_is_ready() {
        let (mut device, log) = make_tracking_device(2);
        let mem = make_mem();

        configure_queue(&mut device, &mem, 0, 0x1000, 0x2000, 0x3000);
        configure_queue(&mut device, &mem, 1, 0x4000, 0x5000, 0x6000);

        device.mmio_write(VIRTIO_MMIO_QUEUE_NOTIFY, 1, &mem);

        let calls = log.lock().unwrap();
        assert_eq!(
            calls.as_slice(),
            &[1usize],
            "process_queue must be called exactly once with queue index 1"
        );
    }

    #[test]
    fn queue_notify_does_not_call_process_queue_when_queue_is_not_ready() {
        let (mut device, log) = make_tracking_device(2);
        let mem = make_mem();

        // Deliberately do NOT call configure_queue (which sets QUEUE_READY=1).
        device.mmio_write(VIRTIO_MMIO_QUEUE_NOTIFY, 0, &mem);

        let calls = log.lock().unwrap();
        assert!(
            calls.is_empty(),
            "process_queue must NOT be called when the queue is not ready (calls: {calls:?})"
        );
    }

    #[test]
    fn queue_notify_after_queue_marked_not_ready_does_not_call_process_queue() {
        let (mut device, log) = make_tracking_device(1);
        let mem = make_mem();

        // Make the queue ready, then mark it not ready again.
        configure_queue(&mut device, &mem, 0, 0x1000, 0x2000, 0x3000);
        device.mmio_write(VIRTIO_MMIO_QUEUE_SEL, 0, &mem);
        device.mmio_write(VIRTIO_MMIO_QUEUE_READY, 0, &mem);

        device.mmio_write(VIRTIO_MMIO_QUEUE_NOTIFY, 0, &mem);

        let calls = log.lock().unwrap();
        assert!(
            calls.is_empty(),
            "process_queue must NOT be called after queue was marked not ready"
        );
    }

    #[test]
    fn queue_notify_out_of_bounds_index_is_silently_ignored() {
        let (mut device, log) = make_tracking_device(2);
        let mem = make_mem();

        // Index 99 is well beyond the 2-queue device — must not panic.
        device.mmio_write(VIRTIO_MMIO_QUEUE_NOTIFY, 99, &mem);

        let calls = log.lock().unwrap();
        assert!(
            calls.is_empty(),
            "process_queue must NOT be called for an out-of-bounds queue index"
        );
    }

    #[test]
    fn process_queue_receives_correct_queue_state_addresses() {
        // Backend that captures the VirtqueueState fields it was called with via shared state.
        struct CapturingBackend {
            captured: Arc<Mutex<Option<(u64, u64, u64)>>>,
        }

        impl VirtioBackend for CapturingBackend {
            fn device_id(&self) -> u32 { 0 }
            fn num_queues(&self) -> usize { 1 }
            fn process_queue(
                &mut self,
                _queue_idx: usize,
                queue: &VirtqueueState,
                _mem: &GuestMemoryMmap<()>,
            ) {
                *self.captured.lock().unwrap() =
                    Some((queue.desc_addr, queue.avail_addr, queue.used_addr));
            }
        }

        let captured = Arc::new(Mutex::new(None));
        let backend = CapturingBackend { captured: Arc::clone(&captured) };
        let mut device = VirtioMmioDevice::new(Box::new(backend));
        let mem = make_mem();

        let desc: u64  = 0x0000_ABCD_1234_0000;
        let avail: u64 = 0x0000_ABCD_2000_0000;
        let used: u64  = 0x0000_ABCD_3000_0000;

        configure_queue(&mut device, &mem, 0, desc, avail, used);
        device.mmio_write(VIRTIO_MMIO_QUEUE_NOTIFY, 0, &mem);

        let result = captured.lock().unwrap().expect(
            "process_queue was not called even though the queue was ready"
        );
        assert_eq!(result.0, desc,  "desc_addr passed to process_queue must match MMIO writes");
        assert_eq!(result.1, avail, "avail_addr passed to process_queue must match MMIO writes");
        assert_eq!(result.2, used,  "used_addr passed to process_queue must match MMIO writes");
    }

    // ── status register tests ─────────────────────────────────────────────────

    #[test]
    fn status_write_and_read_round_trips() {
        let mut device = make_device(1);
        let mem = make_mem();

        let status = VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK;
        device.mmio_write(VIRTIO_MMIO_STATUS, status, &mem);
        assert_eq!(
            device.mmio_read(VIRTIO_MMIO_STATUS),
            status,
            "STATUS register must read back what was written"
        );
    }

    // ── queue_sel isolation tests ─────────────────────────────────────────────

    #[test]
    fn queue_sel_isolates_per_queue_settings() {
        let mut device = make_device(2);
        let mem = make_mem();

        configure_queue(&mut device, &mem, 0, 0x1000, 0x2000, 0x3000);
        configure_queue(&mut device, &mem, 1, 0xA000, 0xB000, 0xC000);

        assert_eq!(
            device.queues[0].desc_addr, 0x1000,
            "queue 0 desc_addr must not be overwritten by queue 1 configuration"
        );
        assert_eq!(
            device.queues[1].desc_addr, 0xA000,
            "queue 1 desc_addr must reflect its own configuration"
        );
    }

    // ── interrupt status tests ────────────────────────────────────────────────

    #[test]
    fn interrupt_status_always_returns_zero() {
        let device = make_device(1);
        assert_eq!(
            device.mmio_read(VIRTIO_MMIO_INTERRUPT_STATUS),
            0,
            "INTERRUPT_STATUS must return 0 (interrupts not used)"
        );
    }
}
