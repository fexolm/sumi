use core::mem::size_of;

use sumi_abi::{
    address::{DirectMap, PhysicalAddr, VirtualAddr},
    virtio::*,
};

use crate::memory::alloc::kmalloc::KernelAllocator;
use crate::memory::errors::MemoryError;

pub struct Virtqueue {
    desc: VirtualAddr,
    avail: VirtualAddr,
    used: VirtualAddr,
    desc_phys: PhysicalAddr,
    avail_phys: PhysicalAddr,
    used_phys: PhysicalAddr,
    free_head: u16,
    num_free: u16,
    last_used_idx: u16,
}

impl Virtqueue {
    /// Allocate and initialize a split virtqueue from the kernel allocator.
    pub fn new<DM: DirectMap>(kalloc: &KernelAllocator<DM>) -> Result<Self, MemoryError> {
        let dm = kalloc.direct_map();

        let desc_size = QUEUE_SIZE as usize * size_of::<VirtqDesc>();
        let avail_size = 4 + QUEUE_SIZE as usize * 2;
        let used_size = 4 + QUEUE_SIZE as usize * size_of::<VirtqUsedElem>();
        let total = desc_size + avail_size + used_size;

        let base_phys = kalloc.calloc(total)?;
        let base_virt = base_phys.to_virtual(dm);

        let desc_phys = base_phys;
        let desc_virt = base_virt;
        let avail_phys = base_phys.add(desc_size);
        let avail_virt = base_virt.add(desc_size);
        let used_phys = base_phys.add(desc_size + avail_size);
        let used_virt = base_virt.add(desc_size + avail_size);

        // Initialize descriptor free list: each next points to the following index
        for i in 0..QUEUE_SIZE {
            let desc_ptr = desc_virt
                .add(i as usize * size_of::<VirtqDesc>())
                .as_ptr::<VirtqDesc>();
            // SAFETY: desc_ptr points into the zeroed, allocated descriptor table.
            unsafe {
                (*desc_ptr).next = i + 1;
                (*desc_ptr).flags = 0;
            }
        }

        // Set VIRTQ_AVAIL_F_NO_INTERRUPT — we never need host→guest notifications
        // SAFETY: avail_virt points to zeroed, allocated available ring memory.
        unsafe {
            let avail = avail_virt.as_ptr::<VirtqAvail>();
            (*avail).flags = VIRTQ_AVAIL_F_NO_INTERRUPT;
            (*avail).idx = 0;
        }

        Ok(Self {
            desc: desc_virt,
            avail: avail_virt,
            used: used_virt,
            desc_phys,
            avail_phys,
            used_phys,
            free_head: 0,
            num_free: QUEUE_SIZE,
            last_used_idx: 0,
        })
    }

    pub fn desc_phys(&self) -> PhysicalAddr {
        self.desc_phys
    }
    pub fn avail_phys(&self) -> PhysicalAddr {
        self.avail_phys
    }
    pub fn used_phys(&self) -> PhysicalAddr {
        self.used_phys
    }

    /// Get a mutable pointer to a descriptor by index.
    #[allow(clippy::mut_from_ref)] // Mutation goes through MMIO raw pointers, not &self
    pub fn desc_mut(&self, idx: u16) -> &mut VirtqDesc {
        // SAFETY: idx < QUEUE_SIZE is enforced by alloc_desc returning valid indices.
        unsafe {
            &mut *self
                .desc
                .add(idx as usize * size_of::<VirtqDesc>())
                .as_ptr::<VirtqDesc>()
        }
    }

    fn desc_ref(&self, idx: u16) -> &VirtqDesc {
        // SAFETY: idx < QUEUE_SIZE is enforced by alloc_desc returning valid indices.
        unsafe {
            &*self
                .desc
                .add(idx as usize * size_of::<VirtqDesc>())
                .as_ptr::<VirtqDesc>()
        }
    }

    /// Allocate a descriptor from the free list.
    pub fn alloc_desc(&mut self) -> Option<u16> {
        if self.num_free == 0 {
            return None;
        }
        let idx = self.free_head;
        self.free_head = self.desc_ref(idx).next;
        self.num_free -= 1;
        Some(idx)
    }

    /// Free a single descriptor back to the free list.
    pub fn free_desc(&mut self, idx: u16) {
        let desc = self.desc_mut(idx);
        desc.flags = 0;
        desc.next = self.free_head;
        self.free_head = idx;
        self.num_free += 1;
    }

    /// Free a chain of descriptors starting from head.
    pub fn free_chain(&mut self, head: u16) {
        let mut idx = head;
        loop {
            let flags = self.desc_ref(idx).flags;
            let next = self.desc_ref(idx).next;
            self.free_desc(idx);
            if flags & VIRTQ_DESC_F_NEXT == 0 {
                break;
            }
            idx = next;
        }
    }

    /// Add a descriptor chain head to the available ring.
    pub fn submit(&mut self, head: u16) {
        // SAFETY: avail points to the allocated available ring.
        let avail = unsafe { &mut *self.avail.as_ptr::<VirtqAvail>() };
        let idx = avail.idx;
        avail.ring[(idx % QUEUE_SIZE) as usize] = head;
        // Ensure descriptor writes are visible before idx update
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
        avail.idx = idx.wrapping_add(1);
    }

    /// Read the next completed descriptor from the used ring, if any.
    pub fn complete(&mut self) -> Option<VirtqUsedElem> {
        // SAFETY: used points to the allocated used ring.
        let used = unsafe { &*self.used.as_ptr::<VirtqUsed>() };
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
        if self.last_used_idx == used.idx {
            return None;
        }
        let elem = used.ring[(self.last_used_idx % QUEUE_SIZE) as usize];
        self.last_used_idx = self.last_used_idx.wrapping_add(1);
        Some(elem)
    }
}
