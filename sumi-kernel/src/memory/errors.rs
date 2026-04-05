use core::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryError {
    VirtualToPhysical { addr: usize },
    InvalidPageCount { pages: usize },
    OutOfMemory,
    AllocationTooLarge { requested: usize, max: usize },
    UnknownAllocation { addr: usize },
    AlreadyMapped { addr: usize },
    NotMapped { addr: usize },
}

pub type Result<T> = core::result::Result<T, MemoryError>;

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::VirtualToPhysical { addr } => {
                write!(f, "failed to convert virtual address {addr:#x} to physical")
            }
            Self::InvalidPageCount { pages } => write!(f, "invalid page count: {pages}"),
            Self::OutOfMemory => write!(f, "out of memory"),
            Self::AllocationTooLarge { requested, max } => write!(
                f,
                "allocation too large: requested {requested} bytes, max {max} bytes"
            ),
            Self::UnknownAllocation { addr } => {
                write!(f, "unknown allocation at physical address {addr:#x}")
            }
            Self::AlreadyMapped { addr } => {
                write!(f, "virtual address {addr:#x} is already mapped")
            }
            Self::NotMapped { addr } => {
                write!(f, "virtual address {addr:#x} is not mapped")
            }
        }
    }
}
