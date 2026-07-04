// Exercises std::sync::mpsc: multiple producer threads send to a single
// consumer over one channel; verify every message is received exactly
// once and the sum of payloads matches what was sent.

use std::sync::mpsc;
use std::thread;

const PRODUCERS: u64 = 4;
const MESSAGES_PER_PRODUCER: u64 = 1_000;

fn main() {
    let (tx, rx) = mpsc::channel::<u64>();

    let handles: Vec<_> = (0..PRODUCERS)
        .map(|p| {
            let tx = tx.clone();
            thread::spawn(move || {
                for i in 0..MESSAGES_PER_PRODUCER {
                    tx.send(p * MESSAGES_PER_PRODUCER + i).expect("send failed");
                }
            })
        })
        .collect();
    // Drop the original sender so `rx` iteration ends once all producer
    // clones are dropped, rather than blocking forever.
    drop(tx);

    let mut received_count: u64 = 0;
    let mut sum: u64 = 0;
    for msg in rx {
        received_count += 1;
        sum += msg;
    }

    for h in handles {
        h.join().expect("producer thread panicked");
    }

    let expected_count = PRODUCERS * MESSAGES_PER_PRODUCER;
    assert_eq!(received_count, expected_count);

    let expected_sum: u64 = (0..expected_count).sum();
    assert_eq!(sum, expected_sum);
    println!("mpsc_channel ok: received={received_count} sum={sum}");
}
