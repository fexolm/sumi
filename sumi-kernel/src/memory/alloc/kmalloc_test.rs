use super::*;
use crate::memory::test_utils::{TestDirectMap, make_alloc};
use std::{collections::HashSet, sync::Arc};

#[test]
fn small_alloc_and_free_reuses_address() {
    let (_dm, _pa, alloc) = make_alloc(4);
    let a = alloc.alloc(64).unwrap();
    let b = alloc.alloc(64).unwrap();
    assert_ne!(a, b);

    alloc.free(a);
    let c = alloc.alloc(64).unwrap();
    assert_eq!(c, a);
}

#[test]
fn small_allocs_do_not_overlap() {
    let (_dm, _pa, alloc) = make_alloc(4);
    let a = alloc.alloc(4096).unwrap();
    let b = alloc.alloc(4096).unwrap();
    assert!(a.as_usize().abs_diff(b.as_usize()) >= 4096);
}

#[test]
fn calloc_zeroes_memory() {
    let (dm, _pa, alloc) = make_alloc(4);

    let ptr = alloc.alloc(128).unwrap();
    unsafe {
        *ptr.to_virtual(&*dm).as_ptr::<u64>() = 0xDEAD_BEEF_CAFE_BABE;
    }
    alloc.free(ptr);

    let zeroed = alloc.calloc(128).unwrap();
    let slice = unsafe { core::slice::from_raw_parts(zeroed.to_virtual(&*dm).as_ptr::<u8>(), 128) };
    assert!(slice.iter().all(|&byte| byte == 0));
}

#[test]
fn page_table_alloc_is_aligned() {
    let (_dm, _pa, alloc) = make_alloc(4);
    let ptr = alloc.alloc(PAGE_TABLE_SIZE).unwrap();
    assert_eq!(ptr.as_usize() % PAGE_TABLE_SIZE, 0);
}

#[test]
fn adjacent_frees_are_coalesced() {
    let (_dm, _pa, alloc) = make_alloc(4);
    let a = alloc.alloc(PAGE_TABLE_SIZE).unwrap();
    let b = alloc.alloc(PAGE_TABLE_SIZE).unwrap();
    assert_ne!(a, b);

    alloc.free(b);
    alloc.free(a);

    // After coalescing, a single allocation spanning both blocks must succeed.
    let big = alloc.alloc(PAGE_TABLE_SIZE * 2).unwrap();
    assert!(big.as_usize() > 0);
}

#[test]
fn large_allocs_dont_overlap() {
    let (_dm, _pa, alloc) = make_alloc(8);
    let a = alloc.alloc(1 << 22).unwrap();
    let b = alloc.alloc(1 << 22).unwrap();
    assert!(a.as_usize().abs_diff(b.as_usize()) >= (1 << 22));
}

#[test]
fn large_free_and_realloc_reuses_address() {
    let (_dm, _pa, alloc) = make_alloc(8);
    let a = alloc.alloc(1 << 22).unwrap();
    let b = alloc.alloc(1 << 22).unwrap();
    assert_ne!(a, b);

    alloc.free(b);
    let c = alloc.alloc(1 << 22).unwrap();
    assert_eq!(c, b);
}

#[test]
fn zero_size_alloc_succeeds() {
    let (_dm, _pa, alloc) = make_alloc(2);
    let ptr = alloc.alloc(0).unwrap();
    alloc.free(ptr);
}

#[test]
fn alloc_too_large_fails() {
    let (_dm, _pa, alloc) = make_alloc(8);
    assert!(matches!(
        alloc.alloc(MAX_ALLOC + 1),
        Err(MemoryError::AllocationTooLarge { .. })
    ));
}

#[test]
fn cross_thread_free_is_reused() {
    let (_dm, _pa, alloc) = make_alloc(4);
    let alloc: Arc<KernelAllocator<'static, TestDirectMap>> = Arc::from(alloc);

    let ptr = alloc.alloc(64).unwrap();

    let worker_alloc = Arc::clone(&alloc);
    let worker = std::thread::spawn(move || {
        worker_alloc.free(ptr);
    });
    worker.join().unwrap();

    let next = alloc.alloc(64).unwrap();
    assert_eq!(next, ptr);
}

#[test]
fn concurrent_live_allocations_are_unique() {
    const THREADS: usize = 32;
    const OPS: usize = 16;
    const SIZE: usize = 64;

    let (_dm, _pa, alloc) = make_alloc(4);
    let alloc: Arc<KernelAllocator<'static, TestDirectMap>> = Arc::from(alloc);

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let alloc = Arc::clone(&alloc);
            std::thread::spawn(move || {
                (0..OPS)
                    .map(|_| alloc.alloc(SIZE).unwrap().as_usize())
                    .collect::<Vec<_>>()
            })
        })
        .collect();

    let allocations: Vec<Vec<usize>> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    let mut seen = HashSet::new();
    for ptrs in &allocations {
        for &addr in ptrs {
            assert!(seen.insert(addr), "duplicate live allocation at {addr:#x}");
        }
    }

    for ptrs in allocations {
        for addr in ptrs {
            alloc.free(PhysicalAddr::new(addr));
        }
    }
}
