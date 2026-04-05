use std::fs;
use std::io::Write;

fn main() {
    println!("Hello from std Rust!");

    // Heap allocation + formatting
    let v: Vec<i32> = (1..=10).collect();
    println!("Vec: {:?}", v);
    println!("Sum: {}", v.iter().sum::<i32>());

    // Fibonacci
    let fib = |n: u32| -> u64 {
        let (mut a, mut b) = (0u64, 1u64);
        for _ in 0..n {
            let t = a + b;
            a = b;
            b = t;
        }
        a
    };
    println!("fib(50) = {}", fib(50));

    // File I/O
    let mut f = fs::File::create("/output.txt").expect("create file");
    writeln!(f, "Written from std Rust in sumi!").expect("write file");
    writeln!(f, "fib(30) = {}", fib(30)).expect("write file");
    drop(f);

    let contents = fs::read_to_string("/output.txt").expect("read file");
    print!("File contents:\n{}", contents);

    let args: Vec<String> = std::env::args().collect();
    println!("args = {:?}", args);

    println!("All done!");
}
