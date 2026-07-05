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
        Self {
            device_id,
            num_queues,
        }
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

// ── queue readiness tests ─────────────────────────────────────────────────

#[test]
fn queue_ready_register_tracks_selected_queue() {
    let mut device = make_device(2);
    let mem = make_mem();
    device.mmio_write(VIRTIO_MMIO_QUEUE_SEL, 0, &mem);
    assert_eq!(device.mmio_read(VIRTIO_MMIO_QUEUE_READY), 0);
    device.mmio_write(VIRTIO_MMIO_QUEUE_READY, 1, &mem);
    assert_eq!(device.mmio_read(VIRTIO_MMIO_QUEUE_READY), 1);
    device.mmio_write(VIRTIO_MMIO_QUEUE_READY, 0, &mem);
    assert_eq!(device.mmio_read(VIRTIO_MMIO_QUEUE_READY), 0);
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
fn queue_addresses_assemble_from_two_32bit_writes() {
    let mut device = make_device(1);
    let mem = make_mem();

    let desc: u64 = 0x0000_0001_DEAD_BEEF;
    device.mmio_write(VIRTIO_MMIO_QUEUE_SEL, 0, &mem);
    device.mmio_write(VIRTIO_MMIO_QUEUE_DESC_LOW, desc as u32, &mem);
    device.mmio_write(VIRTIO_MMIO_QUEUE_DESC_HIGH, (desc >> 32) as u32, &mem);
    assert_eq!(device.queues[0].desc_addr, desc);

    let avail: u64 = 0x0000_0002_CAFE_BABE;
    device.mmio_write(VIRTIO_MMIO_QUEUE_AVAIL_LOW, avail as u32, &mem);
    device.mmio_write(VIRTIO_MMIO_QUEUE_AVAIL_HIGH, (avail >> 32) as u32, &mem);
    assert_eq!(device.queues[0].avail_addr, avail);

    let used: u64 = 0x0000_0003_1234_5678;
    device.mmio_write(VIRTIO_MMIO_QUEUE_USED_LOW, used as u32, &mem);
    device.mmio_write(VIRTIO_MMIO_QUEUE_USED_HIGH, (used >> 32) as u32, &mem);
    assert_eq!(device.queues[0].used_addr, used);
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
        fn device_id(&self) -> u32 {
            0
        }
        fn num_queues(&self) -> usize {
            1
        }
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
    let backend = CapturingBackend {
        captured: Arc::clone(&captured),
    };
    let mut device = VirtioMmioDevice::new(Box::new(backend));
    let mem = make_mem();

    let desc: u64 = 0x0000_ABCD_1234_0000;
    let avail: u64 = 0x0000_ABCD_2000_0000;
    let used: u64 = 0x0000_ABCD_3000_0000;

    configure_queue(&mut device, &mem, 0, desc, avail, used);
    device.mmio_write(VIRTIO_MMIO_QUEUE_NOTIFY, 0, &mem);

    let result = captured
        .lock()
        .unwrap()
        .expect("process_queue was not called even though the queue was ready");
    assert_eq!(
        result.0, desc,
        "desc_addr passed to process_queue must match MMIO writes"
    );
    assert_eq!(
        result.1, avail,
        "avail_addr passed to process_queue must match MMIO writes"
    );
    assert_eq!(
        result.2, used,
        "used_addr passed to process_queue must match MMIO writes"
    );
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

// ── device_features / config-space wiring (added for virtio-net) ──────────

struct FeatureBackend {
    features: u64,
    config: [u8; 8],
}

impl VirtioBackend for FeatureBackend {
    fn device_id(&self) -> u32 {
        1
    }
    fn num_queues(&self) -> usize {
        2
    }
    fn process_queue(&mut self, _queue_idx: usize, _queue: &VirtqueueState, _mem: &GuestMemoryMmap<()>) {
    }
    fn device_features(&self) -> u64 {
        self.features
    }
    fn read_config(&self, offset: usize) -> u32 {
        match self.config.get(offset..offset + 4) {
            Some(word) => u32::from_le_bytes(word.try_into().unwrap()),
            None => 0,
        }
    }
}

#[test]
fn device_features_sel_selects_low_and_high_32_bits() {
    let backend = FeatureBackend {
        features: 0x0000_0020_0000_0001,
        config: [0; 8],
    };
    let mut device = VirtioMmioDevice::new(Box::new(backend));
    let mem = make_mem();

    device.mmio_write(VIRTIO_MMIO_DEVICE_FEATURES_SEL, 0, &mem);
    assert_eq!(device.mmio_read(VIRTIO_MMIO_DEVICE_FEATURES), 0x0000_0001);

    device.mmio_write(VIRTIO_MMIO_DEVICE_FEATURES_SEL, 1, &mem);
    assert_eq!(device.mmio_read(VIRTIO_MMIO_DEVICE_FEATURES), 0x0000_0020);
}

#[test]
fn default_device_features_is_zero() {
    // MockBackend does not override device_features(); the trait default
    // (0, "no features") must be what mmio_read reports.
    let device = make_device(1);
    assert_eq!(device.mmio_read(VIRTIO_MMIO_DEVICE_FEATURES), 0);
}

#[test]
fn config_space_read_dispatches_to_backend_read_config() {
    let backend = FeatureBackend {
        features: 0,
        config: [0x02, 0x00, 0x00, 0x00, 0x00, 0x15, 0, 0],
    };
    let device = VirtioMmioDevice::new(Box::new(backend));
    assert_eq!(device.mmio_read(VIRTIO_MMIO_CONFIG), 0x0000_0002);
    assert_eq!(device.mmio_read(VIRTIO_MMIO_CONFIG + 4), 0x0000_1500);
}

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
