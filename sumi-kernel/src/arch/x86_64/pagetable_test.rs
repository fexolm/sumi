use super::*;
use crate::memory::alloc::kmalloc::KernelAllocator;
use crate::memory::test_utils::{TestDirectMap, make_alloc};

/// Allocates a PML4 page through `kalloc` and wraps it in a `RootPageTable`.
/// The PML4 is kalloc-tracked, so Drop can free it correctly.
fn make_page_table<'a>(
    kalloc: &'a KernelAllocator<'a, TestDirectMap>,
) -> RootPageTable<'a, TestDirectMap> {
    let pml4_addr = kalloc.calloc(PAGE_TABLE_SIZE).expect("alloc PML4");
    // SAFETY: pml4_addr is zeroed memory allocated by kalloc and valid for the
    // lifetime of kalloc. It is tracked in the allocator so Drop can free it.
    unsafe { RootPageTable::from_paddr(pml4_addr, kalloc) }
}

/// Allocates a "data page" through kalloc so that the page-table Drop can
/// free the PD entry.
fn alloc_data_page<'a>(kalloc: &'a KernelAllocator<'a, TestDirectMap>) -> PhysicalAddr {
    kalloc.calloc(PAGE_TABLE_SIZE).expect("alloc data page")
}

// A 2 MB-aligned virtual address well within user space (PML4 index 0).
const VADDR_A: VirtualAddr = VirtualAddr::new(0x0000_0000_0020_0000); // 2 MB
const VADDR_B: VirtualAddr = VirtualAddr::new(0x0000_0000_0040_0000); // 4 MB — same PDPT/PD slot column, different PD entry
const VADDR_C: VirtualAddr = VirtualAddr::new(0x0000_0040_0000_0000); // different PDPT entry

#[test]
fn map_2mb_succeeds() {
    let (_dm, _pa, kalloc) = make_alloc(16);
    let pt = make_page_table(&kalloc);
    let pdata = alloc_data_page(&kalloc);

    pt.map_2mb(VADDR_A, pdata).expect("map should succeed");

    let entry = pt.get_if_present(VADDR_A).expect("get_if_present");
    assert!(entry.is_some(), "mapping should be present after map_2mb");
    assert_eq!(
        entry.unwrap().addr(),
        pdata,
        "entry should point to the mapped physical address"
    );
}

#[test]
fn double_map_returns_already_mapped() {
    let (_dm, _pa, kalloc) = make_alloc(16);
    let pt = make_page_table(&kalloc);
    let pdata = alloc_data_page(&kalloc);

    pt.map_2mb(VADDR_A, pdata)
        .expect("first map should succeed");
    let result = pt.map_2mb(VADDR_A, pdata);
    assert!(
        matches!(result, Err(MemoryError::AlreadyMapped { addr }) if addr == VADDR_A.as_usize()),
        "second map to the same vaddr must return AlreadyMapped, got {result:?}"
    );
}

#[test]
fn unmap_2mb_returns_correct_physical_address() {
    let (_dm, _pa, kalloc) = make_alloc(16);
    let pt = make_page_table(&kalloc);
    let pdata = alloc_data_page(&kalloc);

    pt.map_2mb(VADDR_A, pdata).expect("map should succeed");
    let returned = pt.unmap_2mb(VADDR_A).expect("unmap should succeed");
    assert_eq!(
        returned, pdata,
        "unmap_2mb should return the physical address that was mapped"
    );
    // Free the data page ourselves — unmap_2mb does not free it.
    kalloc.free(pdata);
}

#[test]
fn unmap_unmapped_address_returns_not_mapped() {
    let (_dm, _pa, kalloc) = make_alloc(16);
    let pt = make_page_table(&kalloc);

    let result = pt.unmap_2mb(VADDR_A);
    assert!(
        matches!(result, Err(MemoryError::NotMapped { addr }) if addr == VADDR_A.as_usize()),
        "unmap of never-mapped address must return NotMapped, got {result:?}"
    );
}

#[test]
fn unmap_after_unmap_returns_not_mapped() {
    let (_dm, _pa, kalloc) = make_alloc(16);
    let pt = make_page_table(&kalloc);
    let pdata = alloc_data_page(&kalloc);

    pt.map_2mb(VADDR_A, pdata).unwrap();
    let first = pt.unmap_2mb(VADDR_A).unwrap();
    kalloc.free(first);

    let result = pt.unmap_2mb(VADDR_A);
    assert!(
        matches!(result, Err(MemoryError::NotMapped { .. })),
        "second unmap of the same address must return NotMapped"
    );
}

#[test]
fn map_multiple_distinct_addresses() {
    let (_dm, _pa, kalloc) = make_alloc(32);
    let pt = make_page_table(&kalloc);
    let pdata_a = alloc_data_page(&kalloc);
    let pdata_b = alloc_data_page(&kalloc);
    let pdata_c = alloc_data_page(&kalloc);

    pt.map_2mb(VADDR_A, pdata_a).expect("map A");
    pt.map_2mb(VADDR_B, pdata_b).expect("map B");
    pt.map_2mb(VADDR_C, pdata_c).expect("map C");

    assert_eq!(
        pt.get_if_present(VADDR_A).unwrap().unwrap().addr(),
        pdata_a,
        "VADDR_A should map to pdata_a"
    );
    assert_eq!(
        pt.get_if_present(VADDR_B).unwrap().unwrap().addr(),
        pdata_b,
        "VADDR_B should map to pdata_b"
    );
    assert_eq!(
        pt.get_if_present(VADDR_C).unwrap().unwrap().addr(),
        pdata_c,
        "VADDR_C should map to pdata_c"
    );
}

#[test]
fn map_unmap_remap_succeeds() {
    let (_dm, _pa, kalloc) = make_alloc(16);
    let pt = make_page_table(&kalloc);
    let pdata1 = alloc_data_page(&kalloc);
    let pdata2 = alloc_data_page(&kalloc);

    pt.map_2mb(VADDR_A, pdata1).expect("first map");

    let returned = pt.unmap_2mb(VADDR_A).expect("unmap");
    assert_eq!(returned, pdata1);
    kalloc.free(returned);

    pt.map_2mb(VADDR_A, pdata2)
        .expect("remap after unmap should succeed");
    assert_eq!(
        pt.get_if_present(VADDR_A).unwrap().unwrap().addr(),
        pdata2,
        "remap should install the new physical address"
    );
}

#[test]
fn get_if_present_returns_none_for_unmapped() {
    let (_dm, _pa, kalloc) = make_alloc(16);
    let pt = make_page_table(&kalloc);

    let result = pt.get_if_present(VADDR_A).expect("get_if_present");
    assert!(
        result.is_none(),
        "get_if_present on never-mapped address must return None"
    );
}

#[test]
fn independent_addresses_do_not_alias() {
    let (_dm, _pa, kalloc) = make_alloc(32);
    let pt = make_page_table(&kalloc);
    let pdata_a = alloc_data_page(&kalloc);
    let pdata_b = alloc_data_page(&kalloc);

    pt.map_2mb(VADDR_A, pdata_a).expect("map A");
    pt.map_2mb(VADDR_B, pdata_b).expect("map B");

    let returned_a = pt.unmap_2mb(VADDR_A).expect("unmap A");
    kalloc.free(returned_a);

    // B must still be intact after A is unmapped.
    assert_eq!(
        pt.get_if_present(VADDR_B).unwrap().unwrap().addr(),
        pdata_b,
        "unmapping VADDR_A must not affect VADDR_B"
    );
    assert!(
        pt.get_if_present(VADDR_A).unwrap().is_none(),
        "VADDR_A must be absent after unmap"
    );
}

#[test]
fn clear_present_hides_mapping() {
    let (_dm, _pa, kalloc) = make_alloc(16);
    let pt = make_page_table(&kalloc);
    let pdata = alloc_data_page(&kalloc);

    pt.map_2mb(VADDR_A, pdata).expect("map");
    pt.clear_present_2mb(VADDR_A).expect("clear_present");

    // get_if_present walks only PRESENT entries, so it returns None now.
    let result = pt.get_if_present(VADDR_A).expect("get_if_present");
    assert!(result.is_none(), "cleared entry must not appear as present");
}

#[test]
fn restore_present_reveals_mapping() {
    let (_dm, _pa, kalloc) = make_alloc(16);
    let pt = make_page_table(&kalloc);
    let pdata = alloc_data_page(&kalloc);

    pt.map_2mb(VADDR_A, pdata).expect("map");
    pt.clear_present_2mb(VADDR_A).expect("clear_present");
    pt.restore_present_2mb(VADDR_A).expect("restore_present");

    let entry = pt.get_if_present(VADDR_A).expect("get_if_present");
    assert!(entry.is_some(), "restored entry must be present again");
    assert_eq!(
        entry.unwrap().addr(),
        pdata,
        "physical address must be preserved through clear/restore"
    );
}

#[test]
fn clear_present_on_unmapped_returns_not_mapped() {
    let (_dm, _pa, kalloc) = make_alloc(16);
    let pt = make_page_table(&kalloc);

    let result = pt.clear_present_2mb(VADDR_A);
    assert!(
        matches!(result, Err(MemoryError::NotMapped { addr }) if addr == VADDR_A.as_usize()),
        "clear_present on never-mapped address must return NotMapped, got {result:?}"
    );
}

#[test]
fn restore_present_on_never_mapped_returns_not_mapped() {
    let (_dm, _pa, kalloc) = make_alloc(16);
    let pt = make_page_table(&kalloc);

    // No map at all — the PML4/PDPT entries don't exist either.
    let result = pt.restore_present_2mb(VADDR_A);
    assert!(
        matches!(result, Err(MemoryError::NotMapped { .. })),
        "restore_present on never-mapped address must return NotMapped, got {result:?}"
    );
}

#[test]
fn restore_present_on_empty_pd_slot_returns_not_mapped() {
    let (_dm, _pa, kalloc) = make_alloc(16);
    let pt = make_page_table(&kalloc);
    let pdata = alloc_data_page(&kalloc);

    // Map A, which creates the PML4/PDPT/PD tables shared with B. B's PD
    // slot is therefore reachable (intermediate tables exist) but empty —
    // exactly the guard-page / partial-VMA case. restore_present must NOT
    // set PRESENT on the zero slot (that would forge a non-huge PDE at
    // physical address 0); it must report NotMapped.
    pt.map_2mb(VADDR_A, pdata).expect("map A");

    let result = pt.restore_present_2mb(VADDR_B);
    assert!(
        matches!(result, Err(MemoryError::NotMapped { addr }) if addr == VADDR_B.as_usize()),
        "restore_present on an empty PD slot must return NotMapped, got {result:?}"
    );
    // The empty slot must remain empty — not a forged present entry.
    assert!(
        pt.get_if_present(VADDR_B).expect("walk B").is_none(),
        "empty PD slot must stay absent after a failed restore"
    );
}

#[test]
fn raw_returns_entry_bits() {
    let (_dm, _pa, kalloc) = make_alloc(16);
    let pt = make_page_table(&kalloc);
    let pdata = alloc_data_page(&kalloc);

    pt.map_2mb(VADDR_A, pdata).expect("map");
    let entry = pt.get_if_present(VADDR_A).expect("get").unwrap();
    // The raw value must have PRESENT (bit 0), WRITABLE (bit 1), USER (bit 2),
    // HUGE_PAGE (bit 7) set, plus the physical address.
    let raw = entry.raw();
    assert_eq!(raw & 1, 1, "PRESENT must be set");
    assert_eq!(raw & (1 << 7), 1 << 7, "HUGE_PAGE must be set");
    assert_eq!(entry.addr(), pdata, "addr extracted from raw matches");
}
