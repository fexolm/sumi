pub mod virtio_fs;

pub const MAX_FDS: usize = 256;

#[derive(Clone, Copy)]
pub enum FdKind {
    /// Console: debugcon port for output, no input yet.
    Console,
    /// Host file accessed via virtio-fs FUSE.
    File {
        fuse_fh: u64,
        fuse_nodeid: u64,
        offset: u64,
    },
    /// Host directory accessed via virtio-fs FUSE.
    Directory {
        fuse_fh: u64,
        fuse_nodeid: u64,
        offset: u64,
    },
}

#[derive(Clone, Copy)]
pub struct FileDescriptor {
    pub kind: FdKind,
    pub flags: u32,
}

pub struct FdTable {
    fds: [Option<FileDescriptor>; MAX_FDS],
}

impl FdTable {
    pub const fn new() -> Self {
        let mut fds = [const { None }; MAX_FDS];
        fds[0] = Some(FileDescriptor {
            kind: FdKind::Console,
            flags: 0,
        }); // stdin
        fds[1] = Some(FileDescriptor {
            kind: FdKind::Console,
            flags: 1,
        }); // stdout (O_WRONLY)
        fds[2] = Some(FileDescriptor {
            kind: FdKind::Console,
            flags: 1,
        }); // stderr (O_WRONLY)
        Self { fds }
    }

    pub fn get(&self, fd: usize) -> Option<&FileDescriptor> {
        if fd >= MAX_FDS {
            return None;
        }
        self.fds[fd].as_ref()
    }

    pub fn get_mut(&mut self, fd: usize) -> Option<&mut FileDescriptor> {
        if fd >= MAX_FDS {
            return None;
        }
        self.fds[fd].as_mut()
    }

    /// Allocate the lowest available fd, matching Linux's guarantee.
    pub fn alloc(&mut self, desc: FileDescriptor) -> Option<usize> {
        for (i, slot) in self.fds.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(desc);
                return Some(i);
            }
        }
        None
    }

    /// Free an fd slot and return the old descriptor.
    pub fn free(&mut self, fd: usize) -> Option<FileDescriptor> {
        if fd >= MAX_FDS {
            return None;
        }
        self.fds[fd].take()
    }
}
