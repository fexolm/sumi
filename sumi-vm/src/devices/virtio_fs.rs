use std::fs::{File, OpenOptions};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;

use libc;

use sumi_abi::arch::layout::DAX_WINDOW_SIZE;
use sumi_abi::fuse::*;
use sumi_abi::virtio::*;
use vm_memory::bitmap::BitmapSlice;
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap, VolatileSlice};

use super::virtio_mmio::{
    VirtioBackend, VirtqueueState, post_used, read_avail_head, read_avail_idx, read_desc,
};

const INLINE_FUSE_REQ: usize = 512;

struct FuseNode {
    host_path: PathBuf,
    lookup_count: u64,
}

pub struct VirtioFs {
    _share_root: PathBuf,
    nodes: Vec<Option<FuseNode>>,
    file_handles: Vec<Option<File>>,
    last_avail_idx: u16,
    /// Host pointer to the 128 GB DAX window. Null if DAX is not available.
    dax_host_ptr: Option<*mut u8>,
}

// SAFETY: VirtioFs is only accessed from a single thread (inside Mutex<DeviceRegistry>).
unsafe impl Send for VirtioFs {}

fn errno_from_io_error(err: std::io::Error) -> i32 {
    err.raw_os_error().unwrap_or(5)
}

fn pread_guest<B: BitmapSlice>(
    fd: RawFd,
    buf: &mut VolatileSlice<'_, B>,
    offset: u64,
) -> Result<usize, i32> {
    let guard = buf.ptr_guard_mut();
    let ptr = guard.as_ptr().cast::<libc::c_void>();
    // SAFETY: `fd` is a live host file descriptor and `ptr..+buf.len()` is
    // the validated guest memory range represented by `VolatileSlice`.
    let n = unsafe { libc::pread(fd, ptr, buf.len(), offset as libc::off_t) };
    if n < 0 {
        buf.bitmap().mark_dirty(0, buf.len());
        Err(errno_from_io_error(std::io::Error::last_os_error()))
    } else {
        let n = n as usize;
        buf.bitmap().mark_dirty(0, n);
        Ok(n)
    }
}

fn pwrite_guest<B: BitmapSlice>(
    fd: RawFd,
    buf: &VolatileSlice<'_, B>,
    offset: u64,
) -> Result<usize, i32> {
    let guard = buf.ptr_guard();
    let ptr = guard.as_ptr().cast::<libc::c_void>();
    // SAFETY: `fd` is a live host file descriptor and `ptr..+buf.len()` is
    // the validated guest memory range represented by `VolatileSlice`.
    let n = unsafe { libc::pwrite(fd, ptr, buf.len(), offset as libc::off_t) };
    if n < 0 {
        Err(errno_from_io_error(std::io::Error::last_os_error()))
    } else {
        Ok(n as usize)
    }
}

impl VirtioFs {
    pub fn new(share_dir: &std::path::Path, dax_host_ptr: *mut u8) -> Self {
        let nodes = vec![
            // nodeid 0 is unused (FUSE convention)
            None,
            // nodeid 1 = root
            Some(FuseNode {
                host_path: share_dir.to_path_buf(),
                lookup_count: 1,
            }),
        ];

        let dax_host_ptr = if dax_host_ptr.is_null() {
            None
        } else {
            Some(dax_host_ptr)
        };

        Self {
            _share_root: share_dir.to_path_buf(),
            nodes,
            file_handles: Vec::new(),
            last_avail_idx: 0,
            dax_host_ptr,
        }
    }

    fn alloc_nodeid(&mut self, path: PathBuf) -> u64 {
        for (nodeid, slot) in self.nodes.iter_mut().enumerate().skip(1) {
            if let Some(node) = slot
                && node.host_path == path
            {
                node.lookup_count = node.lookup_count.saturating_add(1);
                return nodeid as u64;
            }
        }

        let nodeid = self.nodes.len() as u64;
        self.nodes.push(Some(FuseNode {
            host_path: path,
            lookup_count: 1,
        }));
        nodeid
    }

    fn alloc_fh(&mut self, file: File) -> u64 {
        for (i, slot) in self.file_handles.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(file);
                return i as u64;
            }
        }
        self.file_handles.push(Some(file));
        (self.file_handles.len() - 1) as u64
    }

    fn process_queue_inner(&mut self, queue: &VirtqueueState, mem: &GuestMemoryMmap<()>) {
        let avail_idx = read_avail_idx(queue, mem);

        while self.last_avail_idx != avail_idx {
            let head = read_avail_head(queue, self.last_avail_idx, mem);
            let total_written = self.process_descriptor_chain(queue, head, mem);
            post_used(queue, head, total_written, mem);
            self.last_avail_idx = self.last_avail_idx.wrapping_add(1);
        }
    }

    fn process_descriptor_chain(
        &mut self,
        queue: &VirtqueueState,
        head: u16,
        mem: &GuestMemoryMmap<()>,
    ) -> u32 {
        let mut readable_bufs = [(0u64, 0u32); 4];
        let mut writable_bufs = [(0u64, 0u32); 4];
        let mut readable_len = 0usize;
        let mut writable_len = 0usize;

        let mut idx = head;
        loop {
            let desc = read_desc(queue, idx, mem);
            if desc.flags & VIRTQ_DESC_F_WRITE != 0 {
                if writable_len < writable_bufs.len() {
                    writable_bufs[writable_len] = (desc.addr, desc.len);
                    writable_len += 1;
                }
            } else {
                if readable_len < readable_bufs.len() {
                    readable_bufs[readable_len] = (desc.addr, desc.len);
                    readable_len += 1;
                }
            }
            if desc.flags & VIRTQ_DESC_F_NEXT == 0 {
                break;
            }
            idx = desc.next;
        }

        let readable_bufs = &readable_bufs[..readable_len];
        let writable_bufs = &writable_bufs[..writable_len];

        if readable_bufs.is_empty() {
            return 0;
        }

        // Read FUSE request from first readable buffer
        let (req_addr, req_len) = readable_bufs[0];
        let req_len = req_len as usize;

        if req_len < core::mem::size_of::<FuseInHeader>() {
            return 0;
        }

        if req_len <= INLINE_FUSE_REQ {
            let mut req_data = [0u8; INLINE_FUSE_REQ];
            let req_data = &mut req_data[..req_len];
            mem.read_slice(req_data, GuestAddress(req_addr)).unwrap();

            // SAFETY: req_data is large enough for FuseInHeader, and the struct is repr(C).
            let header = unsafe { &*(req_data.as_ptr() as *const FuseInHeader) };
            return self.dispatch_fuse(header, req_data, readable_bufs, writable_bufs, mem);
        }

        let mut req_data = vec![0u8; req_len];
        mem.read_slice(&mut req_data, GuestAddress(req_addr))
            .unwrap();

        // SAFETY: req_data is large enough for FuseInHeader, and the struct is repr(C).
        let header = unsafe { &*(req_data.as_ptr() as *const FuseInHeader) };
        self.dispatch_fuse(header, &req_data, readable_bufs, writable_bufs, mem)
    }

    fn dispatch_fuse(
        &mut self,
        header: &FuseInHeader,
        req_data: &[u8],
        readable_bufs: &[(u64, u32)],
        writable_bufs: &[(u64, u32)],
        mem: &GuestMemoryMmap<()>,
    ) -> u32 {
        match header.opcode {
            FUSE_INIT => self.handle_init(header, writable_bufs, mem),
            FUSE_LOOKUP => self.handle_lookup(header, req_data, writable_bufs, mem),
            FUSE_GETATTR => self.handle_getattr(header, writable_bufs, mem),
            FUSE_SETATTR => self.handle_setattr(header, req_data, writable_bufs, mem),
            FUSE_MKDIR => self.handle_mkdir(header, req_data, writable_bufs, mem),
            FUSE_UNLINK => self.handle_unlink(header, req_data, writable_bufs, mem),
            FUSE_RMDIR => self.handle_rmdir(header, req_data, writable_bufs, mem),
            FUSE_RENAME => self.handle_rename(header, req_data, writable_bufs, mem),
            FUSE_CREATE => self.handle_create(header, req_data, writable_bufs, mem),
            FUSE_FSYNC => self.handle_fsync(header, req_data, writable_bufs, mem),
            FUSE_OPEN | FUSE_OPENDIR => self.handle_open(header, req_data, writable_bufs, mem),
            FUSE_READ => self.handle_read(header, req_data, writable_bufs, mem),
            FUSE_READDIR => self.handle_readdir(header, req_data, writable_bufs, mem),
            FUSE_WRITE => self.handle_write(header, req_data, readable_bufs, writable_bufs, mem),
            FUSE_RELEASE | FUSE_RELEASEDIR => {
                self.handle_release(header, req_data, writable_bufs, mem)
            }
            FUSE_FORGET => {
                self.handle_forget(header, req_data);
                0
            }
            FUSE_SETUPMAPPING => self.handle_setupmapping(header, req_data, writable_bufs, mem),
            FUSE_REMOVEMAPPING => self.handle_removemapping(header, req_data, writable_bufs, mem),
            _ => self.write_error(header.unique, -38, writable_bufs, mem),
        }
    }

    // ── Response helpers ─────────────────────────────────────────────

    fn write_response(
        &self,
        unique: u64,
        body: &[u8],
        writable_bufs: &[(u64, u32)],
        mem: &GuestMemoryMmap<()>,
    ) -> u32 {
        let hdr_size = core::mem::size_of::<FuseOutHeader>();
        let out_header = FuseOutHeader {
            len: (hdr_size + body.len()) as u32,
            error: 0,
            unique,
        };
        let hdr_bytes: &[u8] =
            unsafe { core::slice::from_raw_parts(&out_header as *const _ as *const u8, hdr_size) };

        if writable_bufs.is_empty() {
            return 0;
        }

        let (buf0_addr, buf0_len) = writable_bufs[0];

        // Write header into first writable buffer
        let hdr_write = hdr_size.min(buf0_len as usize);
        mem.write_slice(&hdr_bytes[..hdr_write], GuestAddress(buf0_addr))
            .unwrap();

        let mut written = hdr_size as u32;

        if !body.is_empty() {
            let remaining_in_first = (buf0_len as usize).saturating_sub(hdr_size);
            if remaining_in_first >= body.len() {
                mem.write_slice(body, GuestAddress(buf0_addr + hdr_size as u64))
                    .unwrap();
            } else {
                if remaining_in_first > 0 {
                    mem.write_slice(
                        &body[..remaining_in_first],
                        GuestAddress(buf0_addr + hdr_size as u64),
                    )
                    .unwrap();
                }
                if writable_bufs.len() > 1 {
                    let (buf1_addr, _) = writable_bufs[1];
                    mem.write_slice(&body[remaining_in_first..], GuestAddress(buf1_addr))
                        .unwrap();
                }
            }
            written += body.len() as u32;
        }

        written
    }

    fn write_error(
        &self,
        unique: u64,
        errno: i32,
        writable_bufs: &[(u64, u32)],
        mem: &GuestMemoryMmap<()>,
    ) -> u32 {
        let hdr_size = core::mem::size_of::<FuseOutHeader>();
        let out_header = FuseOutHeader {
            len: hdr_size as u32,
            error: errno,
            unique,
        };
        let hdr_bytes: &[u8] =
            unsafe { core::slice::from_raw_parts(&out_header as *const _ as *const u8, hdr_size) };

        if writable_bufs.is_empty() {
            return 0;
        }
        let (addr, _) = writable_bufs[0];
        mem.write_slice(hdr_bytes, GuestAddress(addr)).unwrap();
        hdr_size as u32
    }

    // ── FUSE handlers ────────────────────────────────────────────────

    fn handle_init(
        &self,
        header: &FuseInHeader,
        writable_bufs: &[(u64, u32)],
        mem: &GuestMemoryMmap<()>,
    ) -> u32 {
        let init_out = FuseInitOut {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION,
            max_readahead: 0,
            flags: 0,
            max_background: 0,
            congestion_threshold: 0,
            max_write: 1_048_576, // 1 MB
            time_gran: 1,
            max_pages: 0,
            // 21 = log2(2MB): tells the kernel that DAX mappings must be 2MB-aligned.
            map_alignment: 21,
            flags2: 0,
            unused: [0; 7],
        };
        let body: &[u8] = unsafe {
            core::slice::from_raw_parts(
                &init_out as *const _ as *const u8,
                core::mem::size_of::<FuseInitOut>(),
            )
        };
        self.write_response(header.unique, body, writable_bufs, mem)
    }

    fn handle_lookup(
        &mut self,
        header: &FuseInHeader,
        req_data: &[u8],
        writable_bufs: &[(u64, u32)],
        mem: &GuestMemoryMmap<()>,
    ) -> u32 {
        let hdr_size = core::mem::size_of::<FuseInHeader>();
        let name_bytes = &req_data[hdr_size..];
        let name_end = name_bytes
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(name_bytes.len());
        let name = match std::str::from_utf8(&name_bytes[..name_end]) {
            Ok(s) => s,
            Err(_) => return self.write_error(header.unique, -22, writable_bufs, mem),
        };

        if name == ".." || name == "." || name.contains('/') || name.contains('\0') {
            return self.write_error(header.unique, -22, writable_bufs, mem);
        }

        let parent_path = match self.nodes.get(header.nodeid as usize) {
            Some(Some(node)) => node.host_path.clone(),
            _ => return self.write_error(header.unique, -2, writable_bufs, mem),
        };

        let child_path = parent_path.join(name);

        let metadata = match std::fs::metadata(&child_path) {
            Ok(m) => m,
            Err(e) => {
                let errno = e.raw_os_error().unwrap_or(2);
                return self.write_error(header.unique, -errno, writable_bufs, mem);
            }
        };

        let nodeid = self.alloc_nodeid(child_path);

        let entry_out = FuseEntryOut {
            nodeid,
            generation: 0,
            entry_valid: 0,
            attr_valid: 0,
            entry_valid_nsec: 0,
            attr_valid_nsec: 0,
            attr: metadata_to_fuse_attr(&metadata, nodeid),
        };
        let body: &[u8] = unsafe {
            core::slice::from_raw_parts(
                &entry_out as *const _ as *const u8,
                core::mem::size_of::<FuseEntryOut>(),
            )
        };
        self.write_response(header.unique, body, writable_bufs, mem)
    }

    fn handle_getattr(
        &self,
        header: &FuseInHeader,
        writable_bufs: &[(u64, u32)],
        mem: &GuestMemoryMmap<()>,
    ) -> u32 {
        let path = match self.nodes.get(header.nodeid as usize) {
            Some(Some(node)) => node.host_path.clone(),
            _ => return self.write_error(header.unique, -2, writable_bufs, mem),
        };

        let metadata = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                let errno = e.raw_os_error().unwrap_or(5);
                return self.write_error(header.unique, -errno, writable_bufs, mem);
            }
        };

        let attr_out = FuseAttrOut {
            attr_valid: 0,
            attr_valid_nsec: 0,
            dummy: 0,
            attr: metadata_to_fuse_attr(&metadata, header.nodeid),
        };
        let body: &[u8] = unsafe {
            core::slice::from_raw_parts(
                &attr_out as *const _ as *const u8,
                core::mem::size_of::<FuseAttrOut>(),
            )
        };
        self.write_response(header.unique, body, writable_bufs, mem)
    }

    fn handle_setattr(
        &mut self,
        header: &FuseInHeader,
        req_data: &[u8],
        writable_bufs: &[(u64, u32)],
        mem: &GuestMemoryMmap<()>,
    ) -> u32 {
        let hdr_size = core::mem::size_of::<FuseInHeader>();
        let setattr_in_size = core::mem::size_of::<FuseSetattrIn>();
        if req_data.len() < hdr_size + setattr_in_size {
            return self.write_error(header.unique, -22, writable_bufs, mem);
        }

        let setattr = unsafe {
            core::ptr::read_unaligned(req_data[hdr_size..].as_ptr() as *const FuseSetattrIn)
        };
        if setattr.valid & FATTR_SIZE == 0 {
            return self.write_error(header.unique, -22, writable_bufs, mem);
        }
        if setattr.valid & !(FATTR_SIZE | FATTR_FH) != 0 {
            return self.write_error(header.unique, -95, writable_bufs, mem);
        }

        let path = match self.nodes.get(header.nodeid as usize) {
            Some(Some(node)) => node.host_path.clone(),
            _ => return self.write_error(header.unique, -2, writable_bufs, mem),
        };

        let set_len_result = if setattr.valid & FATTR_FH != 0 {
            match self.file_handles.get_mut(setattr.fh as usize) {
                Some(Some(file)) => file.set_len(setattr.size),
                _ => return self.write_error(header.unique, -9, writable_bufs, mem),
            }
        } else {
            OpenOptions::new()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_len(setattr.size))
        };
        if let Err(e) = set_len_result {
            let errno = e.raw_os_error().unwrap_or(5);
            return self.write_error(header.unique, -errno, writable_bufs, mem);
        }

        let metadata = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                let errno = e.raw_os_error().unwrap_or(5);
                return self.write_error(header.unique, -errno, writable_bufs, mem);
            }
        };
        let attr_out = FuseAttrOut {
            attr_valid: 0,
            attr_valid_nsec: 0,
            dummy: 0,
            attr: metadata_to_fuse_attr(&metadata, header.nodeid),
        };
        let body: &[u8] = unsafe {
            core::slice::from_raw_parts(
                &attr_out as *const _ as *const u8,
                core::mem::size_of::<FuseAttrOut>(),
            )
        };
        self.write_response(header.unique, body, writable_bufs, mem)
    }

    fn handle_unlink(
        &mut self,
        header: &FuseInHeader,
        req_data: &[u8],
        writable_bufs: &[(u64, u32)],
        mem: &GuestMemoryMmap<()>,
    ) -> u32 {
        let hdr_size = core::mem::size_of::<FuseInHeader>();
        let name_bytes = &req_data[hdr_size..];
        let name_end = name_bytes
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(name_bytes.len());
        let name = match std::str::from_utf8(&name_bytes[..name_end]) {
            Ok(s) => s,
            Err(_) => return self.write_error(header.unique, -22, writable_bufs, mem),
        };

        if name == ".." || name == "." || name.contains('/') || name.contains('\0') {
            return self.write_error(header.unique, -22, writable_bufs, mem);
        }

        let parent_path = match self.nodes.get(header.nodeid as usize) {
            Some(Some(node)) => node.host_path.clone(),
            _ => return self.write_error(header.unique, -2, writable_bufs, mem),
        };
        let child_path = parent_path.join(name);

        if let Err(e) = std::fs::remove_file(&child_path) {
            let errno = e.raw_os_error().unwrap_or(5);
            return self.write_error(header.unique, -errno, writable_bufs, mem);
        }

        self.write_response(header.unique, &[], writable_bufs, mem)
    }

    fn handle_mkdir(
        &mut self,
        header: &FuseInHeader,
        req_data: &[u8],
        writable_bufs: &[(u64, u32)],
        mem: &GuestMemoryMmap<()>,
    ) -> u32 {
        let hdr_size = core::mem::size_of::<FuseInHeader>();
        let mkdir_in_size = core::mem::size_of::<FuseMkdirIn>();
        if req_data.len() < hdr_size + mkdir_in_size {
            return self.write_error(header.unique, -22, writable_bufs, mem);
        }

        let name_bytes = &req_data[hdr_size + mkdir_in_size..];
        let name_end = name_bytes
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(name_bytes.len());
        let name = match std::str::from_utf8(&name_bytes[..name_end]) {
            Ok(s) => s,
            Err(_) => return self.write_error(header.unique, -22, writable_bufs, mem),
        };

        if name == ".." || name == "." || name.contains('/') || name.contains('\0') {
            return self.write_error(header.unique, -22, writable_bufs, mem);
        }

        let parent_path = match self.nodes.get(header.nodeid as usize) {
            Some(Some(node)) => node.host_path.clone(),
            _ => return self.write_error(header.unique, -2, writable_bufs, mem),
        };
        let child_path = parent_path.join(name);

        if let Err(e) = std::fs::create_dir(&child_path) {
            let errno = e.raw_os_error().unwrap_or(5);
            return self.write_error(header.unique, -errno, writable_bufs, mem);
        }

        let metadata = match std::fs::metadata(&child_path) {
            Ok(m) => m,
            Err(e) => {
                let errno = e.raw_os_error().unwrap_or(5);
                return self.write_error(header.unique, -errno, writable_bufs, mem);
            }
        };
        let nodeid = self.alloc_nodeid(child_path);
        let entry_out = FuseEntryOut {
            nodeid,
            generation: 0,
            entry_valid: 0,
            attr_valid: 0,
            entry_valid_nsec: 0,
            attr_valid_nsec: 0,
            attr: metadata_to_fuse_attr(&metadata, nodeid),
        };
        let body: &[u8] = unsafe {
            core::slice::from_raw_parts(
                &entry_out as *const _ as *const u8,
                core::mem::size_of::<FuseEntryOut>(),
            )
        };
        self.write_response(header.unique, body, writable_bufs, mem)
    }

    fn handle_rmdir(
        &mut self,
        header: &FuseInHeader,
        req_data: &[u8],
        writable_bufs: &[(u64, u32)],
        mem: &GuestMemoryMmap<()>,
    ) -> u32 {
        let hdr_size = core::mem::size_of::<FuseInHeader>();
        let name_bytes = &req_data[hdr_size..];
        let name_end = name_bytes
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(name_bytes.len());
        let name = match std::str::from_utf8(&name_bytes[..name_end]) {
            Ok(s) => s,
            Err(_) => return self.write_error(header.unique, -22, writable_bufs, mem),
        };

        if name == ".." || name == "." || name.contains('/') || name.contains('\0') {
            return self.write_error(header.unique, -22, writable_bufs, mem);
        }

        let parent_path = match self.nodes.get(header.nodeid as usize) {
            Some(Some(node)) => node.host_path.clone(),
            _ => return self.write_error(header.unique, -2, writable_bufs, mem),
        };
        let child_path = parent_path.join(name);

        if let Err(e) = std::fs::remove_dir(&child_path) {
            let errno = e.raw_os_error().unwrap_or(5);
            return self.write_error(header.unique, -errno, writable_bufs, mem);
        }

        self.write_response(header.unique, &[], writable_bufs, mem)
    }

    fn handle_rename(
        &mut self,
        header: &FuseInHeader,
        req_data: &[u8],
        writable_bufs: &[(u64, u32)],
        mem: &GuestMemoryMmap<()>,
    ) -> u32 {
        let hdr_size = core::mem::size_of::<FuseInHeader>();
        let rename_in_size = core::mem::size_of::<FuseRenameIn>();
        if req_data.len() < hdr_size + rename_in_size {
            return self.write_error(header.unique, -22, writable_bufs, mem);
        }

        let rename_in = unsafe {
            core::ptr::read_unaligned(req_data[hdr_size..].as_ptr() as *const FuseRenameIn)
        };
        let names = &req_data[hdr_size + rename_in_size..];
        let Some(old_end) = names.iter().position(|&b| b == 0) else {
            return self.write_error(header.unique, -22, writable_bufs, mem);
        };
        let old_name = match std::str::from_utf8(&names[..old_end]) {
            Ok(s) => s,
            Err(_) => return self.write_error(header.unique, -22, writable_bufs, mem),
        };
        let new_bytes = &names[old_end + 1..];
        let Some(new_end) = new_bytes.iter().position(|&b| b == 0) else {
            return self.write_error(header.unique, -22, writable_bufs, mem);
        };
        let new_name = match std::str::from_utf8(&new_bytes[..new_end]) {
            Ok(s) => s,
            Err(_) => return self.write_error(header.unique, -22, writable_bufs, mem),
        };

        if old_name == ".."
            || old_name == "."
            || old_name.contains('/')
            || old_name.contains('\0')
            || new_name == ".."
            || new_name == "."
            || new_name.contains('/')
            || new_name.contains('\0')
        {
            return self.write_error(header.unique, -22, writable_bufs, mem);
        }

        let old_parent = match self.nodes.get(header.nodeid as usize) {
            Some(Some(node)) => node.host_path.clone(),
            _ => return self.write_error(header.unique, -2, writable_bufs, mem),
        };
        let new_parent = match self.nodes.get(rename_in.newdir as usize) {
            Some(Some(node)) => node.host_path.clone(),
            _ => return self.write_error(header.unique, -2, writable_bufs, mem),
        };

        if let Err(e) = std::fs::rename(old_parent.join(old_name), new_parent.join(new_name)) {
            let errno = e.raw_os_error().unwrap_or(5);
            return self.write_error(header.unique, -errno, writable_bufs, mem);
        }

        self.write_response(header.unique, &[], writable_bufs, mem)
    }

    fn handle_create(
        &mut self,
        header: &FuseInHeader,
        req_data: &[u8],
        writable_bufs: &[(u64, u32)],
        mem: &GuestMemoryMmap<()>,
    ) -> u32 {
        let hdr_size = core::mem::size_of::<FuseInHeader>();
        let create_in_size = core::mem::size_of::<FuseCreateIn>();

        if req_data.len() < hdr_size + create_in_size {
            return self.write_error(header.unique, -22, writable_bufs, mem);
        }

        let create_in = unsafe { &*(req_data[hdr_size..].as_ptr() as *const FuseCreateIn) };

        let name_bytes = &req_data[hdr_size + create_in_size..];
        let name_end = name_bytes
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(name_bytes.len());
        let name = match std::str::from_utf8(&name_bytes[..name_end]) {
            Ok(s) => s,
            Err(_) => return self.write_error(header.unique, -22, writable_bufs, mem),
        };

        if name == ".." || name == "." || name.contains('/') || name.contains('\0') {
            return self.write_error(header.unique, -22, writable_bufs, mem);
        }

        let parent_path = match self.nodes.get(header.nodeid as usize) {
            Some(Some(node)) => node.host_path.clone(),
            _ => return self.write_error(header.unique, -2, writable_bufs, mem),
        };

        let child_path = parent_path.join(name);

        let file = match open_file(&child_path, create_in.flags | 0o100, create_in.mode) {
            // ensure O_CREAT
            Ok(f) => f,
            Err(e) => {
                let errno = e.raw_os_error().unwrap_or(5);
                return self.write_error(header.unique, -errno, writable_bufs, mem);
            }
        };

        let metadata = match std::fs::metadata(&child_path) {
            Ok(m) => m,
            Err(e) => {
                let errno = e.raw_os_error().unwrap_or(5);
                return self.write_error(header.unique, -errno, writable_bufs, mem);
            }
        };

        let nodeid = self.alloc_nodeid(child_path);
        let fh = self.alloc_fh(file);

        // Response: FuseEntryOut + FuseOpenOut
        let entry_out = FuseEntryOut {
            nodeid,
            generation: 0,
            entry_valid: 0,
            attr_valid: 0,
            entry_valid_nsec: 0,
            attr_valid_nsec: 0,
            attr: metadata_to_fuse_attr(&metadata, nodeid),
        };
        let open_out = FuseOpenOut {
            fh,
            open_flags: 0,
            padding: 0,
        };

        let entry_size = core::mem::size_of::<FuseEntryOut>();
        let open_size = core::mem::size_of::<FuseOpenOut>();
        let mut body = vec![0u8; entry_size + open_size];
        unsafe {
            core::ptr::copy_nonoverlapping(
                &entry_out as *const _ as *const u8,
                body.as_mut_ptr(),
                entry_size,
            );
            core::ptr::copy_nonoverlapping(
                &open_out as *const _ as *const u8,
                body.as_mut_ptr().add(entry_size),
                open_size,
            );
        }

        self.write_response(header.unique, &body, writable_bufs, mem)
    }

    fn handle_open(
        &mut self,
        header: &FuseInHeader,
        req_data: &[u8],
        writable_bufs: &[(u64, u32)],
        mem: &GuestMemoryMmap<()>,
    ) -> u32 {
        let path = match self.nodes.get(header.nodeid as usize) {
            Some(Some(node)) => node.host_path.clone(),
            _ => return self.write_error(header.unique, -2, writable_bufs, mem),
        };

        let hdr_size = core::mem::size_of::<FuseInHeader>();
        let open_in = unsafe { &*(req_data[hdr_size..].as_ptr() as *const FuseOpenIn) };

        let file = match open_file(&path, open_in.flags, 0) {
            Ok(f) => f,
            Err(e) => {
                let errno = e.raw_os_error().unwrap_or(5);
                return self.write_error(header.unique, -errno, writable_bufs, mem);
            }
        };

        let fh = self.alloc_fh(file);

        let open_out = FuseOpenOut {
            fh,
            open_flags: 0,
            padding: 0,
        };
        let body: &[u8] = unsafe {
            core::slice::from_raw_parts(
                &open_out as *const _ as *const u8,
                core::mem::size_of::<FuseOpenOut>(),
            )
        };
        self.write_response(header.unique, body, writable_bufs, mem)
    }

    fn handle_read(
        &mut self,
        header: &FuseInHeader,
        req_data: &[u8],
        writable_bufs: &[(u64, u32)],
        mem: &GuestMemoryMmap<()>,
    ) -> u32 {
        let hdr_size = core::mem::size_of::<FuseInHeader>();
        let read_in = unsafe { &*(req_data[hdr_size..].as_ptr() as *const FuseReadIn) };

        let fh = read_in.fh as usize;
        let file = match self.file_handles.get_mut(fh) {
            Some(Some(f)) => f,
            _ => return self.write_error(header.unique, -9, writable_bufs, mem),
        };

        // For read: writable_bufs[0] = FuseOutHeader, writable_bufs[1] = data buffer
        let out_hdr_size = core::mem::size_of::<FuseOutHeader>();
        let mut bytes_read = 0usize;
        if read_in.size > 0 && writable_bufs.len() > 1 {
            let (data_addr, data_len) = writable_bufs[1];
            let size = (read_in.size as usize).min(data_len as usize);
            let fd = file.as_raw_fd();
            match mem.get_slice(GuestAddress(data_addr), size) {
                Ok(guest_buf) => {
                    while bytes_read < size {
                        let mut chunk = match guest_buf.offset(bytes_read) {
                            Ok(slice) => slice,
                            Err(_) => break,
                        };
                        match pread_guest(fd, &mut chunk, read_in.offset + bytes_read as u64) {
                            Ok(0) => break,
                            Ok(n) => bytes_read += n,
                            Err(e) => {
                                if bytes_read > 0 {
                                    break;
                                }
                                return self.write_error(header.unique, -e, writable_bufs, mem);
                            }
                        }
                    }
                }
                Err(_) => return self.write_error(header.unique, -14, writable_bufs, mem),
            }
        }

        let out_header = FuseOutHeader {
            len: (out_hdr_size + bytes_read) as u32,
            error: 0,
            unique: header.unique,
        };
        let hdr_bytes: &[u8] = unsafe {
            core::slice::from_raw_parts(&out_header as *const _ as *const u8, out_hdr_size)
        };

        if writable_bufs.is_empty() {
            return 0;
        }

        let (hdr_addr, _) = writable_bufs[0];
        mem.write_slice(hdr_bytes, GuestAddress(hdr_addr)).unwrap();
        out_hdr_size as u32 + bytes_read as u32
    }

    fn handle_readdir(
        &mut self,
        header: &FuseInHeader,
        req_data: &[u8],
        writable_bufs: &[(u64, u32)],
        mem: &GuestMemoryMmap<()>,
    ) -> u32 {
        let hdr_size = core::mem::size_of::<FuseInHeader>();
        let read_in = unsafe { &*(req_data[hdr_size..].as_ptr() as *const FuseReadIn) };

        let path = match self.nodes.get(header.nodeid as usize) {
            Some(Some(node)) => node.host_path.clone(),
            _ => return self.write_error(header.unique, -2, writable_bufs, mem),
        };

        let entries: Vec<_> = match std::fs::read_dir(&path) {
            Ok(iter) => iter.filter_map(|e| e.ok()).collect(),
            Err(e) => {
                let errno = e.raw_os_error().unwrap_or(5);
                return self.write_error(header.unique, -errno, writable_bufs, mem);
            }
        };

        let max_size = read_in.size as usize;
        let start_offset = read_in.offset as usize;
        let mut buf = Vec::new();
        let dirent_hdr_size = core::mem::size_of::<FuseDirent>();

        // Entries: ".", "..", then real entries. Offset is 1-based entry index.
        let total_entries = 2 + entries.len();

        for idx in start_offset..total_entries {
            // Extract name bytes, ino, and type for this entry.
            let owned_name: std::ffi::OsString;
            let (name_bytes, ino, typ): (&[u8], u64, u32) = if idx == 0 {
                (b".", header.nodeid, 4)
            } else if idx == 1 {
                (b"..", 1, 4)
            } else {
                let entry = &entries[idx - 2];
                let ft = entry.file_type().ok();
                let typ = match ft {
                    Some(t) if t.is_dir() => 4,
                    Some(t) if t.is_file() => 8,
                    Some(t) if t.is_symlink() => 10,
                    _ => 0,
                };
                owned_name = entry.file_name();
                (owned_name.as_bytes(), (idx + 1) as u64, typ)
            };

            let namelen = name_bytes.len();
            let entry_size = fuse_dirent_align(dirent_hdr_size + namelen);

            if buf.len() + entry_size > max_size {
                break;
            }

            let dirent = FuseDirent {
                ino,
                off: (idx + 1) as u64,
                namelen: namelen as u32,
                typ,
            };

            let dirent_bytes: &[u8] = unsafe {
                core::slice::from_raw_parts(&dirent as *const _ as *const u8, dirent_hdr_size)
            };
            buf.extend_from_slice(dirent_bytes);
            buf.extend_from_slice(name_bytes);
            while buf.len() % 8 != 0 {
                buf.push(0);
            }
        }

        // For READDIR, response format is: header in buf[0], data in buf[1]
        let out_hdr_size = core::mem::size_of::<FuseOutHeader>();
        let out_header = FuseOutHeader {
            len: (out_hdr_size + buf.len()) as u32,
            error: 0,
            unique: header.unique,
        };
        let hdr_bytes: &[u8] = unsafe {
            core::slice::from_raw_parts(&out_header as *const _ as *const u8, out_hdr_size)
        };

        if writable_bufs.is_empty() {
            return 0;
        }

        let (hdr_addr, _) = writable_bufs[0];
        mem.write_slice(hdr_bytes, GuestAddress(hdr_addr)).unwrap();
        let mut total = out_hdr_size as u32;

        if !buf.is_empty() && writable_bufs.len() > 1 {
            let (data_addr, _) = writable_bufs[1];
            mem.write_slice(&buf, GuestAddress(data_addr)).unwrap();
            total += buf.len() as u32;
        }

        total
    }

    fn handle_write(
        &mut self,
        header: &FuseInHeader,
        req_data: &[u8],
        readable_bufs: &[(u64, u32)],
        writable_bufs: &[(u64, u32)],
        mem: &GuestMemoryMmap<()>,
    ) -> u32 {
        let hdr_size = core::mem::size_of::<FuseInHeader>();
        let write_in = unsafe { &*(req_data[hdr_size..].as_ptr() as *const FuseWriteIn) };

        let fh = write_in.fh as usize;
        let file = match self.file_handles.get_mut(fh) {
            Some(Some(f)) => f,
            _ => return self.write_error(header.unique, -9, writable_bufs, mem),
        };

        let mut bytes_written = 0usize;
        if write_in.size > 0 && readable_bufs.len() > 1 {
            let (data_addr, data_len) = readable_bufs[1];
            let size = (write_in.size as usize).min(data_len as usize);
            let fd = file.as_raw_fd();
            match mem.get_slice(GuestAddress(data_addr), size) {
                Ok(guest_buf) => {
                    while bytes_written < size {
                        let chunk = match guest_buf.offset(bytes_written) {
                            Ok(slice) => slice,
                            Err(_) => break,
                        };
                        match pwrite_guest(fd, &chunk, write_in.offset + bytes_written as u64) {
                            Ok(0) => break,
                            Ok(n) => bytes_written += n,
                            Err(e) => {
                                if bytes_written > 0 {
                                    break;
                                }
                                return self.write_error(header.unique, -e, writable_bufs, mem);
                            }
                        }
                    }
                }
                Err(_) => return self.write_error(header.unique, -14, writable_bufs, mem),
            }
        }

        let write_out = FuseWriteOut {
            size: bytes_written as u32,
            padding: 0,
        };
        let body: &[u8] = unsafe {
            core::slice::from_raw_parts(
                &write_out as *const _ as *const u8,
                core::mem::size_of::<FuseWriteOut>(),
            )
        };
        self.write_response(header.unique, body, writable_bufs, mem)
    }

    fn handle_release(
        &mut self,
        header: &FuseInHeader,
        req_data: &[u8],
        writable_bufs: &[(u64, u32)],
        mem: &GuestMemoryMmap<()>,
    ) -> u32 {
        let hdr_size = core::mem::size_of::<FuseInHeader>();
        if req_data.len() >= hdr_size + core::mem::size_of::<FuseReleaseIn>() {
            let release_in = unsafe { &*(req_data[hdr_size..].as_ptr() as *const FuseReleaseIn) };
            let fh = release_in.fh as usize;
            if fh < self.file_handles.len() {
                self.file_handles[fh] = None;
            }
        }
        self.write_response(header.unique, &[], writable_bufs, mem)
    }

    fn handle_fsync(
        &mut self,
        header: &FuseInHeader,
        req_data: &[u8],
        writable_bufs: &[(u64, u32)],
        mem: &GuestMemoryMmap<()>,
    ) -> u32 {
        let hdr_size = core::mem::size_of::<FuseInHeader>();
        if req_data.len() < hdr_size + core::mem::size_of::<FuseFsyncIn>() {
            return self.write_error(header.unique, -22, writable_bufs, mem);
        }
        // SAFETY: bounds checked above; FuseFsyncIn is repr(C) plain data.
        let fsync_in = unsafe { &*(req_data[hdr_size..].as_ptr() as *const FuseFsyncIn) };
        let Some(Some(file)) = self.file_handles.get(fsync_in.fh as usize) else {
            return self.write_error(header.unique, -9, writable_bufs, mem); // EBADF
        };
        let res = if fsync_in.fsync_flags & 1 != 0 {
            file.sync_data()
        } else {
            file.sync_all()
        };
        match res {
            Ok(()) => self.write_response(header.unique, &[], writable_bufs, mem),
            Err(e) => {
                let errno = e.raw_os_error().unwrap_or(5);
                self.write_error(header.unique, -errno, writable_bufs, mem)
            }
        }
    }

    fn handle_forget(&mut self, header: &FuseInHeader, _req_data: &[u8]) {
        let nodeid = header.nodeid as usize;
        if let Some(Some(node)) = self.nodes.get_mut(nodeid)
            && nodeid > 1
        {
            node.lookup_count = node.lookup_count.saturating_sub(1);
        }
    }

    fn handle_setupmapping(
        &mut self,
        header: &FuseInHeader,
        req_data: &[u8],
        writable_bufs: &[(u64, u32)],
        mem: &GuestMemoryMmap<()>,
    ) -> u32 {
        let hdr_size = core::mem::size_of::<FuseInHeader>();
        let in_size = core::mem::size_of::<FuseSetupMappingIn>();

        if req_data.len() < hdr_size + in_size {
            return self.write_error(header.unique, -22, writable_bufs, mem);
        }

        // SAFETY: req_data is large enough and aligned for FuseSetupMappingIn (repr(C)).
        let setup_in = unsafe { &*(req_data[hdr_size..].as_ptr() as *const FuseSetupMappingIn) };

        let dax_base = match self.dax_host_ptr {
            Some(ptr) => ptr,
            None => return self.write_error(header.unique, -12, writable_bufs, mem),
        };

        let moffset = setup_in.moffset as usize;
        let len = setup_in.len as usize;
        if moffset
            .checked_add(len)
            .is_none_or(|end| end > DAX_WINDOW_SIZE)
        {
            return self.write_error(header.unique, -22, writable_bufs, mem);
        }

        let fh = setup_in.fh as usize;
        let file = match self.file_handles.get(fh) {
            Some(Some(f)) => f,
            _ => return self.write_error(header.unique, -9, writable_bufs, mem),
        };

        let fd = file.as_raw_fd();
        let mmap_flags = libc::MAP_FIXED | libc::MAP_SHARED;
        let prot = if setup_in.flags & FUSE_SETUPMAPPING_FLAG_WRITE != 0 {
            libc::PROT_READ | libc::PROT_WRITE
        } else {
            libc::PROT_READ
        };

        // SAFETY: dax_base + moffset is within the 128 GB DAX window (bounds verified above)
        // that was registered as a KVM memslot. MAP_FIXED replaces the anonymous mapping
        // with a shared file mapping, which is what DAX requires.
        let target = unsafe { dax_base.add(moffset) };
        let result = unsafe {
            libc::mmap(
                target as *mut libc::c_void,
                len as libc::size_t,
                prot,
                mmap_flags,
                fd,
                setup_in.foffset as libc::off_t,
            )
        };

        if result == libc::MAP_FAILED {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(5);
            return self.write_error(header.unique, -errno, writable_bufs, mem);
        }

        self.write_response(header.unique, &[], writable_bufs, mem)
    }

    fn handle_removemapping(
        &mut self,
        header: &FuseInHeader,
        req_data: &[u8],
        writable_bufs: &[(u64, u32)],
        mem: &GuestMemoryMmap<()>,
    ) -> u32 {
        let hdr_size = core::mem::size_of::<FuseInHeader>();
        let in_size = core::mem::size_of::<FuseRemoveMappingIn>();
        let one_size = core::mem::size_of::<FuseRemoveMappingOne>();

        if req_data.len() < hdr_size + in_size + one_size {
            return self.write_error(header.unique, -22, writable_bufs, mem);
        }

        let dax_base = match self.dax_host_ptr {
            Some(ptr) => ptr,
            None => return self.write_error(header.unique, -12, writable_bufs, mem),
        };

        // SAFETY: We verified req_data has enough bytes. Using read_unaligned because
        // FuseRemoveMappingOne starts at byte offset 44 (hdr=40 + in=4), which is not
        // 8-byte aligned.
        let remove_one = unsafe {
            core::ptr::read_unaligned(
                req_data[hdr_size + in_size..].as_ptr() as *const FuseRemoveMappingOne
            )
        };

        let moffset = remove_one.moffset as usize;
        let len = remove_one.len as usize;
        if moffset
            .checked_add(len)
            .is_none_or(|end| end > DAX_WINDOW_SIZE)
        {
            return self.write_error(header.unique, -22, writable_bufs, mem);
        }

        // Replace the shared file mapping with an anonymous private mapping,
        // which disconnects that DAX slot from the file without unmapping the
        // guest physical address (KVM still needs the host VA range present).
        // SAFETY: target is within the DAX window (bounds verified above);
        // MAP_FIXED replaces the previous mapping.
        let target = unsafe { dax_base.add(moffset) };
        let result = unsafe {
            libc::mmap(
                target as *mut libc::c_void,
                len as libc::size_t,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_FIXED | libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };

        if result == libc::MAP_FAILED {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(5);
            return self.write_error(header.unique, -errno, writable_bufs, mem);
        }

        self.write_response(header.unique, &[], writable_bufs, mem)
    }
}

impl VirtioBackend for VirtioFs {
    fn device_id(&self) -> u32 {
        sumi_abi::virtio::VIRTIO_DEVICE_FS
    }

    fn num_queues(&self) -> usize {
        2
    }

    fn process_queue(
        &mut self,
        _queue_idx: usize,
        queue: &VirtqueueState,
        mem: &GuestMemoryMmap<()>,
    ) {
        self.process_queue_inner(queue, mem);
    }
}

/// Open `path` with the guest's Linux open(2) flags passed through to the
/// host kernel (both sides are Linux x86_64). A raw `libc::open` instead of
/// `OpenOptions` because the latter cannot express every valid flag
/// combination — most notably `O_RDONLY|O_CREAT` (create, then open
/// read-only), which mysqld's datadir writability probe uses and
/// `OpenOptions` rejects as `InvalidInput` with no OS errno (previously
/// surfacing as a bogus EIO in the guest).
fn open_file(path: &std::path::Path, flags: u32, mode: u32) -> std::io::Result<File> {
    use std::os::fd::FromRawFd;

    // Only forward flag bits that make sense for a host-side file open;
    // everything else (O_NOCTTY, mysql's legacy junk bits, O_DIRECT, ...)
    // is dropped. O_CLOEXEC is forced so guest files never leak into
    // host child processes.
    const ALLOWED: u32 = 0o3          // O_ACCMODE
        | 0o100                        // O_CREAT
        | 0o200                        // O_EXCL
        | 0o1000                       // O_TRUNC
        | 0o2000                       // O_APPEND
        | 0o400000; // O_NOFOLLOW
    let host_flags = (flags & ALLOWED) as i32 | libc::O_CLOEXEC;

    let cpath = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: `cpath` is a valid NUL-terminated path and `host_flags`/`mode`
    // are plain integers; open(2) has no other preconditions.
    let fd = unsafe { libc::open(cpath.as_ptr(), host_flags, mode as libc::c_uint) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` is a freshly opened descriptor exclusively owned here.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn metadata_to_fuse_attr(meta: &std::fs::Metadata, ino: u64) -> FuseAttr {
    FuseAttr {
        ino,
        size: meta.len(),
        blocks: meta.blocks(),
        atime: meta.atime() as u64,
        mtime: meta.mtime() as u64,
        ctime: meta.ctime() as u64,
        atimensec: meta.atime_nsec() as u32,
        mtimensec: meta.mtime_nsec() as u32,
        ctimensec: meta.ctime_nsec() as u32,
        mode: meta.mode(),
        nlink: meta.nlink() as u32,
        uid: meta.uid(),
        gid: meta.gid(),
        rdev: meta.rdev() as u32,
        blksize: meta.blksize() as u32,
        flags: 0,
    }
}
