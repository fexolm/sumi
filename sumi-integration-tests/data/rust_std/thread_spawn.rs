// Exercises std::thread on top of sumi's clone/futex support: spawn 8
// threads, each computes a distinct value, join all, verify the sum.

use std::thread;

const N: u64 = 8;

fn main() {
    let handles: Vec<_> = (0..N)
        .map(|i| thread::spawn(move || i * i))
        .collect();

    let mut sum: u64 = 0;
    for h in handles {
        sum += h.join().expect("thread panicked");
    }

    let expected: u64 = (0..N).map(|i| i * i).sum();
    assert_eq!(sum, expected);
    println!("thread_spawn ok: sum={sum}");
}
