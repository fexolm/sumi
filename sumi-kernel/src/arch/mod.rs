#[cfg(target_arch = "x86_64")]
pub mod x86_64;

#[cfg(target_arch = "x86_64")]
pub use self::x86_64::{KernelDirectMap, RootPageTable, debugcon_write_byte, halt_forever};
#[cfg(target_arch = "x86_64")]
#[cfg(not(test))]
pub use self::x86_64::syscall;
