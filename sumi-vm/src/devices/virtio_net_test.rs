use super::*;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

// ── guest-memory virtqueue layout (mirrors virtio_console_test.rs) ────────

const DESC_BASE: u64 = 0x0000;
const AVAIL_BASE: u64 = 0x1000;
const USED_BASE: u64 = 0x2000;
const DATA_BASE: u64 = 0x3000;

fn make_mem() -> GuestMemoryMmap<()> {
    GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 4 << 20)]).unwrap()
}

fn make_queue() -> VirtqueueState {
    VirtqueueState {
        num: QUEUE_SIZE as u32,
        ready: true,
        desc_addr: DESC_BASE,
        avail_addr: AVAIL_BASE,
        used_addr: USED_BASE,
    }
}

fn write_desc(mem: &GuestMemoryMmap<()>, idx: u16, addr: u64, len: u32, flags: u16, next: u16) {
    let base = DESC_BASE + idx as u64 * 16;
    mem.write_slice(&addr.to_le_bytes(), GuestAddress(base))
        .unwrap();
    mem.write_slice(&len.to_le_bytes(), GuestAddress(base + 8))
        .unwrap();
    mem.write_slice(&flags.to_le_bytes(), GuestAddress(base + 12))
        .unwrap();
    mem.write_slice(&next.to_le_bytes(), GuestAddress(base + 14))
        .unwrap();
}

fn avail_push(mem: &GuestMemoryMmap<()>, head: u16, avail_idx: u16) {
    let ring_slot = AVAIL_BASE + 4 + (avail_idx % QUEUE_SIZE) as u64 * 2;
    mem.write_slice(&head.to_le_bytes(), GuestAddress(ring_slot))
        .unwrap();
    let new_idx = avail_idx.wrapping_add(1);
    mem.write_slice(&new_idx.to_le_bytes(), GuestAddress(AVAIL_BASE + 2))
        .unwrap();
}

fn used_idx(mem: &GuestMemoryMmap<()>) -> u16 {
    let mut buf = [0u8; 2];
    mem.read_slice(&mut buf, GuestAddress(USED_BASE + 2))
        .unwrap();
    u16::from_le_bytes(buf)
}

fn used_entry(mem: &GuestMemoryMmap<()>, slot: u16) -> (u32, u32) {
    let base = USED_BASE + 4 + (slot % QUEUE_SIZE) as u64 * 8;
    let mut id_buf = [0u8; 4];
    let mut len_buf = [0u8; 4];
    mem.read_slice(&mut id_buf, GuestAddress(base)).unwrap();
    mem.read_slice(&mut len_buf, GuestAddress(base + 4))
        .unwrap();
    (u32::from_le_bytes(id_buf), u32::from_le_bytes(len_buf))
}

const TEST_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x15];

fn make_backend() -> (VirtioNetBackend, Arc<GatewayChannel>) {
    let chan = GatewayChannel::new();
    (VirtioNetBackend::new(Arc::clone(&chan), TEST_MAC), chan)
}

// ── VirtioBackend metadata ─────────────────────────────────────────────────

#[test]
fn backend_advertises_net_device_id_and_two_queues() {
    let (backend, _chan) = make_backend();
    assert_eq!(backend.device_id(), VIRTIO_DEVICE_NET);
    assert_eq!(backend.num_queues(), 2);
}

#[test]
fn backend_device_features_offers_mac_and_version_1_only() {
    let (backend, _chan) = make_backend();
    let feat = backend.device_features();
    assert_ne!(feat & VIRTIO_NET_F_MAC, 0);
    assert_ne!(feat & VIRTIO_F_VERSION_1, 0);
    assert_eq!(feat & VIRTIO_NET_F_MRG_RXBUF, 0, "MRG_RXBUF must not be offered");
}

#[test]
fn backend_read_config_reports_mac_across_two_registers() {
    let (backend, _chan) = make_backend();
    let lo = backend.read_config(0).to_le_bytes();
    let hi = backend.read_config(4).to_le_bytes();
    assert_eq!(&lo, &TEST_MAC[0..4]);
    assert_eq!(&hi[0..2], &TEST_MAC[4..6]);
    assert_eq!(&hi[2..4], &[0, 0], "bytes past the 6-byte MAC must be zero");
}

// ── TX (queue 1): guest -> host, header stripped ───────────────────────────

#[test]
fn process_tx_strips_12_byte_header_and_forwards_payload() {
    let mem = make_mem();
    let queue = make_queue();
    let (mut backend, chan) = make_backend();

    let mut frame = [0u8; VIRTIO_NET_HDR_LEN + 5];
    frame[VIRTIO_NET_HDR_LEN..].copy_from_slice(b"hello");
    mem.write_slice(&frame, GuestAddress(DATA_BASE)).unwrap();
    write_desc(&mem, 0, DATA_BASE, frame.len() as u32, 0, 0);
    avail_push(&mem, 0, 0);

    backend.process_queue(1, &queue, &mem);

    assert_eq!(used_idx(&mem), 1);
    let (id, len) = used_entry(&mem, 0);
    assert_eq!(id, 0);
    assert_eq!(len, frame.len() as u32);

    let forwarded = chan.guest_to_host.lock().unwrap().pop_front();
    assert_eq!(forwarded.as_deref(), Some(b"hello".as_slice()));
}

#[test]
fn process_tx_short_frame_below_header_size_is_dropped_not_forwarded() {
    let mem = make_mem();
    let queue = make_queue();
    let (mut backend, chan) = make_backend();

    // Only 4 bytes total — shorter than the 12-byte header, must not panic
    // or forward a bogus (underflowed) frame.
    write_desc(&mem, 0, DATA_BASE, 4, 0, 0);
    avail_push(&mem, 0, 0);

    backend.process_queue(1, &queue, &mem);

    assert_eq!(used_idx(&mem), 1, "used ring still advances");
    assert!(chan.guest_to_host.lock().unwrap().is_empty());
}

// ── RX (queue 0): host -> guest, header prepended ──────────────────────────

#[test]
fn process_rx_fills_posted_buffer_with_header_and_frame() {
    let mem = make_mem();
    let queue = make_queue();
    let (mut backend, chan) = make_backend();

    chan.host_to_guest
        .lock()
        .unwrap()
        .push_back(b"world".to_vec());

    write_desc(&mem, 0, DATA_BASE, 64, VIRTQ_DESC_F_WRITE, 0);
    avail_push(&mem, 0, 0);

    backend.process_queue(0, &queue, &mem);

    assert_eq!(used_idx(&mem), 1);
    let (id, len) = used_entry(&mem, 0);
    assert_eq!(id, 0);
    assert_eq!(len, (VIRTIO_NET_HDR_LEN + 5) as u32);

    let mut hdr = [0u8; VIRTIO_NET_HDR_LEN];
    mem.read_slice(&mut hdr, GuestAddress(DATA_BASE)).unwrap();
    // num_buffers (last u16 field, offset 10) must be 1.
    assert_eq!(u16::from_le_bytes([hdr[10], hdr[11]]), 1);

    let mut payload = [0u8; 5];
    mem.read_slice(&mut payload, GuestAddress(DATA_BASE + VIRTIO_NET_HDR_LEN as u64))
        .unwrap();
    assert_eq!(&payload, b"world");

    assert!(chan.host_to_guest.lock().unwrap().is_empty());
}

#[test]
fn process_rx_with_no_frames_leaves_used_ring_untouched() {
    let mem = make_mem();
    let queue = make_queue();
    let (mut backend, _chan) = make_backend();

    write_desc(&mem, 0, DATA_BASE, 64, VIRTQ_DESC_F_WRITE, 0);
    avail_push(&mem, 0, 0);

    backend.process_queue(0, &queue, &mem);

    assert_eq!(
        used_idx(&mem),
        0,
        "no frames queued -> posted buffer must be left untouched"
    );
}

#[test]
fn process_rx_truncates_frame_larger_than_descriptor() {
    let mem = make_mem();
    let queue = make_queue();
    let (mut backend, chan) = make_backend();

    chan.host_to_guest
        .lock()
        .unwrap()
        .push_back(vec![0xAAu8; 100]);

    // Descriptor only has room for 12 (header) + 10 bytes of payload.
    write_desc(&mem, 0, DATA_BASE, (VIRTIO_NET_HDR_LEN + 10) as u32, VIRTQ_DESC_F_WRITE, 0);
    avail_push(&mem, 0, 0);

    backend.process_queue(0, &queue, &mem);

    let (_, len) = used_entry(&mem, 0);
    assert_eq!(len, (VIRTIO_NET_HDR_LEN + 10) as u32, "must truncate defensively, not overflow the descriptor");
}
