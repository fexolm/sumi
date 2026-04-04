#![cfg_attr(not(test), no_std)]

pub mod arch;
pub mod drivers;
pub mod fs;
pub mod kernel;
pub mod memory;
pub mod selftest;
pub mod syscall;

pub use crate::kernel::{Kernel, KernelState};

// Global kernel state accessible from syscall handlers
pub static FD_TABLE: spin::Mutex<fs::FdTable> = spin::Mutex::new(fs::FdTable::new());
pub static VIRTIO_FS: spin::Once<fs::virtio_fs::VirtioFsClient> = spin::Once::new();
