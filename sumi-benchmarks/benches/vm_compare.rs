use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group};

const DEFAULT_FIB_STEPS: usize = 64;
const DEFAULT_THREAD_STACK_BYTES: usize = 64 * 1024;
const DEFAULT_MATMUL_DIM: usize = 128;
const DEFAULT_MATMUL_THREADS: usize = 64;
const DEFAULT_MATMUL_TILE_ROWS: usize = 1;
const DEFAULT_MATMUL_TILE_COLS: usize = 16;
const DEFAULT_IO_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_IO_BLOCK_BYTES: usize = 64 * 1024;
const DEFAULT_NET_CONNECTIONS: usize = 8;
const DEFAULT_NET_PACKETS: usize = 64;
const DEFAULT_NET_PAYLOAD_BYTES: usize = 128;
const DEFAULT_NET_PORT_BASE: u16 = 7788;
const DEFAULT_NET_PORT_SPAN: usize = 512;
const DEFAULT_WARMUP_MS: u64 = 500;
const DEFAULT_MEASURE_MS: u64 = 2_000;
const DEFAULT_SAMPLE_SIZE: usize = 10;

fn env_usize(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|&value| value > 0)
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|&value| value > 0)
        .unwrap_or(default)
}

fn env_u16(name: &str, default: u16) -> u16 {
    env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<u16>().ok())
        .filter(|&value| value > 0)
        .unwrap_or(default)
}

fn env_bool(name: &str, default: bool) -> bool {
    env::var(name)
        .ok()
        .map(|raw| {
            matches!(
                raw.as_str(),
                "1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON"
            )
        })
        .unwrap_or(default)
}

fn criterion_config() -> Criterion {
    Criterion::default()
        .warm_up_time(Duration::from_millis(env_u64(
            "SUMI_BENCH_WARMUP_MS",
            DEFAULT_WARMUP_MS,
        )))
        .measurement_time(Duration::from_millis(env_u64(
            "SUMI_BENCH_MEASURE_MS",
            DEFAULT_MEASURE_MS,
        )))
        .sample_size(env_usize("SUMI_BENCH_SAMPLE_SIZE", DEFAULT_SAMPLE_SIZE).max(10))
}

fn maybe_set_working_dir() {
    if let Some(cwd) = env::var_os("SUMI_BENCH_CWD") {
        env::set_current_dir(&cwd).expect("set SUMI_BENCH_CWD as current directory");
    }
}

fn thread_stack_bytes() -> usize {
    env_usize("SUMI_BENCH_THREAD_STACK_BYTES", DEFAULT_THREAD_STACK_BYTES)
}

fn threaded_fibonacci(steps: usize, stack_bytes: usize) -> u64 {
    let mut prev = 0u64;
    let mut curr = 1u64;

    for _ in 0..steps {
        let a = prev;
        let b = curr;
        let next = thread::Builder::new()
            .stack_size(stack_bytes)
            .spawn(move || a.wrapping_add(b))
            .expect("spawn fibonacci worker")
            .join()
            .expect("fibonacci worker panicked");
        prev = curr;
        curr = next;
    }

    curr
}

fn bench_threaded_fibonacci(c: &mut Criterion) {
    let steps = env_usize("SUMI_BENCH_FIB_STEPS", DEFAULT_FIB_STEPS);
    let stack_bytes = thread_stack_bytes();

    let mut group = c.benchmark_group("scheduler_fibonacci");
    group.throughput(Throughput::Elements(steps as u64));
    group.bench_with_input(
        BenchmarkId::new("thread_per_step", steps),
        &steps,
        |b, &steps| {
            b.iter(|| black_box(threaded_fibonacci(black_box(steps), black_box(stack_bytes))));
        },
    );
    group.finish();
}

fn make_matrix(dim: usize, seed: u64) -> Vec<f64> {
    (0..dim * dim)
        .map(|idx| {
            let mixed = (idx as u64)
                .wrapping_mul(6364136223846793005)
                .wrapping_add(seed)
                .rotate_left(17);
            ((mixed & 0xffff) as f64) / 65536.0
        })
        .collect()
}

fn transpose(matrix: &[f64], dim: usize) -> Vec<f64> {
    let mut transposed = vec![0.0; matrix.len()];
    for row in 0..dim {
        for col in 0..dim {
            transposed[col * dim + row] = matrix[row * dim + col];
        }
    }
    transposed
}

#[derive(Clone, Copy)]
struct MatmulTile {
    row_start: usize,
    row_end: usize,
    col_start: usize,
    col_end: usize,
}

fn matmul_tiles(dim: usize, tile_rows: usize, tile_cols: usize) -> Vec<MatmulTile> {
    let tile_rows = tile_rows.max(1);
    let tile_cols = tile_cols.max(1);
    let mut tiles = Vec::new();

    let mut row = 0;
    while row < dim {
        let row_end = (row + tile_rows).min(dim);
        let mut col = 0;
        while col < dim {
            let col_end = (col + tile_cols).min(dim);
            tiles.push(MatmulTile {
                row_start: row,
                row_end,
                col_start: col,
                col_end,
            });
            col = col_end;
        }
        row = row_end;
    }

    tiles
}

#[allow(clippy::needless_range_loop)]
fn matmul_tile_checksum(a: &[f64], b_transposed: &[f64], dim: usize, tile: MatmulTile) -> f64 {
    let mut local_sum = 0.0f64;
    for row in tile.row_start..tile.row_end {
        let a_row = &a[row * dim..(row + 1) * dim];
        for col in tile.col_start..tile.col_end {
            let b_col = &b_transposed[col * dim..(col + 1) * dim];
            let mut acc = 0.0f64;
            for k in 0..dim {
                acc += a_row[k] * b_col[k];
            }
            local_sum += acc;
        }
    }
    local_sum
}

fn matmul_thread_per_task(
    a: &[f64],
    b_transposed: &[f64],
    dim: usize,
    tiles: &[MatmulTile],
    stack_bytes: usize,
) -> f64 {
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(tiles.len());
        for &tile in tiles {
            handles.push(
                thread::Builder::new()
                    .stack_size(stack_bytes)
                    .spawn_scoped(scope, move || {
                        matmul_tile_checksum(a, b_transposed, dim, tile)
                    })
                    .expect("spawn matmul task"),
            );
        }

        handles
            .into_iter()
            .map(|handle| handle.join().expect("matmul task panicked"))
            .sum()
    })
}

enum PoolMessage {
    Work {
        tile: MatmulTile,
        result_tx: mpsc::Sender<f64>,
    },
    Shutdown,
}

struct MatmulPool {
    work_tx: mpsc::Sender<PoolMessage>,
    workers: Vec<thread::JoinHandle<()>>,
}

impl MatmulPool {
    fn new(
        a: Arc<Vec<f64>>,
        b_transposed: Arc<Vec<f64>>,
        dim: usize,
        workers: usize,
        stack_bytes: usize,
    ) -> Self {
        let (work_tx, work_rx) = mpsc::channel::<PoolMessage>();
        let work_rx = Arc::new(Mutex::new(work_rx));
        let mut handles = Vec::with_capacity(workers);

        for _ in 0..workers {
            let a = Arc::clone(&a);
            let b_transposed = Arc::clone(&b_transposed);
            let work_rx = Arc::clone(&work_rx);
            handles.push(
                thread::Builder::new()
                    .stack_size(stack_bytes)
                    .spawn(move || {
                        loop {
                            let message = {
                                work_rx
                                    .lock()
                                    .expect("matmul pool receiver poisoned")
                                    .recv()
                            };
                            match message {
                                Ok(PoolMessage::Work { tile, result_tx }) => {
                                    let checksum =
                                        matmul_tile_checksum(&a, &b_transposed, dim, tile);
                                    let _ = result_tx.send(checksum);
                                }
                                Ok(PoolMessage::Shutdown) | Err(_) => break,
                            }
                        }
                    })
                    .expect("spawn matmul pool worker"),
            );
        }

        Self {
            work_tx,
            workers: handles,
        }
    }

    fn compute(&self, tiles: &[MatmulTile]) -> f64 {
        let (result_tx, result_rx) = mpsc::channel();

        for &tile in tiles {
            self.work_tx
                .send(PoolMessage::Work {
                    tile,
                    result_tx: result_tx.clone(),
                })
                .expect("send matmul pool task");
        }
        drop(result_tx);

        let mut checksum = 0.0f64;
        for _ in 0..tiles.len() {
            checksum += result_rx.recv().expect("receive matmul pool result");
        }
        checksum
    }
}

impl Drop for MatmulPool {
    fn drop(&mut self) {
        for _ in &self.workers {
            let _ = self.work_tx.send(PoolMessage::Shutdown);
        }
        while let Some(worker) = self.workers.pop() {
            if !thread::panicking() {
                worker.join().expect("join matmul pool worker");
            }
        }
    }
}

fn bench_parallel_matmul(c: &mut Criterion) {
    let dim = env_usize("SUMI_BENCH_MATMUL_DIM", DEFAULT_MATMUL_DIM);
    let pool_threads = env_usize("SUMI_BENCH_MATMUL_THREADS", DEFAULT_MATMUL_THREADS);
    let tile_rows = env_usize("SUMI_BENCH_MATMUL_TILE_ROWS", DEFAULT_MATMUL_TILE_ROWS);
    let tile_cols = env_usize("SUMI_BENCH_MATMUL_TILE_COLS", DEFAULT_MATMUL_TILE_COLS);
    let stack_bytes = thread_stack_bytes();
    let a = Arc::new(make_matrix(dim, 0x9e37_79b9));
    let b = make_matrix(dim, 0xd1b5_4a32);
    let b_transposed = Arc::new(transpose(&b, dim));
    let tiles = matmul_tiles(dim, tile_rows, tile_cols);
    let tasks = tiles.len();
    let operations = (dim as u64) * (dim as u64) * (dim as u64);

    let mut group = c.benchmark_group("parallel_matmul");
    group.throughput(Throughput::Elements(operations));
    group.bench_function(
        BenchmarkId::new("thread_per_task", format!("{dim}x{dim}_tasks_{tasks}")),
        |bench| {
            bench.iter(|| {
                black_box(matmul_thread_per_task(
                    black_box(&a),
                    black_box(&b_transposed),
                    black_box(dim),
                    black_box(&tiles),
                    black_box(stack_bytes),
                ));
            });
        },
    );

    let pool = MatmulPool::new(
        Arc::clone(&a),
        Arc::clone(&b_transposed),
        dim,
        pool_threads,
        stack_bytes,
    );
    group.bench_function(
        BenchmarkId::new(
            "pool",
            format!("{dim}x{dim}_tasks_{tasks}_workers_{pool_threads}"),
        ),
        |bench| {
            bench.iter(|| {
                black_box(pool.compute(black_box(&tiles)));
            });
        },
    );
    group.finish();
}

fn io_dir() -> PathBuf {
    env::var_os("SUMI_BENCH_IO_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/sumi-benchmarks"))
}

fn io_block(size: usize) -> Vec<u8> {
    (0..size)
        .map(|idx| (idx as u8).wrapping_mul(31).wrapping_add(7))
        .collect()
}

fn write_pattern(
    path: &Path,
    total_bytes: usize,
    block: &[u8],
    sync_data: bool,
) -> io::Result<u64> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    let mut written = 0usize;

    while written < total_bytes {
        let chunk_len = block.len().min(total_bytes - written);
        file.write_all(&block[..chunk_len])?;
        written += chunk_len;
    }

    file.flush()?;
    if sync_data {
        file.sync_data()?;
    }
    Ok(written as u64)
}

fn read_all(path: &Path, block_size: usize) -> io::Result<u64> {
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; block_size];
    let mut bytes = 0u64;
    let mut checksum = 0u64;

    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        checksum = checksum.wrapping_add(buf[..n].iter().map(|&byte| u64::from(byte)).sum::<u64>());
    }

    black_box(checksum);
    Ok(bytes)
}

fn bench_file_io(c: &mut Criterion) {
    let total_bytes = env_usize("SUMI_BENCH_IO_BYTES", DEFAULT_IO_BYTES);
    let block_bytes = env_usize("SUMI_BENCH_IO_BLOCK_BYTES", DEFAULT_IO_BLOCK_BYTES);
    let sync_data = env_bool("SUMI_BENCH_IO_SYNC", false);
    let dir = io_dir();
    fs::create_dir_all(&dir).expect("create benchmark I/O directory");

    let block = io_block(block_bytes);
    let process_id = std::process::id();
    let read_path = dir.join(format!("read-{process_id}.bin"));
    let write_path = dir.join(format!("write-{process_id}.bin"));
    write_pattern(&read_path, total_bytes, &block, sync_data).expect("prepare read benchmark file");

    let label = format!(
        "{}MiB_block_{}KiB_sync_{}",
        total_bytes / 1024 / 1024,
        block_bytes / 1024,
        u8::from(sync_data)
    );
    let mut group = c.benchmark_group("file_io");
    group.throughput(Throughput::Bytes(total_bytes as u64));
    group.bench_function(BenchmarkId::new("write_truncate", &label), |bench| {
        bench.iter(|| {
            black_box(
                write_pattern(
                    black_box(&write_path),
                    black_box(total_bytes),
                    black_box(&block),
                    black_box(sync_data),
                )
                .expect("write benchmark file"),
            );
        });
    });
    group.bench_function(BenchmarkId::new("read_cached", &label), |bench| {
        bench.iter(|| {
            black_box(
                read_all(black_box(&read_path), black_box(block_bytes))
                    .expect("read benchmark file"),
            );
        });
    });
    group.finish();

    let _ = fs::remove_file(read_path);
    let _ = fs::remove_file(write_path);
}

fn connect_loopback(addr: &str) -> io::Result<TcpStream> {
    TcpStream::connect(addr)
}

fn spawn_io_thread<T, F>(
    label: &str,
    stack_bytes: usize,
    f: F,
) -> io::Result<thread::JoinHandle<io::Result<T>>>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    thread::Builder::new()
        .name(label.to_owned())
        .stack_size(stack_bytes)
        .spawn(f)
}

fn join_io<T>(handle: thread::JoinHandle<io::Result<T>>, label: &str) -> io::Result<T> {
    match handle.join() {
        Ok(result) => result,
        Err(_) => Err(io::Error::other(format!("{label} panicked"))),
    }
}

fn tcp_loopback_roundtrip(
    connections: usize,
    packets: usize,
    payload_bytes: usize,
    port: u16,
    stack_bytes: usize,
) -> io::Result<u64> {
    let addr = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&addr)?;
    let payload = io_block(payload_bytes);

    let server = spawn_io_thread("tcp server", stack_bytes, move || -> io::Result<u64> {
        let mut handlers = Vec::with_capacity(connections);

        for _ in 0..connections {
            let (mut stream, _) = listener.accept()?;
            handlers.push(spawn_io_thread(
                "tcp server worker",
                stack_bytes,
                move || -> io::Result<u64> {
                    let mut buf = vec![0u8; payload_bytes];
                    let mut bytes = 0u64;
                    for _ in 0..packets {
                        stream.read_exact(&mut buf)?;
                        stream.write_all(&buf)?;
                        bytes += (payload_bytes * 2) as u64;
                    }
                    Ok(bytes)
                },
            )?);
        }

        let mut total = 0u64;
        for handle in handlers {
            total += join_io(handle, "tcp server worker")?;
        }
        Ok(total)
    })?;

    let mut clients = Vec::with_capacity(connections);
    for _ in 0..connections {
        let addr = addr.clone();
        let payload = payload.clone();
        clients.push(spawn_io_thread(
            "tcp client",
            stack_bytes,
            move || -> io::Result<u64> {
                let mut stream = connect_loopback(&addr)?;
                let _ = stream.set_nodelay(true);
                let mut echoed = vec![0u8; payload.len()];
                let mut bytes = 0u64;

                for _ in 0..packets {
                    stream.write_all(&payload)?;
                    stream.read_exact(&mut echoed)?;
                    if echoed != payload {
                        return Err(io::Error::other("tcp echo payload mismatch"));
                    }
                    bytes += (payload.len() * 2) as u64;
                }

                Ok(bytes)
            },
        )?);
    }

    let mut client_bytes = 0u64;
    for handle in clients {
        client_bytes += join_io(handle, "tcp client")?;
    }
    let server_bytes = join_io(server, "tcp server")?;
    Ok(client_bytes + server_bytes)
}

fn bench_tcp_loopback(c: &mut Criterion) {
    let connections = env_usize("SUMI_BENCH_NET_CONNECTIONS", DEFAULT_NET_CONNECTIONS);
    let packets = env_usize("SUMI_BENCH_NET_PACKETS", DEFAULT_NET_PACKETS);
    let payload_bytes = env_usize("SUMI_BENCH_NET_PAYLOAD_BYTES", DEFAULT_NET_PAYLOAD_BYTES);
    let stack_bytes = thread_stack_bytes();
    let port_base = env_u16("SUMI_BENCH_NET_PORT_BASE", DEFAULT_NET_PORT_BASE);
    let max_span = usize::from(u16::MAX - port_base) + 1;
    let port_span = env_usize("SUMI_BENCH_NET_PORT_SPAN", DEFAULT_NET_PORT_SPAN)
        .min(max_span)
        .max(1);
    let port_offset = AtomicUsize::new(0);
    let bytes_per_iteration = (connections * packets * payload_bytes * 4) as u64;

    let mut group = c.benchmark_group("tcp_loopback");
    group.throughput(Throughput::Bytes(bytes_per_iteration));
    group.bench_function(
        BenchmarkId::new(
            "connections_packets_payload",
            format!("{connections}x{packets}x{payload_bytes}"),
        ),
        |bench| {
            bench.iter(|| {
                let offset = port_offset.fetch_add(1, Ordering::Relaxed) % port_span;
                let port = port_base + offset as u16;
                black_box(
                    tcp_loopback_roundtrip(
                        black_box(connections),
                        black_box(packets),
                        black_box(payload_bytes),
                        black_box(port),
                        black_box(stack_bytes),
                    )
                    .expect("tcp loopback benchmark"),
                );
            });
        },
    );
    group.finish();
}

criterion_group! {
    name = benches;
    config = criterion_config();
    targets =
        bench_threaded_fibonacci,
        bench_parallel_matmul,
        bench_file_io,
        bench_tcp_loopback
}

fn main() {
    maybe_set_working_dir();
    benches();
}
