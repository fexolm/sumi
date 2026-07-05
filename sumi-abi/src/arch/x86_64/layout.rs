use crate::{
    address::{PhysicalAddr, VirtualAddr},
    arch::x86_64::address::get_pml4_index,
    layout::{KERNEL_CODE_PHYS, KERNEL_CODE_SIZE, MAX_PHYSICAL_ADDR},
};

pub const BASE_PAGE_SIZE: usize = 0x1000;
pub const PAGE_SIZE: usize = 2 << 20;
pub const HUGE_PAGE_SIZE_1G: usize = 1 << 30;

pub const PAGE_TABLE_ENTRIES: usize = 512;
pub const PAGE_TABLE_SIZE: usize = 8 * PAGE_TABLE_ENTRIES;

pub const DIRECT_MAP_OFFSET: VirtualAddr = VirtualAddr::new(0xFFFF_8880_0000_0000);

pub const DIRECT_MAP_PML4: PhysicalAddr = KERNEL_CODE_PHYS.add(KERNEL_CODE_SIZE);

pub const DIRECT_MAP_PML4_OFFSET: usize = get_pml4_index(DIRECT_MAP_OFFSET);
// Each PML4 entry covers 512 PDPT entries * 1GB = 512GB
pub const DIRECT_MAP_PML4_ENTRIES_COUNT: usize =
    (MAX_PHYSICAL_ADDR + 1).div_ceil(HUGE_PAGE_SIZE_1G * PAGE_TABLE_ENTRIES);

// Direct map uses 1GB huge pages at PDPT level (no PD tables needed).
// One PDPT table per PML4 entry.
pub const DIRECT_MAP_PDPT: PhysicalAddr = DIRECT_MAP_PML4.add(PAGE_TABLE_SIZE);
pub const DIRECT_MAP_PDPT_COUNT: usize = DIRECT_MAP_PML4_ENTRIES_COUNT;

// pdpd and pd for the kernel code (we need to reserve 2gb of virtual address space for kernel code, for code-model=kernel)
pub const KERNEL_CODE_PDPD: PhysicalAddr =
    DIRECT_MAP_PDPT.add(DIRECT_MAP_PDPT_COUNT * PAGE_TABLE_SIZE);
pub const KERNEL_CODE_PD: PhysicalAddr = KERNEL_CODE_PDPD.add(PAGE_TABLE_SIZE);

pub const KERNEL_STACK_SIZE: usize = 0x1000 * 8; // 32KB stack
pub const KERNEL_STACK: PhysicalAddr = KERNEL_CODE_PD
    .add(PAGE_TABLE_SIZE + KERNEL_STACK_SIZE)
    .align_up(PAGE_SIZE);

/// Boot info location: just past the kernel's 4 MB physical region.
pub const BOOT_INFO_ADDR: PhysicalAddr = PhysicalAddr::new(0x40_0000);
/// Maximum boot info region size (one 4 KB page).
pub const BOOT_INFO_MAX_SIZE: usize = 0x1000;

/// Default stack top for the user program (page-aligned, within lower half).
pub const USER_STACK_TOP: VirtualAddr = VirtualAddr::new(0x0000_7FFF_FFFF_F000);
/// User stack size: 8 MB = 4 x 2 MB pages.
pub const USER_STACK_SIZE: usize = 8 * 1024 * 1024;
/// Base address for mmap region (grows downward).
pub const USER_MMAP_BASE: VirtualAddr = VirtualAddr::new(0x0000_7FFF_0000_0000);

/// Base address for loading PIE (ET_DYN) main binaries.
/// Linux default for non-ASLR PIE on x86_64.
pub const PIE_LOAD_BASE: u64 = 0x0040_0000;

/// Base address for loading the dynamic linker (interpreter).
/// Placed high in user space, well below the mmap region.
pub const INTERP_LOAD_BASE: u64 = 0x7f00_0000_0000;

/// Physical base address of the local APIC MMIO page (xAPIC mode).
///
/// KVM's in-kernel LAPIC intercepts guest accesses to this page only when
/// no KVM memslot backs it with real RAM. `sumi-vm` must never register a
/// memslot covering this address, and
/// `PageAllocator` must never hand out the 2 MiB frame containing it —
/// both sides reference this one constant to stay in sync. It happens to
/// be exactly 2 MiB-aligned (0xFEE0_0000 == 2039 * PAGE_SIZE), so the
/// "containing frame" is `[LAPIC_BASE_PHYS, LAPIC_BASE_PHYS + PAGE_SIZE)`.
pub const LAPIC_BASE_PHYS: PhysicalAddr = PhysicalAddr::new(0xFEE0_0000);

const _: () = {
    assert!(
        LAPIC_BASE_PHYS.as_usize().is_multiple_of(PAGE_SIZE),
        "LAPIC_BASE_PHYS must be 2 MiB-aligned for the single-frame reservation in palloc",
    );
};

/// Base physical address for virtio MMIO devices.
/// Placed at 64 GB — above any reasonable guest RAM (2 TB max),
/// within the direct-map range so the kernel can access it, and
/// outside the KVM memslot so accesses trigger MMIO exits.
pub const VIRTIO_MMIO_BASE: PhysicalAddr = PhysicalAddr::new(0x10_0000_0000);

/// Each virtio device occupies 4KB (one MMIO page).
pub const VIRTIO_MMIO_STRIDE: usize = 0x1000;

/// Device 0 = virtio-fs.
pub const VIRTIO_FS_MMIO: PhysicalAddr = VIRTIO_MMIO_BASE;

/// Device 1 = virtio-console.
pub const VIRTIO_CONSOLE_MMIO: PhysicalAddr =
    PhysicalAddr::new(VIRTIO_MMIO_BASE.as_u64() as usize + VIRTIO_MMIO_STRIDE);

/// Device 2 = virtio-net.
pub const VIRTIO_NET_MMIO: PhysicalAddr =
    PhysicalAddr::new(VIRTIO_MMIO_BASE.as_u64() as usize + 2 * VIRTIO_MMIO_STRIDE);

/// Base physical address of the DAX shared memory window.
pub const DAX_WINDOW_BASE: PhysicalAddr = PhysicalAddr::new(0x20_0000_0000);

/// Total DAX window size (128 GB = 65536 x 2MB slots).
pub const DAX_WINDOW_SIZE: usize = 128 * 1024 * 1024 * 1024;

/// Number of 2MB slots in the DAX window.
pub const DAX_SLOT_COUNT: usize = DAX_WINDOW_SIZE / PAGE_SIZE;

/// Base physical address of the hypercall MMIO trap range.
///
/// Placed in the gap between guest RAM (which ends at
/// `KERNEL_CODE_SIZE + mem_size = 4 GiB` for the current 2 GiB
/// `mem_size`) and the virtio MMIO band (`VIRTIO_MMIO_BASE = 64 GiB`).
/// Sits at 4 GiB + 8 KiB so it is well clear of the slot-0 boundary
/// even if `mem_size` is bumped to anything below 64 GiB.
///
/// Reserves `sumi_abi::hypercall::HYPERCALL_MMIO_SIZE` (4 KiB) of
/// physical address space. The kernel writes to this region via the
/// direct map (`DIRECT_MAP_OFFSET + HYPERCALL_MMIO_BASE`); the host
/// has no KVM memslot covering it, so the write traps as
/// `VcpuExit::MmioWrite` and is decoded by the hypercall dispatcher.
///
/// See `sumi_abi::hypercall` for the offset / argument layout.
pub const HYPERCALL_MMIO_BASE: PhysicalAddr = PhysicalAddr::new(0x1_0000_2000);

const _: () = {
    // Compile-time invariant: HYPERCALL_MMIO_BASE must lie strictly
    // above the maximum guest RAM region (KERNEL_CODE_SIZE + max
    // expected mem_size). Today the max mem_size is 2 GiB (set by
    // sumi-vm/src/cmd/run.rs), so the slot-0 region ends at exactly
    // KERNEL_CODE_SIZE + 0x8000_0000 = 0x1_0000_0000. The hypercall
    // base is one page (4 KiB) further with an 8 KiB pad — adjust
    // upward if mem_size ever grows past 2 GiB.
    assert!(
        HYPERCALL_MMIO_BASE.as_u64() >= (KERNEL_CODE_SIZE as u64 + 0x8000_0000u64),
        "HYPERCALL_MMIO_BASE overlaps slot-0 guest RAM",
    );
    // Must also lie below the virtio MMIO band so the device
    // dispatcher's range check stays clean.
    assert!(
        HYPERCALL_MMIO_BASE.as_u64() + 0x1000 <= VIRTIO_MMIO_BASE.as_u64(),
        "HYPERCALL_MMIO_BASE overlaps virtio MMIO range",
    );
};
