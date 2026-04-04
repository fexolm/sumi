use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use sumi_abi::fuse::*;
use sumi_abi::virtio::*;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use super::virtio_mmio::VirtqueueState;

struct FuseNode {
    host_path: PathBuf,
    _lookup_count: u64,
}

pub struct VirtioFs {
    _share_root: PathBuf,
    nodes: Vec<Option<FuseNode>>,
    file_handles: Vec<Option<File>>,
    last_avail_idx: u16,
}

impl VirtioFs {
    pub fn new(share_dir: &std::path::Path) -> Self {
        let mut nodes = Vec::new();
        // nodeid 0 is unused (FUSE convention)
        nodes.push(None);
        // nodeid 1 = root
        nodes.push(Some(FuseNode {
            host_path: share_dir.to_path_buf(),
            _lookup_count: 1,
        }));

        Self {
            _share_root: share_dir.to_path_buf(),
            nodes,
            file_handles: Vec::new(),
            last_avail_idx: 0,
        }
    }

    fn alloc_nodeid(&mut self, path: PathBuf) -> u64 {
        let nodeid = self.nodes.len() as u64;
        self.nodes.push(Some(FuseNode {
            host_path: path,
            _lookup_count: 1,
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

    pub fn process_queue(&mut self, queue: &VirtqueueState, mem: &GuestMemoryMmap<()>) {
        // Read available ring idx
        let mut buf = [0u8; 4];
        mem.read_slice(&mut buf, GuestAddress(queue.avail_addr))
            .unwrap();
        let avail_idx = u16::from_le_bytes([buf[2], buf[3]]);

        while self.last_avail_idx != avail_idx {
            let ring_idx = (self.last_avail_idx % QUEUE_SIZE) as u64;
            let mut head_buf = [0u8; 2];
            mem.read_slice(
                &mut head_buf,
                GuestAddress(queue.avail_addr + 4 + ring_idx * 2),
            )
            .unwrap();
            let head = u16::from_le_bytes(head_buf);

            let total_written = self.process_descriptor_chain(queue, head, mem);

            // Write to used ring
            let mut used_buf = [0u8; 4];
            mem.read_slice(&mut used_buf, GuestAddress(queue.used_addr))
                .unwrap();
            let used_idx = u16::from_le_bytes([used_buf[2], used_buf[3]]);

            let entry_addr = queue.used_addr + 4 + (used_idx % QUEUE_SIZE) as u64 * 8;
            mem.write_slice(&(head as u32).to_le_bytes(), GuestAddress(entry_addr))
                .unwrap();
            mem.write_slice(
                &total_written.to_le_bytes(),
                GuestAddress(entry_addr + 4),
            )
            .unwrap();

            let new_used_idx = used_idx.wrapping_add(1);
            // Write flags (unchanged) and new idx
            mem.write_slice(
                &new_used_idx.to_le_bytes(),
                GuestAddress(queue.used_addr + 2),
            )
            .unwrap();

            self.last_avail_idx = self.last_avail_idx.wrapping_add(1);
        }
    }

    fn process_descriptor_chain(
        &mut self,
        queue: &VirtqueueState,
        head: u16,
        mem: &GuestMemoryMmap<()>,
    ) -> u32 {
        let mut readable_bufs: Vec<(u64, u32)> = Vec::new();
        let mut writable_bufs: Vec<(u64, u32)> = Vec::new();

        let mut idx = head;
        loop {
            let desc = self.read_desc(queue, idx, mem);
            if desc.flags & VIRTQ_DESC_F_WRITE != 0 {
                writable_bufs.push((desc.addr, desc.len));
            } else {
                readable_bufs.push((desc.addr, desc.len));
            }
            if desc.flags & VIRTQ_DESC_F_NEXT == 0 {
                break;
            }
            idx = desc.next;
        }

        if readable_bufs.is_empty() {
            return 0;
        }

        // Read FUSE request from first readable buffer
        let (req_addr, req_len) = readable_bufs[0];
        let mut req_data = vec![0u8; req_len as usize];
        mem.read_slice(&mut req_data, GuestAddress(req_addr))
            .unwrap();

        if req_data.len() < core::mem::size_of::<FuseInHeader>() {
            return 0;
        }

        // SAFETY: req_data is large enough for FuseInHeader, and the struct is repr(C).
        let header = unsafe { &*(req_data.as_ptr() as *const FuseInHeader) };

        self.dispatch_fuse(header, &req_data, &readable_bufs, &writable_bufs, mem)
    }

    fn read_desc(
        &self,
        queue: &VirtqueueState,
        idx: u16,
        mem: &GuestMemoryMmap<()>,
    ) -> VirtqDesc {
        let addr = queue.desc_addr + idx as u64 * 16;
        let mut buf = [0u8; 16];
        mem.read_slice(&mut buf, GuestAddress(addr)).unwrap();
        VirtqDesc {
            addr: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            len: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            flags: u16::from_le_bytes(buf[12..14].try_into().unwrap()),
            next: u16::from_le_bytes(buf[14..16].try_into().unwrap()),
        }
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
            FUSE_CREATE => self.handle_create(header, req_data, writable_bufs, mem),
            FUSE_OPEN | FUSE_OPENDIR => self.handle_open(header, req_data, writable_bufs, mem),
            FUSE_READ => self.handle_read(header, req_data, writable_bufs, mem),
            FUSE_WRITE => self.handle_write(header, req_data, readable_bufs, writable_bufs, mem),
            FUSE_RELEASE | FUSE_RELEASEDIR => {
                self.handle_release(header, req_data, writable_bufs, mem)
            }
            FUSE_FORGET => {
                self.handle_forget(header, req_data);
                0
            }
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
        let hdr_bytes: &[u8] = unsafe {
            core::slice::from_raw_parts(&out_header as *const _ as *const u8, hdr_size)
        };

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
        let hdr_bytes: &[u8] = unsafe {
            core::slice::from_raw_parts(&out_header as *const _ as *const u8, hdr_size)
        };

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
            map_alignment: 0,
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
        let name_end = name_bytes.iter().position(|&b| b == 0).unwrap_or(name_bytes.len());
        let name = match std::str::from_utf8(&name_bytes[..name_end]) {
            Ok(s) => s,
            Err(_) => return self.write_error(header.unique, -22, writable_bufs, mem),
        };

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

        let parent_path = match self.nodes.get(header.nodeid as usize) {
            Some(Some(node)) => node.host_path.clone(),
            _ => return self.write_error(header.unique, -2, writable_bufs, mem),
        };

        let child_path = parent_path.join(name);

        let file = match open_file(&child_path, create_in.flags | 0o100) {
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

        let file = match open_file(&path, open_in.flags) {
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

        let size = read_in.size as usize;
        let mut data = vec![0u8; size];

        if file.seek(SeekFrom::Start(read_in.offset)).is_err() {
            return self.write_error(header.unique, -5, writable_bufs, mem);
        }
        let bytes_read = match file.read(&mut data) {
            Ok(n) => n,
            Err(e) => {
                let errno = e.raw_os_error().unwrap_or(5);
                return self.write_error(header.unique, -errno, writable_bufs, mem);
            }
        };
        data.truncate(bytes_read);

        // For read: writable_bufs[0] = FuseOutHeader, writable_bufs[1] = data buffer
        let out_hdr_size = core::mem::size_of::<FuseOutHeader>();
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
        let mut total = out_hdr_size as u32;

        if bytes_read > 0 && writable_bufs.len() > 1 {
            let (data_addr, _) = writable_bufs[1];
            mem.write_slice(&data[..bytes_read], GuestAddress(data_addr))
                .unwrap();
            total += bytes_read as u32;
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

        let size = write_in.size as usize;
        let mut data = vec![0u8; size];

        // Write data is in the second readable buffer
        if readable_bufs.len() > 1 {
            let (data_addr, data_len) = readable_bufs[1];
            let read_size = size.min(data_len as usize);
            mem.read_slice(&mut data[..read_size], GuestAddress(data_addr))
                .unwrap();
        }

        if file.seek(SeekFrom::Start(write_in.offset)).is_err() {
            return self.write_error(header.unique, -5, writable_bufs, mem);
        }
        let bytes_written = match file.write(&data) {
            Ok(n) => n,
            Err(e) => {
                let errno = e.raw_os_error().unwrap_or(5);
                return self.write_error(header.unique, -errno, writable_bufs, mem);
            }
        };

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
            let release_in =
                unsafe { &*(req_data[hdr_size..].as_ptr() as *const FuseReleaseIn) };
            let fh = release_in.fh as usize;
            if fh < self.file_handles.len() {
                self.file_handles[fh] = None;
            }
        }
        self.write_response(header.unique, &[], writable_bufs, mem)
    }

    fn handle_forget(&mut self, header: &FuseInHeader, _req_data: &[u8]) {
        let nodeid = header.nodeid as usize;
        if nodeid < self.nodes.len() && nodeid > 1 {
            self.nodes[nodeid] = None;
        }
    }
}

fn open_file(path: &std::path::Path, flags: u32) -> std::io::Result<File> {
    let access = flags & 3; // O_ACCMODE
    let mut opts = OpenOptions::new();

    match access {
        0 => {
            opts.read(true);
        }
        1 => {
            opts.write(true);
        }
        2 => {
            opts.read(true).write(true);
        }
        _ => {
            opts.read(true);
        }
    }

    if flags & 0o100 != 0 {
        opts.create(true);
    }
    if flags & 0o1000 != 0 {
        opts.truncate(true);
    }
    if flags & 0o2000 != 0 {
        opts.append(true);
    }

    opts.open(path)
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
