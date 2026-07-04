use super::*;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

// ── guest-memory virtqueue layout ─────────────────────────────────────────
//
// Layout within a 4 MB region:
//   0x0000 .. 0x0FFF  descriptor table  (256 * 16 bytes = 4096 bytes)
//   0x1000 .. 0x17FF  available ring     (4 + 256 * 2 = 516 bytes, fits easily)
//   0x2000 .. 0x2FFF  used ring          (4 + 256 * 8 = 2052 bytes, fits)
//   0x3000 ..         data region

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

/// Write a descriptor into the descriptor table at slot `idx`.
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

/// Append `head` to the available ring and bump the available index.
fn avail_push(mem: &GuestMemoryMmap<()>, head: u16, avail_idx: u16) {
    let ring_slot = AVAIL_BASE + 4 + (avail_idx % QUEUE_SIZE) as u64 * 2;
    mem.write_slice(&head.to_le_bytes(), GuestAddress(ring_slot))
        .unwrap();
    let new_idx = avail_idx.wrapping_add(1);
    mem.write_slice(&new_idx.to_le_bytes(), GuestAddress(AVAIL_BASE + 2))
        .unwrap();
}

/// Read the used ring's `idx` field (little-endian u16 at used_addr + 2).
fn used_idx(mem: &GuestMemoryMmap<()>) -> u16 {
    let mut buf = [0u8; 2];
    mem.read_slice(&mut buf, GuestAddress(USED_BASE + 2))
        .unwrap();
    u16::from_le_bytes(buf)
}

/// Read the used ring entry at slot `slot` — returns (id, len).
fn used_entry(mem: &GuestMemoryMmap<()>, slot: u16) -> (u32, u32) {
    let base = USED_BASE + 4 + (slot % QUEUE_SIZE) as u64 * 8;
    let mut id_buf = [0u8; 4];
    let mut len_buf = [0u8; 4];
    mem.read_slice(&mut id_buf, GuestAddress(base)).unwrap();
    mem.read_slice(&mut len_buf, GuestAddress(base + 4))
        .unwrap();
    (u32::from_le_bytes(id_buf), u32::from_le_bytes(len_buf))
}

// ── transmit (queue 1) tests ──────────────────────────────────────────────

#[test]
fn transmit_zero_available_descriptors_leaves_used_ring_unchanged() {
    let mem = make_mem();
    let queue = make_queue();
    // avail_idx is 0 by default (zeroed memory); backend last_avail_idx[1] = 0.
    let mut backend = VirtioConsoleBackend::new();

    backend.process_queue(1, &queue, &mem);

    assert_eq!(
        used_idx(&mem),
        0,
        "used ring idx must not advance when no descriptors are available"
    );
}

#[test]
fn transmit_single_descriptor_posts_used_entry_with_correct_head_and_length() {
    let mem = make_mem();
    let queue = make_queue();

    // Data: write "hello" at DATA_BASE.
    let payload = b"hello";
    mem.write_slice(payload, GuestAddress(DATA_BASE)).unwrap();

    // Descriptor 0: points to DATA_BASE, 5 bytes, no NEXT.
    write_desc(&mem, 0, DATA_BASE, payload.len() as u32, 0, 0);
    // Submit descriptor 0 to avail ring.
    avail_push(&mem, 0, 0);

    let mut backend = VirtioConsoleBackend::new();
    backend.process_queue(1, &queue, &mem);

    assert_eq!(
        used_idx(&mem),
        1,
        "used ring idx must advance by 1 after one descriptor"
    );
    let (id, len) = used_entry(&mem, 0);
    assert_eq!(id, 0, "used entry id must be the head descriptor index (0)");
    assert_eq!(len, 5, "used entry len must equal the descriptor length");
}

#[test]
fn transmit_two_sequential_descriptors_advance_used_ring_twice() {
    let mem = make_mem();
    let queue = make_queue();

    write_desc(&mem, 0, DATA_BASE, 4, 0, 0);
    write_desc(&mem, 1, DATA_BASE + 16, 8, 0, 0);
    avail_push(&mem, 0, 0);
    avail_push(&mem, 1, 1);

    let mut backend = VirtioConsoleBackend::new();
    backend.process_queue(1, &queue, &mem);

    assert_eq!(
        used_idx(&mem),
        2,
        "used ring idx must advance by 2 after two descriptors"
    );
    let (id0, len0) = used_entry(&mem, 0);
    let (id1, len1) = used_entry(&mem, 1);
    assert_eq!(
        (id0, len0),
        (0, 4),
        "first used entry must record head=0, len=4"
    );
    assert_eq!(
        (id1, len1),
        (1, 8),
        "second used entry must record head=1, len=8"
    );
}

#[test]
fn transmit_chained_descriptors_report_total_length_in_used_entry() {
    // Descriptor chain: desc 0 -> desc 1 (NEXT), total 3 + 5 = 8 bytes.
    let mem = make_mem();
    let queue = make_queue();

    write_desc(&mem, 0, DATA_BASE, 3, VIRTQ_DESC_F_NEXT, 1);
    write_desc(&mem, 1, DATA_BASE + 16, 5, 0, 0);
    avail_push(&mem, 0, 0); // submit head=0

    let mut backend = VirtioConsoleBackend::new();
    backend.process_queue(1, &queue, &mem);

    assert_eq!(
        used_idx(&mem),
        1,
        "used ring idx must advance once for a 2-desc chain"
    );
    let (id, len) = used_entry(&mem, 0);
    assert_eq!(id, 0, "used entry id must be the chain head");
    assert_eq!(
        len, 8,
        "used entry len must be sum of all descriptors in chain (3 + 5)"
    );
}

#[test]
fn transmit_chain_bounded_at_queue_size_iterations() {
    // Build a chain that cycles: desc 0 -> desc 1 -> desc 0 -> ...
    // The loop must terminate after at most QUEUE_SIZE iterations and not hang.
    let mem = make_mem();
    let queue = make_queue();

    // desc 0 -> desc 1 with NEXT, desc 1 -> desc 0 with NEXT (cycle).
    write_desc(&mem, 0, DATA_BASE, 1, VIRTQ_DESC_F_NEXT, 1);
    write_desc(&mem, 1, DATA_BASE + 16, 1, VIRTQ_DESC_F_NEXT, 0);
    avail_push(&mem, 0, 0);

    let mut backend = VirtioConsoleBackend::new();
    // Must complete without hanging or panicking.
    backend.process_queue(1, &queue, &mem);

    // The entry should have been posted to used (we don't care about total_len
    // for a cyclic chain — just that it terminated).
    assert_eq!(
        used_idx(&mem),
        1,
        "used ring must be advanced even with a cyclic chain"
    );
}

#[test]
fn transmit_zero_length_descriptor_is_processed_without_panic() {
    // A descriptor with len=0 must not cause a panic or out-of-bounds access.
    let mem = make_mem();
    let queue = make_queue();

    write_desc(&mem, 0, DATA_BASE, 0, 0, 0);
    avail_push(&mem, 0, 0);

    let mut backend = VirtioConsoleBackend::new();
    backend.process_queue(1, &queue, &mem);

    assert_eq!(
        used_idx(&mem),
        1,
        "zero-length descriptor must still advance the used ring"
    );
    let (id, len) = used_entry(&mem, 0);
    assert_eq!(id, 0, "used entry id must be 0");
    assert_eq!(
        len, 0,
        "used entry len must be 0 for a zero-length descriptor"
    );
}

#[test]
fn transmit_descriptor_larger_than_stack_buffer_is_handled() {
    // desc.len > 4096 triggers the heap-buf path.
    let data_len: u32 = 8192;
    let mem = make_mem();
    let queue = make_queue();

    // DATA_BASE area needs to be large enough; our 4 MB region is fine.
    write_desc(&mem, 0, DATA_BASE, data_len, 0, 0);
    avail_push(&mem, 0, 0);

    let mut backend = VirtioConsoleBackend::new();
    backend.process_queue(1, &queue, &mem);

    assert_eq!(
        used_idx(&mem),
        1,
        "large descriptor must still advance the used ring"
    );
    let (_, len) = used_entry(&mem, 0);
    assert_eq!(
        len, data_len,
        "used entry len must equal the large descriptor length"
    );
}

// ── backend VirtioBackend trait tests ─────────────────────────────────────

#[test]
fn backend_device_id_is_virtio_device_console() {
    let backend = VirtioConsoleBackend::new();
    assert_eq!(
        backend.device_id(),
        VIRTIO_DEVICE_CONSOLE,
        "VirtioConsoleBackend must advertise VIRTIO_DEVICE_CONSOLE"
    );
}

#[test]
fn backend_num_queues_is_2() {
    let backend = VirtioConsoleBackend::new();
    assert_eq!(
        backend.num_queues(),
        2,
        "VirtioConsoleBackend must expose 2 queues (receiveq + transmitq)"
    );
}

#[test]
fn process_queue_unknown_index_does_not_panic() {
    let mem = make_mem();
    let queue = make_queue();
    let mut backend = VirtioConsoleBackend::new();
    // queue_idx 2 has no arm in the match — must not panic.
    backend.process_queue(2, &queue, &mem);
}
