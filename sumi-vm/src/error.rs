use crate::vm::Hypervisor;
use thiserror::Error;
use vm_memory::{GuestMemoryError, mmap::FromRangesError};

#[derive(Debug, Error)]
pub enum Error {
    #[cfg(target_os = "linux")]
    #[error("The KVM backend reported an error: {0}")]
    Kvm(#[from] kvm_ioctls::Error),

    #[error("Cannot initialize {0} on the current system")]
    MissingHypervisor(Hypervisor),

    #[error("Failed to map guest memory: {0}")]
    GuestMemoryMmap(#[from] FromRangesError),

    #[error("guest memory error: {0}")]
    GuestMemory(#[from] GuestMemoryError),

    #[error("Invalid VM config: {0}")]
    InvalidVmConfig(String),

    #[error("unexpected vCPU exit: {0}")]
    UnexpectedExit(String),
}

pub type Result<T> = std::result::Result<T, Error>;
