use super::*;
use sumi_abi::arch::layout::{DAX_SLOT_COUNT, PAGE_SIZE};

#[test]
fn dax_alloc_single() {
    let mut dax = DaxAllocator::new();
    let offset0 = dax.alloc(1).expect("first alloc should succeed");
    assert_eq!(offset0, 0, "first slot should be at offset 0");

    let offset1 = dax.alloc(1).expect("second alloc should succeed");
    assert_eq!(
        offset1, PAGE_SIZE,
        "second slot should be at offset PAGE_SIZE"
    );
}

#[test]
fn dax_alloc_contiguous() {
    let mut dax = DaxAllocator::new();
    let count = 8;
    let offset = dax.alloc(count).expect("contiguous alloc should succeed");
    assert_eq!(offset, 0, "contiguous block should start at offset 0");

    // Next single alloc must start immediately after the block.
    let next = dax.alloc(1).expect("alloc after block should succeed");
    assert_eq!(next, count * PAGE_SIZE, "next slot should follow the block");
}

#[test]
fn dax_alloc_and_free_reuses_offset() {
    let mut dax = DaxAllocator::new();
    let offset = dax.alloc(1).expect("alloc should succeed");
    dax.free(offset, 1);
    let reused = dax.alloc(1).expect("re-alloc after free should succeed");
    assert_eq!(reused, offset, "freed offset must be reused on next alloc");
}

#[test]
fn dax_alloc_rejects_invalid_or_exhausted_requests() {
    let mut dax = DaxAllocator::new();
    assert_eq!(dax.alloc(0), Err(DaxError::InvalidSlotCount));
    assert_eq!(
        dax.alloc(DAX_SLOT_COUNT + 1),
        Err(DaxError::WindowExhausted)
    );
    assert_eq!(dax.alloc(DAX_SLOT_COUNT), Ok(0));
    assert_eq!(dax.alloc(1), Err(DaxError::WindowExhausted));
}

#[test]
fn dax_free_and_reuse_middle_block() {
    let mut dax = DaxAllocator::new();
    dax.alloc(1).unwrap(); // slot 0
    let b = dax.alloc(1).unwrap(); // slot 1
    dax.alloc(1).unwrap(); // slot 2

    dax.free(b, 1);

    let reused = dax
        .alloc(1)
        .expect("alloc after middle free should succeed");
    assert_eq!(reused, b, "middle slot must be reused");
}

#[test]
fn dax_free_out_of_bounds_does_not_panic() {
    let mut dax = DaxAllocator::new();
    dax.free(DAX_SLOT_COUNT * PAGE_SIZE, 1);
}

#[test]
fn dax_alloc_crosses_u64_word_boundary() {
    let mut dax = DaxAllocator::new();
    dax.alloc(63).expect("filling 63 slots should succeed");

    let offset = dax
        .alloc(2)
        .expect("cross-word-boundary alloc should succeed");
    assert_eq!(
        offset,
        63 * PAGE_SIZE,
        "contiguous run should start at slot 63"
    );

    dax.free(offset, 2);
    let reused = dax.alloc(2).expect("re-alloc should succeed");
    assert_eq!(reused, offset, "freed cross-boundary slots must be reused");
}
