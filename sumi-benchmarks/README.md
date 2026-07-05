# sumi-benchmarks

Criterion benchmarks intended to run as the same static ELF on the host and
inside `sumi-vm`.

Build the benchmark binary:

```bash
cargo bench -p sumi-benchmarks --target x86_64-unknown-linux-musl --bench vm_compare --no-run
```

Find the produced binary:

```bash
BENCH_BIN=$(find target/x86_64-unknown-linux-musl/release/deps -maxdepth 1 -type f -executable -name 'vm_compare-*' | sort | tail -n1)
```

Run the host baseline:

```bash
SUMI_BENCH_CWD="$PWD" SUMI_BENCH_SAMPLE_SIZE=10 "$BENCH_BIN" \
  --bench --save-baseline host
```

Run the same ELF inside the VM and compare against the host baseline:

```bash
cargo build -p sumi-kernel --target x86_64-unknown-none
cargo build -p sumi-vm

target/debug/sumi-vm run \
  "$PWD/target/x86_64-unknown-none/debug/sumi-kernel" \
  --share / \
  --vcpus 4 \
  --env "SUMI_BENCH_CWD=$PWD" \
  --env SUMI_BENCH_SAMPLE_SIZE=10 \
  --run "$PWD/$BENCH_BIN" \
  -- --bench --baseline host
```

Criterion 0.5 cannot compare against one named baseline and save the same run
under another name in a single invocation. To save the guest run as a separate
baseline too, run the VM command a second time with
`-- --bench --save-baseline vm`.

Useful knobs:

- `SUMI_BENCH_FIB_STEPS` controls sequential Fibonacci steps, one freshly
  spawned thread per step. Default: `64`.
- `SUMI_BENCH_THREAD_STACK_BYTES` controls worker thread stack size. Default:
  `65536`.
- `SUMI_BENCH_MATMUL_DIM` controls square matrix size. Default: `128`.
- `parallel_matmul/pool/...` uses a persistent worker pool. `SUMI_BENCH_MATMUL_THREADS`
  controls pool workers. Default: `64`.
- `parallel_matmul/thread_per_task/...` creates one fresh thread per tile on every
  Criterion iteration; no pool is reused.
- `SUMI_BENCH_MATMUL_TILE_ROWS` and `SUMI_BENCH_MATMUL_TILE_COLS` control matmul
  task granularity. Defaults: `1` and `16`, so a 128x128 matmul creates 1024
  fresh threads in the no-pool benchmark.
- `SUMI_BENCH_IO_BYTES` controls file read/write size. Default: `8388608`.
- `SUMI_BENCH_IO_BLOCK_BYTES` controls file I/O chunk size. Default: `65536`.
- `SUMI_BENCH_IO_DIR` controls the file benchmark directory. Default:
  `/tmp/sumi-benchmarks`.
- `SUMI_BENCH_IO_SYNC=1` includes `sync_data()` in the write benchmark.
- `SUMI_BENCH_NET_CONNECTIONS` controls concurrent TCP connections. Default:
  `8`.
- `SUMI_BENCH_NET_PACKETS` controls round-trip packets per connection.
  Default: `64`.
- `SUMI_BENCH_NET_PAYLOAD_BYTES` controls packet payload size. Default: `128`.
- `SUMI_BENCH_NET_PORT_BASE` controls the first loopback port. Default:
  `7788`.
- `SUMI_BENCH_NET_PORT_SPAN` controls how many loopback ports are rotated
  across Criterion iterations. Default: `512`.
- `SUMI_BENCH_WARMUP_MS`, `SUMI_BENCH_MEASURE_MS`, and
  `SUMI_BENCH_SAMPLE_SIZE` tune Criterion runtime. Defaults: `500`, `2000`,
  and `10`.
- `SUMI_BENCH_CWD` sets the process cwd before Criterion starts. Set it to the
  workspace root for VM runs so host and guest share the same
  `target/criterion` baselines.
