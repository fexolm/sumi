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

    /// Convert a kernel virtual address to physical via direct-map arithmetic.
    pub fn v2p(virt: *const u8) -> PhysicalAddr {
        debug_assert!((virt as usize) >= DIRECT_MAP_OFFSET.as_usize());
        PhysicalAddr::new((virt as usize) - DIRECT_MAP_OFFSET.as_usize())
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
                    for j in 0..i {
                        queue.free_desc(indices[j]);
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

    /// FUSE_INIT handshake.
    fn fuse_init(&mut self) -> bool {
        let mut req = [0u8; size_of::<FuseInHeader>() + size_of::<FuseInitIn>()];

        // SAFETY: Writing repr(C) structs into a properly sized buffer.
        unsafe {
            core::ptr::write_volatile(
                req.as_mut_ptr() as *mut FuseInHeader,
                FuseInHeader {
                    len: req.len() as u32,
                    opcode: FUSE_INIT,
                    unique: self.next_unique(),
                    nodeid: 0,
                    uid: 0,
                    gid: 0,
                    pid: 0,
                    padding: 0,
                },
            );
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

        let specs = [
            DescSpec {
                addr: Self::v2p(req.as_ptr()),
                len: req.len() as u32,
                writable: false,
            },
            DescSpec {
                addr: Self::v2p(resp.as_ptr()),
                len: resp.len() as u32,
                writable: true,
            },
        ];

        if self.submit_chain(&specs).is_err() {
            return false;
        }

        // SAFETY: VM wrote FuseOutHeader + FuseInitOut into resp.
        unsafe {
            let out_header = core::ptr::read_volatile(resp.as_ptr() as *const FuseOutHeader);
            if out_header.error != 0 {
                return false;
            }
            let init_out = core::ptr::read_volatile(
                resp.as_ptr().add(size_of::<FuseOutHeader>()) as *const FuseInitOut,
            );
            self.max_write = init_out.max_write;
        }

        true
    }

    /// FUSE_LOOKUP: resolve a single path component.
    pub fn lookup(&self, parent_nodeid: u64, name: &[u8]) -> Result<FuseEntryOut, i32> {
        let header_size = size_of::<FuseInHeader>();
        let req_len = header_size + name.len() + 1; // +1 for null terminator
        // Stack buffer: header + name + null (max path component ~255 bytes)
        let mut req = [0u8; size_of::<FuseInHeader>() + 256];
        debug_assert!(req_len <= req.len());

        unsafe {
            core::ptr::write_volatile(
                req.as_mut_ptr() as *mut FuseInHeader,
                FuseInHeader {
                    len: req_len as u32,
                    opcode: FUSE_LOOKUP,
                    unique: self.next_unique(),
                    nodeid: parent_nodeid,
                    uid: 0,
                    gid: 0,
                    pid: 0,
                    padding: 0,
                },
            );
            // Copy name after header
            core::ptr::copy_nonoverlapping(
                name.as_ptr(),
                req.as_mut_ptr().add(header_size),
                name.len(),
            );
            // Null terminator (buffer is zeroed, so already 0)
        }

        let resp = [0u8; size_of::<FuseOutHeader>() + size_of::<FuseEntryOut>()];

        let specs = [
            DescSpec {
                addr: Self::v2p(req.as_ptr()),
                len: req_len as u32,
                writable: false,
            },
            DescSpec {
                addr: Self::v2p(resp.as_ptr()),
                len: resp.len() as u32,
                writable: true,
            },
        ];

        self.submit_chain(&specs)?;

        unsafe {
            let out_header = core::ptr::read_volatile(resp.as_ptr() as *const FuseOutHeader);
            if out_header.error != 0 {
                return Err(out_header.error);
            }
            Ok(core::ptr::read_volatile(
                resp.as_ptr().add(size_of::<FuseOutHeader>()) as *const FuseEntryOut,
            ))
        }
    }

    /// FUSE_GETATTR: get file attributes by nodeid.
    pub fn getattr(&self, nodeid: u64) -> Result<FuseAttrOut, i32> {
        let mut req = [0u8; size_of::<FuseInHeader>() + size_of::<FuseGetattrIn>()];

        unsafe {
            core::ptr::write_volatile(
                req.as_mut_ptr() as *mut FuseInHeader,
                FuseInHeader {
                    len: req.len() as u32,
                    opcode: FUSE_GETATTR,
                    unique: self.next_unique(),
                    nodeid,
                    uid: 0,
                    gid: 0,
                    pid: 0,
                    padding: 0,
                },
            );
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

        let specs = [
            DescSpec {
                addr: Self::v2p(req.as_ptr()),
                len: req.len() as u32,
                writable: false,
            },
            DescSpec {
                addr: Self::v2p(resp.as_ptr()),
                len: resp.len() as u32,
                writable: true,
            },
        ];

        self.submit_chain(&specs)?;

        unsafe {
            let out_header = core::ptr::read_volatile(resp.as_ptr() as *const FuseOutHeader);
            if out_header.error != 0 {
                return Err(out_header.error);
            }
            Ok(core::ptr::read_volatile(
                resp.as_ptr().add(size_of::<FuseOutHeader>()) as *const FuseAttrOut,
            ))
        }
    }

    /// FUSE_OPEN: open a file by nodeid.
    pub fn open(&self, nodeid: u64, flags: u32) -> Result<FuseOpenOut, i32> {
        let mut req = [0u8; size_of::<FuseInHeader>() + size_of::<FuseOpenIn>()];

        unsafe {
            core::ptr::write_volatile(
                req.as_mut_ptr() as *mut FuseInHeader,
                FuseInHeader {
                    len: req.len() as u32,
                    opcode: FUSE_OPEN,
                    unique: self.next_unique(),
                    nodeid,
                    uid: 0,
                    gid: 0,
                    pid: 0,
                    padding: 0,
                },
            );
            core::ptr::write_volatile(
                req.as_mut_ptr().add(size_of::<FuseInHeader>()) as *mut FuseOpenIn,
                FuseOpenIn {
                    flags,
                    open_flags: 0,
                },
            );
        }

        let resp = [0u8; size_of::<FuseOutHeader>() + size_of::<FuseOpenOut>()];

        let specs = [
            DescSpec {
                addr: Self::v2p(req.as_ptr()),
                len: req.len() as u32,
                writable: false,
            },
            DescSpec {
                addr: Self::v2p(resp.as_ptr()),
                len: resp.len() as u32,
                writable: true,
            },
        ];

        self.submit_chain(&specs)?;

        unsafe {
            let out_header = core::ptr::read_volatile(resp.as_ptr() as *const FuseOutHeader);
            if out_header.error != 0 {
                return Err(out_header.error);
            }
            Ok(core::ptr::read_volatile(
                resp.as_ptr().add(size_of::<FuseOutHeader>()) as *const FuseOpenOut,
            ))
        }
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
        let header_size = size_of::<FuseInHeader>();
        let create_in_size = size_of::<FuseCreateIn>();
        let req_len = header_size + create_in_size + name.len() + 1;
        let mut req = [0u8; size_of::<FuseInHeader>() + size_of::<FuseCreateIn>() + 256];
        debug_assert!(req_len <= req.len());

        unsafe {
            core::ptr::write_volatile(
                req.as_mut_ptr() as *mut FuseInHeader,
                FuseInHeader {
                    len: req_len as u32,
                    opcode: FUSE_CREATE,
                    unique: self.next_unique(),
                    nodeid: parent_nodeid,
                    uid: 0,
                    gid: 0,
                    pid: 0,
                    padding: 0,
                },
            );
            core::ptr::write_volatile(
                req.as_mut_ptr().add(header_size) as *mut FuseCreateIn,
                FuseCreateIn {
                    flags,
                    mode,
                    umask: 0o022,
                    open_flags: 0,
                },
            );
            core::ptr::copy_nonoverlapping(
                name.as_ptr(),
                req.as_mut_ptr().add(header_size + create_in_size),
                name.len(),
            );
        }

        let resp = [0u8; size_of::<FuseOutHeader>() + size_of::<FuseEntryOut>()
            + size_of::<FuseOpenOut>()];

        let specs = [
            DescSpec {
                addr: Self::v2p(req.as_ptr()),
                len: req_len as u32,
                writable: false,
            },
            DescSpec {
                addr: Self::v2p(resp.as_ptr()),
                len: resp.len() as u32,
                writable: true,
            },
        ];

        self.submit_chain(&specs)?;

        unsafe {
            let out_header = core::ptr::read_volatile(resp.as_ptr() as *const FuseOutHeader);
            if out_header.error != 0 {
                return Err(out_header.error);
            }
            let entry = core::ptr::read_volatile(
                resp.as_ptr().add(size_of::<FuseOutHeader>()) as *const FuseEntryOut,
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

        unsafe {
            core::ptr::write_volatile(
                req.as_mut_ptr() as *mut FuseInHeader,
                FuseInHeader {
                    len: req.len() as u32,
                    opcode: FUSE_READ,
                    unique: self.next_unique(),
                    nodeid: 0,
                    uid: 0,
                    gid: 0,
                    pid: 0,
                    padding: 0,
                },
            );
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

        unsafe {
            let hdr = core::ptr::read_volatile(&resp_hdr);
            if hdr.error != 0 {
                return Err(hdr.error);
            }
            let bytes_read = hdr
                .len
                .saturating_sub(size_of::<FuseOutHeader>() as u32);
            Ok(bytes_read)
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

        unsafe {
            core::ptr::write_volatile(
                req.as_mut_ptr() as *mut FuseInHeader,
                FuseInHeader {
                    len: (req.len() as u32) + count,
                    opcode: FUSE_WRITE,
                    unique: self.next_unique(),
                    nodeid: 0,
                    uid: 0,
                    gid: 0,
                    pid: 0,
                    padding: 0,
                },
            );
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

        unsafe {
            let out_header = core::ptr::read_volatile(resp.as_ptr() as *const FuseOutHeader);
            if out_header.error != 0 {
                return Err(out_header.error);
            }
            let write_out = core::ptr::read_volatile(
                resp.as_ptr().add(size_of::<FuseOutHeader>()) as *const FuseWriteOut,
            );
            Ok(write_out.size)
        }
    }

    /// FUSE_RELEASE: close a file handle.
    pub fn release(&self, fh: u64) {
        let mut req = [0u8; size_of::<FuseInHeader>() + size_of::<FuseReleaseIn>()];

        unsafe {
            core::ptr::write_volatile(
                req.as_mut_ptr() as *mut FuseInHeader,
                FuseInHeader {
                    len: req.len() as u32,
                    opcode: FUSE_RELEASE,
                    unique: self.next_unique(),
                    nodeid: 0,
                    uid: 0,
                    gid: 0,
                    pid: 0,
                    padding: 0,
                },
            );
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

        let specs = [
            DescSpec {
                addr: Self::v2p(req.as_ptr()),
                len: req.len() as u32,
                writable: false,
            },
            DescSpec {
                addr: Self::v2p(resp.as_ptr()),
                len: resp.len() as u32,
                writable: true,
            },
        ];

        let _ = self.submit_chain(&specs);
    }

    /// FUSE_FORGET: drop a lookup reference. No response expected.
    pub fn forget(&self, nodeid: u64, nlookup: u64) {
        let mut req = [0u8; size_of::<FuseInHeader>() + size_of::<FuseForgetIn>()];

        unsafe {
            core::ptr::write_volatile(
                req.as_mut_ptr() as *mut FuseInHeader,
                FuseInHeader {
                    len: req.len() as u32,
                    opcode: FUSE_FORGET,
                    unique: self.next_unique(),
                    nodeid,
                    uid: 0,
                    gid: 0,
                    pid: 0,
                    padding: 0,
                },
            );
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

    /// Resolve a path to a FUSE nodeid by walking each component via FUSE_LOOKUP.
    pub fn resolve_path(&self, path: &[u8]) -> Result<u64, i32> {
        let mut nodeid = FUSE_ROOT_ID;

        for component in path.split(|&b| b == b'/') {
            if component.is_empty() {
                continue;
            }
            let entry = self.lookup(nodeid, component)?;
            nodeid = entry.nodeid;
        }

        Ok(nodeid)
    }
}
