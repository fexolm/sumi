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

    /// Place a descriptor at a specific fd number. If the slot is occupied,
    /// the old descriptor is returned so the caller can clean it up.
    pub fn put(&mut self, fd: usize, desc: FileDescriptor) -> Option<FileDescriptor> {
        if fd >= MAX_FDS {
            return None;
        }
        let old = self.fds[fd].take();
        self.fds[fd] = Some(desc);
        old
    }

    /// Count how many open fds reference the given FUSE file handle.
    /// Used to decide whether to release a handle after close/dup2.
    pub fn count_fh_refs(&self, fh: u64) -> usize {
        self.fds.iter().filter(|slot| {
            matches!(slot, Some(d) if match d.kind {
                FdKind::File { fuse_fh, .. } | FdKind::Directory { fuse_fh, .. } => fuse_fh == fh,
                _ => false,
            })
        }).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_fds_are_console() {
        let table = FdTable::new();
        for fd in 0..3 {
            let desc = table.get(fd).expect("fd 0-2 should exist");
            assert!(matches!(desc.kind, FdKind::Console));
        }
    }

    #[test]
    fn alloc_returns_lowest_free() {
        let mut table = FdTable::new();
        let desc = FileDescriptor {
            kind: FdKind::File { fuse_fh: 1, fuse_nodeid: 1, offset: 0 },
            flags: 0,
        };
        // First alloc should return fd 3 (0-2 are console)
        assert_eq!(table.alloc(desc), Some(3));
        assert_eq!(table.alloc(desc), Some(4));
    }

    #[test]
    fn free_and_realloc_reuses_slot() {
        let mut table = FdTable::new();
        let desc = FileDescriptor {
            kind: FdKind::File { fuse_fh: 1, fuse_nodeid: 1, offset: 0 },
            flags: 0,
        };
        let fd = table.alloc(desc).unwrap();
        assert_eq!(fd, 3);
        table.free(fd);
        // Re-alloc should reuse fd 3
        assert_eq!(table.alloc(desc), Some(3));
    }

    #[test]
    fn get_invalid_fd_returns_none() {
        let table = FdTable::new();
        assert!(table.get(MAX_FDS).is_none());
        assert!(table.get(MAX_FDS + 1).is_none());
        assert!(table.get(3).is_none()); // unallocated
    }

    #[test]
    fn free_unallocated_returns_none() {
        let mut table = FdTable::new();
        assert!(table.free(3).is_none());
        assert!(table.free(MAX_FDS).is_none());
    }

    #[test]
    fn alloc_full_returns_none() {
        let mut table = FdTable::new();
        let desc = FileDescriptor {
            kind: FdKind::Console,
            flags: 0,
        };
        // Fill all remaining slots (3..MAX_FDS)
        for _ in 3..MAX_FDS {
            assert!(table.alloc(desc).is_some());
        }
        // Table is full
        assert!(table.alloc(desc).is_none());
    }

    #[test]
    fn get_mut_updates_offset() {
        let mut table = FdTable::new();
        let desc = FileDescriptor {
            kind: FdKind::File { fuse_fh: 1, fuse_nodeid: 1, offset: 0 },
            flags: 0,
        };
        let fd = table.alloc(desc).unwrap();
        if let Some(d) = table.get_mut(fd) {
            if let FdKind::File { ref mut offset, .. } = d.kind {
                *offset = 42;
            }
        }
        let d = table.get(fd).unwrap();
        if let FdKind::File { offset, .. } = d.kind {
            assert_eq!(offset, 42);
        } else {
            panic!("expected File");
        }
    }
}
