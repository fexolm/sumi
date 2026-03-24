use core::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryError {
    VirtualToPhysical { addr: usize },
    InvalidPageCount { pages: usize },
    OutOfMemory,
    AllocationTooLarge { requested: usize, max: usize },
    TooManySlabs { class_size: u32 },
    TooManyLargeAllocations,
    UnknownAllocation { addr: usize },
    SlabAlignmentMismatch { addr: usize, block_size: usize },
    InvalidSlabCapacity,
    SlabEmpty,
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
            Self::TooManySlabs { class_size } => {
                write!(f, "too many slabs for class {class_size}")
            }
            Self::TooManyLargeAllocations => write!(f, "too many active large allocations"),
            Self::UnknownAllocation { addr } => {
                write!(f, "unknown allocation at physical address {addr:#x}")
            }
            Self::SlabAlignmentMismatch { addr, block_size } => write!(
                f,
                "pointer {addr:#x} does not match slab alignment {block_size}"
            ),
            Self::InvalidSlabCapacity => write!(f, "invalid slab capacity"),
            Self::SlabEmpty => write!(f, "slab is empty"),
        }
    }
}
