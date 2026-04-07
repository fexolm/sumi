use core::mem::size_of;
use core::sync::atomic::{AtomicU64, Ordering};

use sumi_abi::{
    address::{DirectMap, PhysicalAddr, VirtualAddr},
    arch::layout::DIRECT_MAP_OFFSET,
    fuse::*,
    virtio::*,
};

use crate::drivers::virtio::{mmio, virtqueue::Virtqueue};
use crate::memory::alloc::kmalloc::KernelAllocator;

struct DescSpec {
    addr: PhysicalAddr,
    len: u32,
    writable: bool,
}

pub struct VirtioFsClient {
    queue: spin::Mutex<Virtqueue>,
    mmio_base: VirtualAddr,
    max_write: u32,
    next_unique: AtomicU64,
}

impl VirtioFsClient {
    /// Probe and initialize the virtio-fs device. Returns None if not present.
    pub fn init<DM: DirectMap>(kalloc: &KernelAllocator<DM>) -> Option<Self> {
        let dm = kalloc.direct_map();
        let mmio_base = sumi_abi::arch::layout::VIRTIO_FS_MMIO.to_virtual(dm);

        // SAFETY: All MMIO accesses are to the virtio-fs device register region
        // at a known physical address mapped through the direct map.
        unsafe {
            // Verify device identity
            if mmio::read32(mmio_base, VIRTIO_MMIO_MAGIC) != VIRTIO_MMIO_MAGIC_VALUE {
                return None;
            }
            if mmio::read32(mmio_base, VIRTIO_MMIO_VERSION) != 2 {
                return None;
            }
            if mmio::read32(mmio_base, VIRTIO_MMIO_DEVICE_ID) != VIRTIO_DEVICE_FS {
                return None;
            }

            // Reset
            mmio::write32(mmio_base, VIRTIO_MMIO_STATUS, 0);

            // Acknowledge + Driver
            mmio::write32(mmio_base, VIRTIO_MMIO_STATUS, VIRTIO_STATUS_ACKNOWLEDGE);
            mmio::write32(
                mmio_base,
                VIRTIO_MMIO_STATUS,
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER,
            );

            // Negotiate features (accept all)
            mmio::write32(mmio_base, VIRTIO_MMIO_DEVICE_FEATURES_SEL, 0);
            let lo = mmio::read32(mmio_base, VIRTIO_MMIO_DEVICE_FEATURES);
            mmio::write32(mmio_base, VIRTIO_MMIO_DRIVER_FEATURES_SEL, 0);
            mmio::write32(mmio_base, VIRTIO_MMIO_DRIVER_FEATURES, lo);

            mmio::write32(mmio_base, VIRTIO_MMIO_DEVICE_FEATURES_SEL, 1);
            let hi = mmio::read32(mmio_base, VIRTIO_MMIO_DEVICE_FEATURES);
            mmio::write32(mmio_base, VIRTIO_MMIO_DRIVER_FEATURES_SEL, 1);
            mmio::write32(mmio_base, VIRTIO_MMIO_DRIVER_FEATURES, hi);

            // Features OK
            mmio::write32(
                mmio_base,
                VIRTIO_MMIO_STATUS,
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK,
            );
            if mmio::read32(mmio_base, VIRTIO_MMIO_STATUS) & VIRTIO_STATUS_FEATURES_OK == 0 {
                return None;
            }
        }

        // Set up virtqueue 1 (request queue)
        let queue = Virtqueue::new(kalloc).ok()?;

        // SAFETY: Continued MMIO register setup for queue addresses.
        unsafe {
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_SEL, 1);
            if mmio::read32(mmio_base, VIRTIO_MMIO_QUEUE_NUM_MAX) < QUEUE_SIZE as u32 {
                return None;
            }
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_NUM, QUEUE_SIZE as u32);

            let dp = queue.desc_phys().as_u64();
            let ap = queue.avail_phys().as_u64();
            let up = queue.used_phys().as_u64();
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_DESC_LOW, dp as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_DESC_HIGH, (dp >> 32) as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_AVAIL_LOW, ap as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_AVAIL_HIGH, (ap >> 32) as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_USED_LOW, up as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_USED_HIGH, (up >> 32) as u32);
            mmio::write32(mmio_base, VIRTIO_MMIO_QUEUE_READY, 1);

            // Driver OK
            mmio::write32(
                mmio_base,
                VIRTIO_MMIO_STATUS,
                VIRTIO_STATUS_ACKNOWLEDGE
                    | VIRTIO_STATUS_DRIVER
                    | VIRTIO_STATUS_FEATURES_OK
                    | VIRTIO_STATUS_DRIVER_OK,
            );
        }

        let mut client = Self {
            queue: spin::Mutex::new(queue),
            mmio_base,
            max_write: 0,
            next_unique: AtomicU64::new(1),
        };

        if !client.fuse_init() {
            return None;
        }

        Some(client)
    }

    fn next_unique(&self) -> u64 {
        self.next_unique.fetch_add(1, Ordering::Relaxed)
    }

    fn kick(&self) {
        // SAFETY: Writing QueueNotify triggers a synchronous VM exit.
        unsafe { mmio::write32(self.mmio_base, VIRTIO_MMIO_QUEUE_NOTIFY, 1) }
    }

    /// Convert a virtual address to physical.
    /// Handles kernel code/data addresses, direct-map addresses, and user-space
    /// addresses (the latter via page-table walk).
    pub fn v2p(virt: *const u8) -> PhysicalAddr {
        use sumi_abi::layout::{KERNEL_CODE_PHYS, KERNEL_CODE_VIRT};

        let addr = virt as usize;
        if addr >= KERNEL_CODE_VIRT.as_usize() {
            // Kernel code/data/BSS region: mapped at fixed offset
            PhysicalAddr::new(addr - KERNEL_CODE_VIRT.as_usize() + KERNEL_CODE_PHYS.as_usize())
        } else if addr >= DIRECT_MAP_OFFSET.as_usize() {
            // Direct map region
            PhysicalAddr::new(addr - DIRECT_MAP_OFFSET.as_usize())
        } else {
            // User-space address — walk the page table.
            use sumi_abi::arch::layout::PAGE_SIZE;
            let va = VirtualAddr::new(addr);
            let entry = crate::KERNEL_PAGE_TABLE
                .lock()
                .get_if_present(va)
                .expect("v2p: page table walk failed")
                .expect("v2p: address not mapped");
            let page_offset = addr & (PAGE_SIZE - 1);
            entry.addr().add(page_offset)
        }
    }

    /// Submit a descriptor chain, kick, wait for completion, free descriptors.
    /// Returns total bytes written by the device, or -errno.
    fn submit_chain(&self, specs: &[DescSpec]) -> Result<u32, i32> {
        let n = specs.len();
        debug_assert!(n > 0 && n <= 4);

        let mut queue = self.queue.lock();

        // Allocate descriptors, rolling back on failure
        let mut indices = [0u16; 4];
        for i in 0..n {
            match queue.alloc_desc() {
                Some(d) => indices[i] = d,
                None => {
                    for &idx in &indices[..i] {
                        queue.free_desc(idx);
                    }
                    return Err(-12); // ENOMEM
                }
            }
        }

        // Build chain
        for i in 0..n {
            let desc = queue.desc_mut(indices[i]);
            desc.addr = specs[i].addr.as_u64();
            desc.len = specs[i].len;
            desc.flags = if specs[i].writable {
                VIRTQ_DESC_F_WRITE
            } else {
                0
            };
            if i + 1 < n {
                desc.flags |= VIRTQ_DESC_F_NEXT;
                desc.next = indices[i + 1];
            }
        }

        queue.submit(indices[0]);

        // Kick — synchronous VM exit, blocks until response is ready
        self.kick();

        let elem = match queue.complete() {
            Some(e) => e,
            None => {
                queue.free_chain(indices[0]);
                return Err(-5); // EIO
            }
        };

        queue.free_chain(indices[0]);
        Ok(elem.len)
    }

    /// Write a FuseInHeader at the start of `buf`, using `buf.len()` as the FUSE message length.
    fn write_fuse_header(&self, buf: &mut [u8], opcode: u32, nodeid: u64) {
        self.write_fuse_header_with_len(buf, buf.len() as u32, opcode, nodeid);
    }

    /// Write a FuseInHeader at the start of `buf` with an explicit message length.
    fn write_fuse_header_with_len(&self, buf: &mut [u8], len: u32, opcode: u32, nodeid: u64) {
        // SAFETY: Writing a repr(C) struct into a properly sized buffer.
        unsafe {
            core::ptr::write_volatile(
                buf.as_mut_ptr() as *mut FuseInHeader,
                FuseInHeader {
                    len,
                    opcode,
                    unique: self.next_unique(),
                    nodeid,
                    uid: 0,
                    gid: 0,
                    pid: 0,
                    padding: 0,
                },
            );
        }
    }

    /// Submit a 2-descriptor chain (req readable, resp writable), check the FUSE error.
    fn submit_request_response(&self, req: &[u8], req_len: u32, resp: &[u8]) -> Result<(), i32> {
        let specs = [
            DescSpec {
                addr: Self::v2p(req.as_ptr()),
                len: req_len,
                writable: false,
            },
            DescSpec {
                addr: Self::v2p(resp.as_ptr()),
                len: resp.len() as u32,
                writable: true,
            },
        ];
        self.submit_chain(&specs)?;
        // SAFETY: VM wrote FuseOutHeader into resp buffer.
        unsafe {
            let hdr = core::ptr::read_volatile(resp.as_ptr() as *const FuseOutHeader);
            if hdr.error != 0 {
                return Err(hdr.error);
            }
        }
        Ok(())
    }

    /// Submit request/response chain and read a typed payload after the FuseOutHeader.
    fn submit_and_read<T: Copy>(&self, req: &[u8], req_len: u32, resp: &[u8]) -> Result<T, i32> {
        self.submit_request_response(req, req_len, resp)?;
        // SAFETY: VM wrote T immediately after FuseOutHeader in resp.
        unsafe {
            Ok(core::ptr::read_volatile(
                resp.as_ptr().add(size_of::<FuseOutHeader>()) as *const T,
            ))
        }
    }

    /// FUSE_INIT handshake.
    fn fuse_init(&mut self) -> bool {
        let mut req = [0u8; size_of::<FuseInHeader>() + size_of::<FuseInitIn>()];
        self.write_fuse_header(&mut req, FUSE_INIT, 0);
        // SAFETY: Writing repr(C) struct after the header.
        unsafe {
            core::ptr::write_volatile(
                req.as_mut_ptr().add(size_of::<FuseInHeader>()) as *mut FuseInitIn,
                FuseInitIn {
                    major: FUSE_KERNEL_VERSION,
                    minor: FUSE_KERNEL_MINOR_VERSION,
                    max_readahead: 0,
                    flags: 0,
                },
            );
        }
        let resp = [0u8; size_of::<FuseOutHeader>() + size_of::<FuseInitOut>()];
        match self.submit_and_read::<FuseInitOut>(&req, req.len() as u32, &resp) {
            Ok(init_out) => {
                self.max_write = init_out.max_write;
                true
            }
            Err(_) => false,
        }
    }

    /// FUSE_LOOKUP: resolve a single path component.
    pub fn lookup(&self, parent_nodeid: u64, name: &[u8]) -> Result<FuseEntryOut, i32> {
        let req_len = size_of::<FuseInHeader>() + name.len() + 1;
        let mut req = [0u8; size_of::<FuseInHeader>() + 256];
        debug_assert!(req_len <= req.len());

        self.write_fuse_header_with_len(&mut req, req_len as u32, FUSE_LOOKUP, parent_nodeid);
        // SAFETY: Copying name bytes after header; buffer is zeroed so null terminator is implicit.
        unsafe {
            core::ptr::copy_nonoverlapping(
                name.as_ptr(),
                req.as_mut_ptr().add(size_of::<FuseInHeader>()),
                name.len(),
            );
        }

        let resp = [0u8; size_of::<FuseOutHeader>() + size_of::<FuseEntryOut>()];
        self.submit_and_read(&req, req_len as u32, &resp)
    }

    /// FUSE_GETATTR: get file attributes by nodeid.
    pub fn getattr(&self, nodeid: u64) -> Result<FuseAttrOut, i32> {
        let mut req = [0u8; size_of::<FuseInHeader>() + size_of::<FuseGetattrIn>()];
        self.write_fuse_header(&mut req, FUSE_GETATTR, nodeid);
        // SAFETY: Writing repr(C) struct after the header.
        unsafe {
            core::ptr::write_volatile(
                req.as_mut_ptr().add(size_of::<FuseInHeader>()) as *mut FuseGetattrIn,
                FuseGetattrIn {
                    getattr_flags: 0,
                    dummy: 0,
                    fh: 0,
                },
            );
        }
        let resp = [0u8; size_of::<FuseOutHeader>() + size_of::<FuseAttrOut>()];
        self.submit_and_read(&req, req.len() as u32, &resp)
    }

    /// FUSE_OPEN: open a file by nodeid.
    pub fn open(&self, nodeid: u64, flags: u32) -> Result<FuseOpenOut, i32> {
        let mut req = [0u8; size_of::<FuseInHeader>() + size_of::<FuseOpenIn>()];
        self.write_fuse_header(&mut req, FUSE_OPEN, nodeid);
        unsafe {
            core::ptr::write_volatile(
                req.as_mut_ptr().add(size_of::<FuseInHeader>()) as *mut FuseOpenIn,
                FuseOpenIn {
                    flags,
                    open_flags: 0,
                },
            );
        }
        let resp = [0u8; size_of::<FuseOutHeader>() + size_of::<FuseOpenOut>()];
        self.submit_and_read(&req, req.len() as u32, &resp)
    }

    /// FUSE_CREATE: create and open a file in one step.
    /// Returns (FuseEntryOut, FuseOpenOut) on success.
    pub fn create(
        &self,
        parent_nodeid: u64,
        name: &[u8],
        flags: u32,
        mode: u32,
    ) -> Result<(FuseEntryOut, FuseOpenOut), i32> {
        let create_in_size = size_of::<FuseCreateIn>();
        let req_len = size_of::<FuseInHeader>() + create_in_size + name.len() + 1;
        let mut req = [0u8; size_of::<FuseInHeader>() + size_of::<FuseCreateIn>() + 256];
        debug_assert!(req_len <= req.len());

        self.write_fuse_header_with_len(&mut req, req_len as u32, FUSE_CREATE, parent_nodeid);
        unsafe {
            core::ptr::write_volatile(
                req.as_mut_ptr().add(size_of::<FuseInHeader>()) as *mut FuseCreateIn,
                FuseCreateIn {
                    flags,
                    mode,
                    umask: 0o022,
                    open_flags: 0,
                },
            );
            core::ptr::copy_nonoverlapping(
                name.as_ptr(),
                req.as_mut_ptr()
                    .add(size_of::<FuseInHeader>() + create_in_size),
                name.len(),
            );
        }

        let resp = [0u8; size_of::<FuseOutHeader>()
            + size_of::<FuseEntryOut>()
            + size_of::<FuseOpenOut>()];
        self.submit_request_response(&req, req_len as u32, &resp)?;
        unsafe {
            let entry = core::ptr::read_volatile(
                resp.as_ptr().add(size_of::<FuseOutHeader>()) as *const FuseEntryOut
            );
            let open = core::ptr::read_volatile(
                resp.as_ptr()
                    .add(size_of::<FuseOutHeader>() + size_of::<FuseEntryOut>())
                    as *const FuseOpenOut,
            );
            Ok((entry, open))
        }
    }

    /// FUSE_READ: read data into buf_phys. Returns bytes read.
    pub fn read(
        &self,
        fh: u64,
        offset: u64,
        buf_phys: PhysicalAddr,
        count: u32,
    ) -> Result<u32, i32> {
        let mut req = [0u8; size_of::<FuseInHeader>() + size_of::<FuseReadIn>()];
        self.write_fuse_header(&mut req, FUSE_READ, 0);
        unsafe {
            core::ptr::write_volatile(
                req.as_mut_ptr().add(size_of::<FuseInHeader>()) as *mut FuseReadIn,
                FuseReadIn {
                    fh,
                    offset,
                    size: count,
                    read_flags: 0,
                    lock_owner: 0,
                    flags: 0,
                    padding: 0,
                },
            );
        }

        let resp_hdr = FuseOutHeader {
            len: 0,
            error: 0,
            unique: 0,
        };
        let specs = [
            DescSpec {
                addr: Self::v2p(req.as_ptr()),
                len: req.len() as u32,
                writable: false,
            },
            DescSpec {
                addr: Self::v2p(&resp_hdr as *const _ as *const u8),
                len: size_of::<FuseOutHeader>() as u32,
                writable: true,
            },
            DescSpec {
                addr: buf_phys,
                len: count,
                writable: true,
            },
        ];
        self.submit_chain(&specs)?;

        // SAFETY: VM wrote FuseOutHeader into resp_hdr.
        unsafe {
            let hdr = core::ptr::read_volatile(&resp_hdr);
            if hdr.error != 0 {
                return Err(hdr.error);
            }
            Ok(hdr.len.saturating_sub(size_of::<FuseOutHeader>() as u32))
        }
    }

    /// FUSE_WRITE: write data from buf_phys. Returns bytes written.
    pub fn write(
        &self,
        fh: u64,
        offset: u64,
        buf_phys: PhysicalAddr,
        count: u32,
    ) -> Result<u32, i32> {
        let mut req = [0u8; size_of::<FuseInHeader>() + size_of::<FuseWriteIn>()];
        let write_len = (req.len() as u32) + count;
        self.write_fuse_header_with_len(&mut req, write_len, FUSE_WRITE, 0);
        unsafe {
            core::ptr::write_volatile(
                req.as_mut_ptr().add(size_of::<FuseInHeader>()) as *mut FuseWriteIn,
                FuseWriteIn {
                    fh,
                    offset,
                    size: count,
                    write_flags: 0,
                    lock_owner: 0,
                    flags: 0,
                    padding: 0,
                },
            );
        }

        let resp = [0u8; size_of::<FuseOutHeader>() + size_of::<FuseWriteOut>()];
        let specs = [
            DescSpec {
                addr: Self::v2p(req.as_ptr()),
                len: req.len() as u32,
                writable: false,
            },
            DescSpec {
                addr: buf_phys,
                len: count,
                writable: false,
            },
            DescSpec {
                addr: Self::v2p(resp.as_ptr()),
                len: resp.len() as u32,
                writable: true,
            },
        ];
        self.submit_chain(&specs)?;

        let write_out: FuseWriteOut = unsafe {
            let out_header = core::ptr::read_volatile(resp.as_ptr() as *const FuseOutHeader);
            if out_header.error != 0 {
                return Err(out_header.error);
            }
            core::ptr::read_volatile(
                resp.as_ptr().add(size_of::<FuseOutHeader>()) as *const FuseWriteOut
            )
        };
        Ok(write_out.size)
    }

    /// FUSE_RELEASE: close a file handle.
    pub fn release(&self, fh: u64) {
        self.release_common(fh, FUSE_RELEASE);
    }

    /// FUSE_FORGET: drop a lookup reference. No response expected.
    pub fn forget(&self, nodeid: u64, nlookup: u64) {
        let mut req = [0u8; size_of::<FuseInHeader>() + size_of::<FuseForgetIn>()];
        self.write_fuse_header(&mut req, FUSE_FORGET, nodeid);
        unsafe {
            core::ptr::write_volatile(
                req.as_mut_ptr().add(size_of::<FuseInHeader>()) as *mut FuseForgetIn,
                FuseForgetIn { nlookup },
            );
        }
        // FORGET has no response, but virtio requires a used ring entry
        let specs = [DescSpec {
            addr: Self::v2p(req.as_ptr()),
            len: req.len() as u32,
            writable: false,
        }];
        let _ = self.submit_chain(&specs);
    }

    /// FUSE_OPENDIR: open a directory by nodeid.
    pub fn opendir(&self, nodeid: u64) -> Result<FuseOpenOut, i32> {
        let mut req = [0u8; size_of::<FuseInHeader>() + size_of::<FuseOpenIn>()];
        self.write_fuse_header(&mut req, FUSE_OPENDIR, nodeid);
        unsafe {
            core::ptr::write_volatile(
                req.as_mut_ptr().add(size_of::<FuseInHeader>()) as *mut FuseOpenIn,
                FuseOpenIn {
                    flags: 0,
                    open_flags: 0,
                },
            );
        }
        let resp = [0u8; size_of::<FuseOutHeader>() + size_of::<FuseOpenOut>()];
        self.submit_and_read(&req, req.len() as u32, &resp)
    }

    /// FUSE_RELEASEDIR: close a directory handle.
    pub fn releasedir(&self, fh: u64) {
        self.release_common(fh, FUSE_RELEASEDIR);
    }

    /// Shared implementation for FUSE_RELEASE and FUSE_RELEASEDIR.
    fn release_common(&self, fh: u64, opcode: u32) {
        let mut req = [0u8; size_of::<FuseInHeader>() + size_of::<FuseReleaseIn>()];
        self.write_fuse_header(&mut req, opcode, 0);
        unsafe {
            core::ptr::write_volatile(
                req.as_mut_ptr().add(size_of::<FuseInHeader>()) as *mut FuseReleaseIn,
                FuseReleaseIn {
                    fh,
                    flags: 0,
                    release_flags: 0,
                    lock_owner: 0,
                },
            );
        }
        let resp = [0u8; size_of::<FuseOutHeader>()];
        let _ = self.submit_request_response(&req, req.len() as u32, &resp);
    }

    /// FUSE_READDIR: read directory entries into buf_phys. Returns bytes read.
    pub fn readdir(
        &self,
        nodeid: u64,
        fh: u64,
        offset: u64,
        buf_phys: PhysicalAddr,
        count: u32,
    ) -> Result<u32, i32> {
        let mut req = [0u8; size_of::<FuseInHeader>() + size_of::<FuseReadIn>()];
        self.write_fuse_header(&mut req, FUSE_READDIR, nodeid);
        unsafe {
            core::ptr::write_volatile(
                req.as_mut_ptr().add(size_of::<FuseInHeader>()) as *mut FuseReadIn,
                FuseReadIn {
                    fh,
                    offset,
                    size: count,
                    read_flags: 0,
                    lock_owner: 0,
                    flags: 0,
                    padding: 0,
                },
            );
        }

        let resp_hdr = FuseOutHeader {
            len: 0,
            error: 0,
            unique: 0,
        };
        let specs = [
            DescSpec {
                addr: Self::v2p(req.as_ptr()),
                len: req.len() as u32,
                writable: false,
            },
            DescSpec {
                addr: Self::v2p(&resp_hdr as *const _ as *const u8),
                len: size_of::<FuseOutHeader>() as u32,
                writable: true,
            },
            DescSpec {
                addr: buf_phys,
                len: count,
                writable: true,
            },
        ];
        self.submit_chain(&specs)?;

        // SAFETY: VM wrote FuseOutHeader into resp_hdr.
        unsafe {
            let hdr = core::ptr::read_volatile(&resp_hdr);
            if hdr.error != 0 {
                return Err(hdr.error);
            }
            Ok(hdr.len.saturating_sub(size_of::<FuseOutHeader>() as u32))
        }
    }

    /// FUSE_SETUPMAPPING: map a file region into the DAX window.
    pub fn setup_mapping(
        &self,
        fh: u64,
        file_offset: u64,
        len: u64,
        dax_offset: usize,
        flags: u64,
    ) -> Result<(), i32> {
        let mut req = [0u8; size_of::<FuseInHeader>() + size_of::<FuseSetupMappingIn>()];
        self.write_fuse_header(&mut req, FUSE_SETUPMAPPING, 0);
        unsafe {
            core::ptr::write_volatile(
                req.as_mut_ptr().add(size_of::<FuseInHeader>()) as *mut FuseSetupMappingIn,
                FuseSetupMappingIn {
                    fh,
                    foffset: file_offset,
                    len,
                    flags,
                    moffset: dax_offset as u64,
                },
            );
        }
        let resp = [0u8; size_of::<FuseOutHeader>()];
        self.submit_request_response(&req, req.len() as u32, &resp)
    }

    /// FUSE_REMOVEMAPPING: unmap a region from the DAX window.
    pub fn remove_mapping(&self, dax_offset: usize, len: u64) -> Result<(), i32> {
        let mut req = [0u8; size_of::<FuseInHeader>()
            + size_of::<FuseRemoveMappingIn>()
            + size_of::<FuseRemoveMappingOne>()];
        self.write_fuse_header(&mut req, FUSE_REMOVEMAPPING, 0);
        unsafe {
            core::ptr::write_volatile(
                req.as_mut_ptr().add(size_of::<FuseInHeader>()) as *mut FuseRemoveMappingIn,
                FuseRemoveMappingIn { count: 1 },
            );
            // SAFETY: offset 44 is not 8-byte aligned, so we must use write_unaligned.
            core::ptr::write_unaligned(
                req.as_mut_ptr()
                    .add(size_of::<FuseInHeader>() + size_of::<FuseRemoveMappingIn>())
                    as *mut FuseRemoveMappingOne,
                FuseRemoveMappingOne {
                    moffset: dax_offset as u64,
                    len,
                },
            );
        }
        let resp = [0u8; size_of::<FuseOutHeader>()];
        self.submit_request_response(&req, req.len() as u32, &resp)
    }

    /// Resolve a path to a FUSE nodeid by walking each component via FUSE_LOOKUP.
    /// Intermediate nodeids are forgotten so only the final nodeid is retained.
    pub fn resolve_path(&self, path: &[u8]) -> Result<u64, i32> {
        let mut nodeid = FUSE_ROOT_ID;

        for component in path.split(|&b| b == b'/') {
            if component.is_empty() {
                continue;
            }
            let prev = nodeid;
            let entry = match self.lookup(nodeid, component) {
                Ok(e) => e,
                Err(e) => {
                    // Forget the last intermediate (unless it's root)
                    if prev != FUSE_ROOT_ID {
                        self.forget(prev, 1);
                    }
                    return Err(e);
                }
            };
            // Forget the previous intermediate (not the root, which is permanent)
            if prev != FUSE_ROOT_ID {
                self.forget(prev, 1);
            }
            nodeid = entry.nodeid;
        }

        Ok(nodeid)
    }
}
