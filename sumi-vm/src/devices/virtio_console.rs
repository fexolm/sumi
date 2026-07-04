use std::io::{Read, Write};

use sumi_abi::virtio::*;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use super::virtio_mmio::{
    VirtioBackend, VirtqueueState, post_used, read_avail_head, read_avail_idx, read_desc,
};

#[derive(Default)]
pub struct VirtioConsoleBackend {
    last_avail_idx: [u16; 2],
}

impl VirtioConsoleBackend {
    pub fn new() -> Self {
        Self {
            last_avail_idx: [0; 2],
        }
    }

    // Queue 1 = transmitq: guest writes data, host reads it and sends to stdout.
    fn process_transmit(&mut self, queue: &VirtqueueState, mem: &GuestMemoryMmap<()>) {
        let avail_idx = read_avail_idx(queue, mem);

        while self.last_avail_idx[1] != avail_idx {
            let head = read_avail_head(queue, self.last_avail_idx[1], mem);

            // Collect all data from the descriptor chain and write to stdout.
            // Iterate at most QUEUE_SIZE times to guard against a malformed
            // or cyclic descriptor chain.
            let mut total_len = 0u32;
            let mut idx = head;
            for _ in 0..QUEUE_SIZE {
                let desc = read_desc(queue, idx, mem);
                let len = desc.len as usize;
                let mut stack_buf = [0u8; 4096];
                if len <= 4096 {
                    mem.read_slice(&mut stack_buf[..len], GuestAddress(desc.addr))
                        .unwrap();
                    std::io::stdout().write_all(&stack_buf[..len]).ok();
                } else {
                    let mut heap_buf = vec![0u8; len];
                    mem.read_slice(&mut heap_buf, GuestAddress(desc.addr))
                        .unwrap();
                    std::io::stdout().write_all(&heap_buf).ok();
                }
                total_len += desc.len;
                if desc.flags & VIRTQ_DESC_F_NEXT == 0 {
                    break;
                }
                idx = desc.next;
            }
            std::io::stdout().flush().ok();

            post_used(queue, head, total_len, mem);
            self.last_avail_idx[1] = self.last_avail_idx[1].wrapping_add(1);
        }
    }

    // Queue 0 = receiveq: host reads stdin and fills guest descriptors.
    fn process_receive(&mut self, queue: &VirtqueueState, mem: &GuestMemoryMmap<()>) {
        let avail_idx = read_avail_idx(queue, mem);

        while self.last_avail_idx[0] != avail_idx {
            let head = read_avail_head(queue, self.last_avail_idx[0], mem);

            let desc = read_desc(queue, head, mem);
            let len = desc.len as usize;
            let mut stack_buf = [0u8; 4096];
            let bytes_read = if len <= 4096 {
                let n = std::io::stdin().read(&mut stack_buf[..len]).unwrap_or(0);
                if n > 0 {
                    mem.write_slice(&stack_buf[..n], GuestAddress(desc.addr))
                        .unwrap();
                }
                n
            } else {
                let mut heap_buf = vec![0u8; len];
                let n = std::io::stdin().read(&mut heap_buf).unwrap_or(0);
                if n > 0 {
                    mem.write_slice(&heap_buf[..n], GuestAddress(desc.addr))
                        .unwrap();
                }
                n
            };

            post_used(queue, head, bytes_read as u32, mem);
            self.last_avail_idx[0] = self.last_avail_idx[0].wrapping_add(1);
        }
    }
}

impl VirtioBackend for VirtioConsoleBackend {
    fn device_id(&self) -> u32 {
        sumi_abi::virtio::VIRTIO_DEVICE_CONSOLE
    }

    fn num_queues(&self) -> usize {
        2
    }

    fn process_queue(
        &mut self,
        queue_idx: usize,
        queue: &VirtqueueState,
        mem: &GuestMemoryMmap<()>,
    ) {
        match queue_idx {
            0 => self.process_receive(queue, mem),
            1 => self.process_transmit(queue, mem),
            _ => {}
        }
    }
}

#[cfg(test)]
#[path = "virtio_console_test.rs"]
mod virtio_console_test;
