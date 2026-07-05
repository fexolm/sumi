// Stress a burst of fresh CPU-bound std threads without a pool. Workers are
// intentionally allowed to finish while the parent is still spawning later
// workers, so pthread stack munmap overlaps new stack mmap. That caught a
// lock-order deadlock in sumi's small anonymous mmap path.

use std::io::{self, Write};
use std::thread;

const STACK_BYTES: usize = 64 * 1024;
const THREADS: usize = 512;
const ROUNDS: usize = 16;
const WORK_ITERS: usize = 4096;

fn busy_work(round: usize, id: usize) -> u64 {
    let mut x = 0x9e37_79b9_7f4a_7c15u64 ^ ((round as u64) << 32) ^ id as u64;
    for i in 0..WORK_ITERS {
        x ^= (i as u64).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x = x.rotate_left(13).wrapping_mul(0x94d0_49bb_1331_11eb);
    }
    x
}

fn spawn_worker(round: usize, id: usize) -> thread::JoinHandle<u64> {
    thread::Builder::new()
        .stack_size(STACK_BYTES)
        .spawn(move || busy_work(round, id))
        .expect("spawn worker")
}

fn main() {
    println!("thread_burst start: rounds={ROUNDS} threads={THREADS}");
    io::stdout().flush().expect("flush start");

    let expected_one_round = |round| {
        (0..THREADS)
            .map(|id| busy_work(round, id))
            .fold(0u64, u64::wrapping_add)
    };

    let mut total = 0u64;
    let mut expected_total = 0u64;
    for round in 0..ROUNDS {
        if round > 0 && round % 4 == 0 {
            println!("thread_burst progress: round={round}");
            io::stdout().flush().expect("flush progress");
        }

        let handles: Vec<_> = (0..THREADS).map(|id| spawn_worker(round, id)).collect();
        let mut round_sum = 0u64;
        for handle in handles {
            round_sum = round_sum.wrapping_add(handle.join().expect("join worker"));
        }

        let expected = expected_one_round(round);
        assert_eq!(round_sum, expected);
        total = total.wrapping_add(round_sum);
        expected_total = expected_total.wrapping_add(expected);
    }

    assert_eq!(total, expected_total);
    println!("thread_burst ok: rounds={ROUNDS} threads={THREADS}");
}
