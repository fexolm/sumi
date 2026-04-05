pub const BOOT_INFO_MAGIC: u32 = 0x5355_4D49; // "SUMI"
pub const BOOT_INFO_VERSION: u32 = 1;
pub const BOOT_INFO_FLAG_HAS_RUN_PATH: u32 = 1 << 0;

/// Boot-time parameters written by the host, read by the guest.
/// Placed at BOOT_INFO_ADDR in guest physical memory.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BootInfo {
    pub magic: u32,
    pub version: u32,
    pub flags: u32,
    pub _reserved: u32,
    pub mem_size: u64,
    pub run_path_offset: u32,
    pub run_path_len: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::offset_of;

    #[test]
    fn boot_info_size() {
        assert_eq!(core::mem::size_of::<BootInfo>(), 32);
    }

    #[test]
    fn boot_info_field_offsets() {
        // The host writes this struct at BOOT_INFO_ADDR and the guest reads it.
        // Offsets must never change without a matching host-side update.
        assert_eq!(offset_of!(BootInfo, magic), 0);
        assert_eq!(offset_of!(BootInfo, version), 4);
        assert_eq!(offset_of!(BootInfo, flags), 8);
        assert_eq!(offset_of!(BootInfo, _reserved), 12);
        assert_eq!(offset_of!(BootInfo, mem_size), 16);
        assert_eq!(offset_of!(BootInfo, run_path_offset), 24);
        assert_eq!(offset_of!(BootInfo, run_path_len), 28);
    }

    #[test]
    fn boot_info_magic_value() {
        // "SUMI" encoded as little-endian u32: 'S'=0x53, 'U'=0x55, 'M'=0x4D, 'I'=0x49
        assert_eq!(BOOT_INFO_MAGIC, 0x5355_4D49);
    }

    #[test]
    fn boot_info_version_is_one() {
        assert_eq!(BOOT_INFO_VERSION, 1);
    }

    #[test]
    fn boot_info_flag_has_run_path_is_bit_zero() {
        assert_eq!(BOOT_INFO_FLAG_HAS_RUN_PATH, 1);
        assert_eq!(BOOT_INFO_FLAG_HAS_RUN_PATH & (BOOT_INFO_FLAG_HAS_RUN_PATH - 1), 0,
            "flag must be a single bit");
    }
}
