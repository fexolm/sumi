// Rust std TCP echo client/server pair, entirely inside the guest.
//
// Unlike the raw-syscall test programs (data/syscalls/tcp_*.rs), this uses
// std::net + std::thread end to end, which exercises the *blocking* socket
// paths a real server binary (e.g. mysqld) relies on:
//   - socket() + setsockopt(SO_REUSEADDR) via TcpListener::bind
//   - blocking accept4(SOCK_CLOEXEC)
//   - blocking connect
//   - plain read()/write() on connected sockets (NOT recvfrom/sendto)
//   - getsockname (local_addr), shutdown, close
//   - a server std::thread concurrently blocked in accept while the client
//     thread connects — the kernel's net_wait block/wake path across threads.
//
// Server thread: bind 127.0.0.1:7777, accept one connection, echo until EOF.
// Client (main): connect, send several messages, verify each echo, close,
// then join the server. Exit 0 only if every byte round-tripped.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

const ADDR: &str = "127.0.0.1:7777";

fn server(listener: TcpListener) {
    // One connection is enough: echo until the client closes.
    let (mut stream, peer) = listener.accept().expect("accept");
    println!("[server] accepted connection from {peer}");
    let mut buf = [0u8; 1024];
    loop {
        let n = stream.read(&mut buf).expect("server read");
        if n == 0 {
            println!("[server] client closed, exiting");
            return;
        }
        stream.write_all(&buf[..n]).expect("server write");
    }
}

fn main() {
    let listener = TcpListener::bind(ADDR).expect("bind");
    println!("[server] listening on {}", listener.local_addr().expect("local_addr"));

    let server_thread = thread::spawn(move || server(listener));

    let mut client = TcpStream::connect(ADDR).expect("connect");
    println!("[client] connected from {}", client.local_addr().expect("local_addr"));

    for (i, msg) in [
        "hello from rust std inside sumi",
        "second message, a bit longer than the first one to vary sizes",
        "third",
    ]
    .iter()
    .enumerate()
    {
        client.write_all(msg.as_bytes()).expect("client write");
        let mut echoed = vec![0u8; msg.len()];
        client.read_exact(&mut echoed).expect("client read");
        assert_eq!(&echoed, msg.as_bytes(), "echo mismatch on message {i}");
        println!("[client] message {i} echoed intact ({} bytes)", msg.len());
    }

    drop(client); // EOF to the server
    server_thread.join().expect("server thread panicked");
    println!("[client] all messages echoed, done");
}
