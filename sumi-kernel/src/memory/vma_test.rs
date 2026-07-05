use super::*;
use sumi_abi::{address::VirtualAddr, arch::layout::PAGE_SIZE};

fn make_vma(start: usize, end: usize) -> Vma {
    Vma {
        start: VirtualAddr::new(start),
        end: VirtualAddr::new(end),
        backing: MappingBacking::Anonymous,
    }
}

#[test]
fn vma_insert_and_find() {
    let mut table = VmaTable::new();
    let start = 0x1000_0000;
    let end = start + PAGE_SIZE;
    table.insert(make_vma(start, end));

    let found = table.find(VirtualAddr::new(start));
    assert!(found.is_some(), "find at vma.start should return the VMA");
    assert_eq!(found.unwrap().start.as_usize(), start);

    let mid = table.find(VirtualAddr::new(start + PAGE_SIZE / 2));
    assert!(
        mid.is_some(),
        "find in the middle of the VMA should return it"
    );
}

#[test]
fn vma_find_returns_none_outside_range() {
    let mut table = VmaTable::new();
    let start = 0x1000_0000;
    let end = start + PAGE_SIZE;
    table.insert(make_vma(start, end));

    assert!(
        table.find(VirtualAddr::new(start - 1)).is_none(),
        "address before VMA must not be found"
    );
    assert!(
        table.find(VirtualAddr::new(end + PAGE_SIZE)).is_none(),
        "address after VMA must not be found"
    );
}

#[test]
fn vma_find_adjacent_not_contained() {
    let mut table = VmaTable::new();
    let start = 0x2000_0000;
    let end = start + PAGE_SIZE;
    table.insert(make_vma(start, end));

    assert!(
        table.find(VirtualAddr::new(end)).is_none(),
        "address at exclusive end must not be found"
    );
}

#[test]
fn vma_remove_by_start() {
    let mut table = VmaTable::new();
    let start = 0x3000_0000;
    let end = start + PAGE_SIZE;
    table.insert(make_vma(start, end));

    let removed = table.remove(VirtualAddr::new(start));
    assert!(
        removed.is_some(),
        "remove by start address should return the VMA"
    );
    assert_eq!(removed.unwrap().start.as_usize(), start);

    assert!(
        table.find(VirtualAddr::new(start)).is_none(),
        "VMA must not be findable after removal"
    );
}

#[test]
fn vma_remove_nonexistent() {
    let mut table = VmaTable::new();
    let result = table.remove(VirtualAddr::new(0xDEAD_0000));
    assert!(
        result.is_none(),
        "removing non-existent VMA must return None"
    );
}

#[test]
fn vma_remove_overlapping_single() {
    let mut table = VmaTable::new();
    let start = 0x4000_0000;
    let end = start + PAGE_SIZE;
    table.insert(make_vma(start, end));

    let removed = table.remove_overlapping(VirtualAddr::new(start), VirtualAddr::new(end));
    assert_eq!(
        removed.len(),
        1,
        "exactly one overlapping VMA must be removed"
    );

    assert!(
        table.find(VirtualAddr::new(start)).is_none(),
        "VMA must not be findable after remove_overlapping"
    );
}

#[test]
fn vma_remove_overlapping_none() {
    let mut table = VmaTable::new();
    let start = 0x5000_0000;
    let end = start + PAGE_SIZE;
    table.insert(make_vma(start, end));

    let removed =
        table.remove_overlapping(VirtualAddr::new(end), VirtualAddr::new(end + PAGE_SIZE));
    assert!(
        removed.is_empty(),
        "remove_overlapping with non-overlapping range must return nothing"
    );

    assert!(
        table.find(VirtualAddr::new(start)).is_some(),
        "VMA must still be present when remove_overlapping found nothing"
    );
}

#[test]
fn vma_remove_overlapping_multiple() {
    let mut table = VmaTable::new();
    let a_start = 0x6000_0000;
    let a_end = a_start + PAGE_SIZE;
    let b_start = a_end;
    let b_end = b_start + PAGE_SIZE;
    table.insert(make_vma(a_start, a_end));
    table.insert(make_vma(b_start, b_end));

    let removed = table.remove_overlapping(VirtualAddr::new(a_start), VirtualAddr::new(b_end));
    assert_eq!(removed.len(), 2, "both overlapping VMAs must be removed");
}

#[test]
fn vma_remove_overlapping_partial_overlap() {
    let mut table = VmaTable::new();
    let start = 0x7000_0000;
    let end = start + 2 * PAGE_SIZE;
    table.insert(make_vma(start, end));

    let removed = table.remove_overlapping(
        VirtualAddr::new(start + PAGE_SIZE),
        VirtualAddr::new(end + PAGE_SIZE),
    );
    assert_eq!(
        removed.len(),
        1,
        "partially overlapping VMA must be removed"
    );

    assert!(
        table.find(VirtualAddr::new(start)).is_none(),
        "removed VMA must not be findable"
    );
}

#[test]
fn vma_find_free_downward_reuses_highest_hole() {
    let mut table = VmaTable::new();
    let high = 0x9000_0000;
    let len = PAGE_SIZE;

    table.insert(make_vma(high - len, high));
    table.insert(make_vma(high - 3 * len, high - 2 * len));

    let found = table
        .find_free_downward(VirtualAddr::new(high), len)
        .expect("free hole should exist");
    assert_eq!(
        found.as_usize(),
        high - 2 * len,
        "allocator should reuse the highest hole below high"
    );
}

#[test]
fn vma_find_free_downward_returns_top_when_empty() {
    let table = VmaTable::new();
    let high = 0xA000_0000;
    let len = 2 * PAGE_SIZE;

    let found = table
        .find_free_downward(VirtualAddr::new(high), len)
        .expect("empty table should have free space");
    assert_eq!(found.as_usize(), high - len);
}

#[test]
fn vma_find_free_downward_aligned_keeps_requested_alignment() {
    let mut table = VmaTable::new();
    let high = 0x8000_0000usize;
    let len = PAGE_SIZE;
    table.insert(make_vma(high - 0x10000, high));

    let found = table
        .find_free_downward_aligned(VirtualAddr::new(high), len, PAGE_SIZE)
        .expect("aligned hole below unaligned VMA start");

    assert_eq!(found.as_usize() % PAGE_SIZE, 0);
    assert!(found.as_usize() + len <= high - 0x10000);
}
