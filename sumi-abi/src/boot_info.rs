pub const BOOT_INFO_MAGIC: u32 = 0x5355_4D49; // "SUMI"
pub const BOOT_INFO_VERSION: u32 = 3;
pub const BOOT_INFO_FLAG_HAS_RUN_PATH: u32 = 1 << 0;

/// Boot-time parameters written by the host, read by the guest.
/// Placed at BOOT_INFO_ADDR in guest physical memory.
///
/// Version history:
///   1: initial layout.
///   2: tsc_freq_khz / wall_clock / rng_seed.
///   3: num_cpus.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BootInfo {
    pub magic: u32,           // offset 0
    pub version: u32,         // offset 4
    pub flags: u32,           // offset 8
    pub _reserved: u32,       // offset 12
    pub mem_size: u64,        // offset 16
    pub run_path_offset: u32, // offset 24
    pub run_path_len: u32,    // offset 28
    // v2 fields
    pub tsc_freq_khz: u32,    // offset 32
    pub wall_clock_nsec: u32, // offset 36
    pub wall_clock_sec: u64,  // offset 40
    pub rng_seed: [u8; 32],   // offset 48
    // v3 fields
    /// Total number of vCPUs created by the host. Always in 1..=MAX_VCPUS.
    pub num_cpus: u32,        // offset 80
    pub _pad1: u32,           // offset 84 — keep 8-byte alignment
}
