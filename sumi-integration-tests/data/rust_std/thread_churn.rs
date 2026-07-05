// Stress Rust std thread creation/destruction enough to exercise pthread
// stack mmap/munmap reuse. This is intentionally larger than the basic
// thread_spawn smoke test: Linux handles this pattern routinely, and sumi
// must not exhaust its mmap area just because stacks were unmapped.

use std::thread;
use std::{io, io::Write};

const STACK_BYTES: usize = 64 * 1024;
const SEQUENTIAL_THREADS: u64 = 2_048;
const CONCURRENT_THREADS: u64 = 64;

fn spawn_value(value: u64) -> thread::JoinHandle<u64> {
    thread::Builder::new()
        .stack_size(STACK_BYTES)
        .spawn(move || value.wrapping_mul(3).wrapping_add(1))
        .expect("spawn worker")
}

fn main() {
    println!("thread_churn start");
    io::stdout().flush().expect("flush start");

    let mut sequential_sum = 0u64;
    for i in 0..SEQUENTIAL_THREADS {
        if i > 0 && i % 512 == 0 {
            println!("thread_churn progress: sequential={i}");
            io::stdout().flush().expect("flush progress");
        }
        sequential_sum = sequential_sum.wrapping_add(spawn_value(i).join().expect("join worker"));
    }

    let handles: Vec<_> = (0..CONCURRENT_THREADS).map(spawn_value).collect();
    let mut concurrent_sum = 0u64;
    for handle in handles {
        concurrent_sum = concurrent_sum.wrapping_add(handle.join().expect("join concurrent worker"));
    }

    let expected_sequential = (0..SEQUENTIAL_THREADS)
        .map(|i| i.wrapping_mul(3).wrapping_add(1))
        .fold(0u64, u64::wrapping_add);
    let expected_concurrent = (0..CONCURRENT_THREADS)
        .map(|i| i.wrapping_mul(3).wrapping_add(1))
        .fold(0u64, u64::wrapping_add);

    assert_eq!(sequential_sum, expected_sequential);
    assert_eq!(concurrent_sum, expected_concurrent);
    println!("thread_churn ok: sequential={SEQUENTIAL_THREADS} concurrent={CONCURRENT_THREADS}");
}
