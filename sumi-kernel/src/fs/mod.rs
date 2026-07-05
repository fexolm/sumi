use alloc::vec::Vec;

pub mod dax;
pub mod virtio_fs;

#[derive(Clone, Copy)]
pub enum FdKind {
    /// Console: debugcon port for output, no input yet.
    Console,
    /// Host file accessed via virtio-fs FUSE.
    File {
        fuse_fh: u64,
        fuse_nodeid: u64,
        offset: u64,
        /// Best-effort cached file length. Used by `mmap` to bound the host's
        /// `setup_mapping` length so DAX never extends past EOF (which would
        /// SIGBUS the host on access). Updated on every successful write,
        /// pwrite, and writev so a freshly grown file can be mapped through
        /// the same fd without re-opening it.
        size: u64,
    },
    /// Host directory accessed via virtio-fs FUSE.
    Directory {
        fuse_fh: u64,
        fuse_nodeid: u64,
        offset: u64,
    },
    /// A socket, indexing into `net::NetState::socktab`.
    Socket { id: usize },
    /// An epoll instance, indexing into `net::NetState::epolltab`.
    Epoll { id: usize },
    /// One end of an in-kernel pipe, indexing into `net::NetState::pipetab`.
    /// `write_end` distinguishes the read fd from the write fd — both index
    /// the same `PipeState`, but close()/refcounting/readiness treat them
    /// as independent sides (see `net::pipe`).
    Pipe { id: usize, write_end: bool },
}

#[derive(Clone, Copy)]
pub struct FileDescriptor {
    pub kind: FdKind,
    pub flags: u32,
}

pub struct FdTable {
    fds: Vec<Option<FileDescriptor>>,
}

impl Default for FdTable {
    fn default() -> Self {
        Self::new()
    }
}

impl FdTable {
    pub const fn new() -> Self {
        Self { fds: Vec::new() }
    }

    /// Set up stdin/stdout/stderr. Must be called once after the allocator is ready.
    pub fn init_defaults(&mut self) {
        self.fds.push(Some(FileDescriptor {
            kind: FdKind::Console,
            flags: 0,
        })); // stdin
        self.fds.push(Some(FileDescriptor {
            kind: FdKind::Console,
            flags: 1,
        })); // stdout (O_WRONLY)
        self.fds.push(Some(FileDescriptor {
            kind: FdKind::Console,
            flags: 1,
        })); // stderr (O_WRONLY)
    }

    pub fn get(&self, fd: usize) -> Option<&FileDescriptor> {
        self.fds.get(fd)?.as_ref()
    }

    pub fn get_mut(&mut self, fd: usize) -> Option<&mut FileDescriptor> {
        self.fds.get_mut(fd)?.as_mut()
    }

    /// Allocate the lowest available fd, matching Linux's guarantee.
    pub fn alloc(&mut self, desc: FileDescriptor) -> usize {
        for (i, slot) in self.fds.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(desc);
                return i;
            }
        }
        let fd = self.fds.len();
        self.fds.push(Some(desc));
        fd
    }

    /// Free an fd slot and return the old descriptor.
    pub fn free(&mut self, fd: usize) -> Option<FileDescriptor> {
        self.fds.get_mut(fd)?.take()
    }

    /// Place a descriptor at a specific fd number. If the slot is occupied,
    /// the old descriptor is returned so the caller can clean it up.
    pub fn put(&mut self, fd: usize, desc: FileDescriptor) -> Option<FileDescriptor> {
        // Grow the table if needed.
        if fd >= self.fds.len() {
            self.fds.resize(fd + 1, None);
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

    /// Count how many open fds reference the given socket table id.
    pub fn count_socket_refs(&self, id: usize) -> usize {
        self.fds
            .iter()
            .filter(|slot| matches!(slot, Some(d) if matches!(d.kind, FdKind::Socket { id: sid } if sid == id)))
            .count()
    }

    /// Count how many open fds reference the given epoll table id.
    pub fn count_epoll_refs(&self, id: usize) -> usize {
        self.fds
            .iter()
            .filter(|slot| matches!(slot, Some(d) if matches!(d.kind, FdKind::Epoll { id: eid } if eid == id)))
            .count()
    }

    /// Count how many open fds reference this exact (pipe id, end) pair.
    /// The read end and write end of the same pipe are refcounted
    /// independently — dup()ing a read fd must not affect the write end's
    /// count and vice versa.
    pub fn count_pipe_refs(&self, id: usize, write_end: bool) -> usize {
        self.fds
            .iter()
            .filter(|slot| {
                matches!(slot, Some(d) if matches!(
                    d.kind,
                    FdKind::Pipe { id: pid, write_end: pwe } if pid == id && pwe == write_end
                ))
            })
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_table() -> FdTable {
        let mut t = FdTable::new();
        t.init_defaults();
        t
    }

    #[test]
    fn default_fds_are_console() {
        let table = make_table();
        for fd in 0..3 {
            let desc = table.get(fd).expect("fd 0-2 should exist");
            assert!(matches!(desc.kind, FdKind::Console));
        }
    }

    #[test]
    fn alloc_returns_lowest_free() {
        let mut table = make_table();
        let desc = FileDescriptor {
            kind: FdKind::File {
                fuse_fh: 1,
                fuse_nodeid: 1,
                offset: 0,
                size: 0,
            },
            flags: 0,
        };
        // First alloc should return fd 3 (0-2 are console)
        assert_eq!(table.alloc(desc), 3);
        assert_eq!(table.alloc(desc), 4);
    }

    #[test]
    fn free_and_realloc_reuses_slot() {
        let mut table = make_table();
        let desc = FileDescriptor {
            kind: FdKind::File {
                fuse_fh: 1,
                fuse_nodeid: 1,
                offset: 0,
                size: 0,
            },
            flags: 0,
        };
        let fd = table.alloc(desc);
        assert_eq!(fd, 3);
        table.free(fd);
        // Re-alloc should reuse fd 3
        assert_eq!(table.alloc(desc), 3);
    }

    #[test]
    fn get_invalid_fd_returns_none() {
        let table = make_table();
        assert!(table.get(1000).is_none());
        assert!(table.get(3).is_none()); // unallocated
    }

    #[test]
    fn free_unallocated_returns_none() {
        let mut table = make_table();
        assert!(table.free(3).is_none());
        assert!(table.free(1000).is_none());
    }

    #[test]
    fn alloc_grows_table() {
        let mut table = make_table();
        let desc = FileDescriptor {
            kind: FdKind::Console,
            flags: 0,
        };
        // Alloc many fds — table should grow without limit.
        for expected_fd in 3..300 {
            assert_eq!(table.alloc(desc), expected_fd);
        }
    }

    #[test]
    fn get_mut_updates_offset() {
        let mut table = make_table();
        let desc = FileDescriptor {
            kind: FdKind::File {
                fuse_fh: 1,
                fuse_nodeid: 1,
                offset: 0,
                size: 0,
            },
            flags: 0,
        };
        let fd = table.alloc(desc);
        if let Some(d) = table.get_mut(fd)
            && let FdKind::File { ref mut offset, .. } = d.kind
        {
            *offset = 42;
        }
        let d = table.get(fd).unwrap();
        if let FdKind::File { offset, .. } = d.kind {
            assert_eq!(offset, 42);
        } else {
            panic!("expected File");
        }
    }

    #[test]
    fn count_socket_and_epoll_refs_track_duplicate_descriptors() {
        let mut table = make_table();
        let socket_desc = FileDescriptor {
            kind: FdKind::Socket { id: 7 },
            flags: 0,
        };
        let epoll_desc = FileDescriptor {
            kind: FdKind::Epoll { id: 3 },
            flags: 0,
        };

        let s0 = table.alloc(socket_desc);
        let s1 = table.alloc(socket_desc);
        let e0 = table.alloc(epoll_desc);
        let e1 = table.alloc(epoll_desc);

        assert_eq!(table.count_socket_refs(7), 2);
        assert_eq!(table.count_epoll_refs(3), 2);

        table.free(s0);
        table.free(e0);

        assert_eq!(table.count_socket_refs(7), 1);
        assert_eq!(table.count_epoll_refs(3), 1);

        table.free(s1);
        table.free(e1);

        assert_eq!(table.count_socket_refs(7), 0);
        assert_eq!(table.count_epoll_refs(3), 0);
    }
}
