// Exercises std::sync::{Arc, Mutex} — 8 threads each incrementing a
// shared counter 50_000 times under a mutex. Final value must be exact,
// proving the futex-backed mutex serializes access correctly.

use std::sync::{Arc, Mutex};
use std::thread;

const THREADS: u64 = 8;
const INCREMENTS: u64 = 50_000;

fn main() {
    let counter = Arc::new(Mutex::new(0u64));

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let counter = Arc::clone(&counter);
            thread::spawn(move || {
                for _ in 0..INCREMENTS {
                    *counter.lock().expect("lock poisoned") += 1;
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked");
    }

    let total = *counter.lock().expect("lock poisoned");
    assert_eq!(total, THREADS * INCREMENTS);
    println!("mutex_arc ok: total={total}");
}
