# Networking Design

Status: **in progress** (Phase 1). This document is the plan of record for
adding TCP/IP networking to sumi, with the end goal of running a full
`mysqld` server under the VM and connecting to it from the host.

## Goal

Run an unmodified Linux `mysqld` x86_64 binary under sumi and accept client
connections over TCP. `mysqld` is an event-driven server: it opens listening
sockets, sets them non-blocking, and multiplexes I/O with `epoll`. So the
concrete capabilities we must deliver are:

1. The BSD socket syscall family (`socket`, `bind`, `listen`, `accept4`,
   `connect`, `send*`/`recv*`, `setsockopt`, `getsockname`, `shutdown`, ...).
2. A real TCP/IP stack behind those syscalls.
3. `epoll` (`epoll_create1`, `epoll_ctl`, `epoll_wait`) with correct
   edge/level readiness and blocking semantics.
4. A path for packets to leave and enter the VM so an external client can
   reach the guest.

`io_uring` is explicitly **out of scope** for the initial mysqld bring-up
(`mysqld` runs fine on `epoll`); it is a deferred phase.

## Architecture Decision: in-guest TCP/IP via smoltcp

The guest runs a real TCP/IP stack ([smoltcp](https://github.com/smoltcp-rs/smoltcp),
`no_std` + `alloc`). Socket syscalls manipulate smoltcp sockets; smoltcp owns
the TCP state machine, timers, retransmission, and windowing. Ethernet frames
move in and out through a virtio-net device.

Rejected alternative: host-proxied sockets (RPC each socket syscall to a real
host socket, like virtio-fs does for files). Simpler, but the user chose a
faithful in-guest stack.

```
  user program (mysqld / test)
        │  socket()/epoll_wait()/...  (Linux x86_64 syscalls)
        ▼
  syscall/handlers/net.rs, epoll.rs        ── thin translation layer
        │
        ▼
  net::stack  (smoltcp Interface + SocketSet + port/handle tables)
        │  ethernet frames
        ▼
  net::device   ── Phase 1: Loopback (in-guest)   Phase 2: VirtioNetDevice
        │
        ▼  (Phase 2 only)
  virtio-mmio RX/TX virtqueues  ⇄  host VirtioNet backend  ⇄  TAP device  ⇄  host network
```

## The critical constraint: synchronous virtqueue vs. blocking net ops

The existing virtqueue path (`drivers/virtio/virtqueue.rs`) is **synchronous
and poll-based**: the avail ring sets `VIRTQ_AVAIL_F_NO_INTERRUPT`, and the
host services the queue *inside the notify-MMIO vCPU exit*, so by the time the
guest's notify write returns, the used ring already holds the result. This is
correct for virtio-fs because file ops complete quickly on the host.

Networking breaks this model. `accept`, `recv` on an empty socket, `connect`,
and `epoll_wait` can block indefinitely. The host must **never** block inside
the MMIO exit — that would freeze the whole vCPU (and every thread the
scheduler has placed on it).

The design resolves this in two layers:

- **All socket data ops are non-blocking at the stack layer.** A socket
  syscall runs smoltcp against whatever is buffered right now and returns
  immediately, yielding `EAGAIN` when it would block. This is exactly how an
  event-driven server drives its non-blocking sockets, so it matches mysqld's
  usage and fits the synchronous virtqueue perfectly.

- **Blocking is the scheduler's job, not the device's.** When a thread must
  wait (a blocking-mode socket op, or `epoll_wait` with no ready fds), it
  registers interest and parks via the existing block/wake primitive
  (`ThreadState::Blocked` + `sched::wake_blocked`, the same mechanism futex
  uses). It is woken when readiness changes:
  - Phase 1 (loopback): every TX immediately drives the loopback so the peer
    becomes ready in the same or next stack poll; the timer tick also polls
    the stack for timeouts.
  - Phase 2 (virtio-net): an RX interrupt from the host wakes the net poller,
    which advances smoltcp and wakes any thread whose socket became ready.

This keeps the hard part (async completion) confined to *one* place — the
readiness/wakeup bridge between smoltcp and the scheduler — instead of
smearing it across every syscall.

## Reference implementation: Hermit

The [Hermit unikernel](https://github.com/hermit-os/kernel) integrates smoltcp
in a Rust `no_std` kernel, so it is our closest reference. Its net code lives
in `src/executor/{network,device}.rs`, `src/drivers/net/virtio/`, and
`src/fd/socket/`. We take its **smoltcp wiring verbatim** but **discard its
async layer**, because that layer only exists to work around a missing
blocking-thread primitive that sumi already has:

- Hermit has no way to block a thread, so it runs an async executor and, for
  every blocking syscall, calls `block_on(future)` — where `block_on` builds a
  per-call `TaskNotify` (an `AtomicU32` used exactly like a futex) and uses its
  `Waker` as the poll context. smoltcp's `register_recv_waker(cx.waker())` then
  stores that futex-waker inside the socket; `iface.poll` fires it when the
  socket becomes ready; `block_on` parks on the futex with a compare-and-block
  (`wait if still 0`) so a wake between poll and park is never lost.
- **`TaskNotify` is a hand-rolled futex.** sumi has the real thing
  (`ThreadState::Blocked` + `sched::wake_blocked`, with the same compare-and-CAS
  discipline as `sched::futex`). So we replace Hermit's
  executor/`block_on`/futures/per-socket-waker machinery with **per-socket wait
  queues of blocked thread IDs**, and drop smoltcp's `async` feature entirely.

What we copy from Hermit unchanged: the exact Cargo features; a single global
`NetworkInterface { iface, sockets, device }` behind one IRQ-disabling lock
(device stored *by value* inside so one guard borrows all three for
`iface.poll`); the `Instant::from_micros(monotonic_us())` clock shim and a
`rdtsc`/RNG-seeded ephemeral-port start; the phy `Device` RxToken/TxToken
pattern with RX-descriptor replenish and `max_burst_size`/`send_capacity`
backpressure (Phase 2); arming a one-shot timer from `iface.poll_delay()` so
TCP retransmit/TIME-WAIT/delayed-ACK fire while idle; and the readiness
predicates `can_recv || (may_recv && listen) → POLLIN`, `can_send → POLLOUT`,
closing → `POLLHUP`.

Note Hermit does **not** implement epoll (only POSIX `poll`); we must, since we
run Linux binaries. We build it on the same wait-queue substrate.

## smoltcp configuration

`sumi-kernel/Cargo.toml`:

```toml
smoltcp = { version = "0.13", default-features = false, features = [
    "alloc", "medium-ethernet", "proto-ipv4", "proto-ipv6",
    "socket-tcp", "socket-udp",
] }
```

`medium-ethernet` (virtio-net is an Ethernet device; loopback also runs as
Ethernet). No `std`, no `async`. Add fragmentation features only if needed.

## Module layout (`sumi-kernel/src/net/`)

- `net/mod.rs` — the global `NetworkInterface { iface, sockets, device }`
  behind one IRQ-safe lock (`spin`), `init`, and the `poll()` entry point.
  `poll()` is a **plain function** (no executor): it calls `iface.poll(now)`,
  then walks sockets whose readiness changed and `wake_blocked`s their waiters,
  and arms the net timer from `poll_delay()`. It is called from three sites:
  (a) the top of every blocking socket op (poll-before-block), (b) the net
  timer callback, and (c) Phase 2's RX IRQ path.
- `net/stack.rs` — smoltcp `Interface` + `SocketSet` construction, IP config,
  the monotonic-clock shim (`crate::time::monotonic_ns` → smoltcp `Instant`),
  and the ephemeral-port allocator.
- `net/device.rs` — smoltcp `Device` impls: smoltcp's built-in `Loopback`
  (Phase 1) and `VirtioNetDevice` (Phase 2).
- `net/socket.rs` — `SocketObject`: per-fd socket state (smoltcp
  `SocketHandle`(s), domain/type/protocol, `nonblocking` flag, bound/peer
  addrs) and its **wait queue of blocked TIDs**.
- `net/epoll.rs` — `EpollInstance` (interest set + ready computation), built on
  the same readiness predicates and wait queues.
- `net/wait.rs` — the readiness→thread wakeup helper shared by blocking socket
  ops and `epoll_wait`, mirroring the `sched::futex` discipline.

### Phase 2 note: one net-service thread, not IRQ-side poll

To avoid IRQ/lock reentrancy (a syscall thread may hold the `NetworkInterface`
lock when RX fires), Phase 2's RX IRQ will **not** run `iface.poll` directly.
It sets a needs-poll flag and wakes a dedicated kernel net-service thread that
owns the poll loop (the executor-free analog of Hermit's `network_run` task).
Phase 1 needs none of this — `poll()` runs synchronously from the syscall
thread on the loopback device.

FD integration: `fs::FdKind` gains `Socket { id }` and `Epoll { id }` variants
indexing into the net subsystem's own tables (kept out of `fs` so the file
layer stays unaware of sockets). `FdTable::alloc`/`free`/`get` are reused
unchanged; `close` routes to the net subsystem when the kind is a socket/epoll.

## Blocking / wakeup contract

`net::wait` mirrors the futex discipline exactly (which is already proven in
this codebase):

1. Under the net lock, the thread evaluates readiness. If ready, it proceeds
   without blocking.
2. If not ready, still under the net lock, it records `(thread, socket,
   interest)` in the wait registry and sets its own state to `Blocked`, then
   drops the lock and calls `schedule()`.
3. A waker (loopback TX progress, timer-driven `net::poll`, or Phase-2 RX IRQ)
   acquires the net lock, advances smoltcp, computes newly-ready sockets, and
   for each waiter whose interest is now satisfied does the
   `Blocked → Runnable` CAS + `wake_blocked` (which enqueues + kicks the home
   CPU). Because the readiness re-check and the state transition both happen
   under the net lock, no wakeup is lost.

Timeouts (`epoll_wait` timeout, `SO_RCVTIMEO`) reuse the nanosleep/timer
machinery to arm a wake.

## Syscall surface

Phase 1 (loopback) implements, in `handlers/net.rs` and a new
`handlers/epoll.rs`:

| nr  | syscall        | Phase 1 behavior                                  |
|-----|----------------|---------------------------------------------------|
| 41  | socket         | AF_INET/AF_INET6, SOCK_STREAM (+SOCK_NONBLOCK/CLOEXEC in type) |
| 49  | bind           | bind to addr/port (127.0.0.1 loopback in P1)      |
| 50  | listen         | mark listening, set backlog                       |
| 43  | accept         | = accept4 with flags 0                             |
| 288 | accept4        | pop a completed passive connection                |
| 42  | connect        | active open; non-blocking → EINPROGRESS           |
| 44/45 | sendto/recvfrom | TCP send/recv (addr ignored for connected TCP)  |
| 46/47 | sendmsg/recvmsg | iovec gather/scatter over the same path         |
| 48  | shutdown       | half-close                                        |
| 51/52 | getsockname/getpeername | report bound/peer addr                    |
| 54/55 | setsockopt/getsockopt | store/echo; honor the options mysqld needs |
| 291 | epoll_create1  | allocate an EpollInstance fd                      |
| 233 | epoll_ctl      | ADD/MOD/DEL an fd + event mask                    |
| 232 | epoll_wait     | (with 281 epoll_pwait) block until ready/timeout  |

Also wired as needed: `fcntl(F_SETFL, O_NONBLOCK)` for sockets (reuse existing
`handlers/io.rs` fcntl), `poll`/`select` extended to consult socket readiness,
and `eventfd2` (nr 290) since mysqld/glib may use it for wakeups.

## Phased plan

### Phase 0 — Design doc (this document). ✅ in progress

### Phase 1 — Socket + epoll over an in-guest loopback device
No host networking, no virtio-net, no IRQ work. Validates the entire
syscall + smoltcp + scheduler-blocking layer in isolation.
- Add `smoltcp` (`default-features = false`, `alloc`, `medium-ethernet` or
  `medium-ip`, `proto-ipv4`, `proto-ipv6`, `socket-tcp`) to `sumi-kernel`.
- Build `net::stack` on smoltcp's `Loopback` device with a fixed guest IP
  (e.g. 127.0.0.1/8 + 10.0.2.15/24 reserved for Phase 2).
- Implement the socket + epoll syscalls above.
- Wire the block/wake registry (`net::wait`) and drive `net::poll` from the
  timer tick and after every send.
- **Exit criterion / deliverable:** a single-process integration test
  (`data/syscalls/tcp_epoll_loopback.rs`) that: opens a listener on
  127.0.0.1, connects a client to it, uses `epoll_wait` to drive accept +
  bidirectional echo, verifies the round-trip, and `pass!()`es. This is the
  "test program using TCP sockets and epoll" milestone.

### Phase 2 — virtio-net driver + userspace host gateway (no root, no IRQ)
Give the guest real external reachability. Two environment realities shaped
this away from the original TAP+IRQ sketch:

- **No `CAP_NET_ADMIN`** in the dev/CI environment (and `sudo` is
  interactive), so a TAP device cannot be created autonomously. The host
  backend is therefore a **userspace gateway** (the QEMU "user net"/slirp
  model), which needs no privileges and works anywhere.
- **No host→guest IRQ needed for correctness.** `timer_interrupt()` already
  calls `net::poll()` every tick, so the guest drains the virtio-net **RX
  queue on each timer tick** (and on every socket syscall). A blocked thread
  is woken within one tick (~1 ms) when its data arrives. Interrupt-driven RX
  (clearing `NO_INTERRUPT` + a device IRQ vector + KVM IRQ injection) is
  deferred to a later latency optimization — it is not required to function.

Components:
- Guest `net/device.rs::VirtioNetDevice` implementing smoltcp's `Device` over
  RX/TX virtqueues (reuse `drivers/virtio` mmio/virtqueue infra). Production
  `NetState` uses it; host unit tests keep `Loopback`. Guest static IP
  `10.0.2.15/24`, gateway `10.0.2.2`. `Device::transmit` enqueues a frame on
  TX + notifies host; `Device::receive` pops from RX; both driven by
  `net::poll`.
- Host `sumi-vm/src/devices/virtio_net.rs`: virtio-net backend. On a TX
  notify it drains guest frames into the gateway; it fills the RX queue from
  the gateway.
- Host gateway (`slirp`-lite, smoltcp on the host `std` side): answers ARP for
  the gateway IP, and does bidirectional TCP port forwarding between real host
  sockets and guest TCP endpoints. `--hostfwd tcp:HOSTIP:HOSTPORT-GUESTIP:GUESTPORT`
  on `sumi-vm run`. This is what lets a real client (eventually the `mysql`
  client) reach a guest server on a forwarded host port.
- **Deliverable:** a host TCP client connects to a forwarded port and a guest
  echo server round-trips its bytes (and/or the guest connects out to a host
  peer), verified by an integration test that needs no root.

### Phase 3 — Broaden socket/syscall coverage for mysqld
Driven by actually launching mysqld and fixing what it hits.
- `setsockopt`/`getsockopt`: honor `SO_REUSEADDR`, `SO_KEEPALIVE`,
  `TCP_NODELAY`, `SO_RCVBUF`/`SO_SNDBUF`, `SO_ERROR`, `SO_RCVTIMEO`.
- `accept4` flags, `getsockname`/`getpeername` correctness, `IPV6_V6ONLY`.
- `eventfd2`, `signalfd4`/`ppoll` if mysqld's thread pool needs them.
- Sweep for any remaining `ENOSYS` syscalls mysqld issues at startup.

### Phase 4 — Run mysqld end-to-end
- Stage a mysqld binary + initialized datadir on the host, expose via
  virtio-fs (`--share`).
- Boot to "ready for connections", connect the `mysql` client from the host,
  run `SELECT 1` / create a table / insert / select.
- Iterate on crashes and missing features surfaced during bring-up.
- **Deliverable:** a scripted end-to-end mysqld test.

### Phase 5 (deferred) — io_uring
`io_uring_setup`/`io_uring_enter`/`io_uring_register`, the SQ/CQ ring mmap
contract, and the opcodes mysqld's io_uring path uses. Only after mysqld runs
on epoll.

## Open questions / risks

- **smoltcp on `x86_64-unknown-none`**: must build with `alloc` and no float;
  confirmed as the first implementation step before writing syscalls.
- **PAGE_SIZE is 2 MiB here**: virtqueue/DMA buffers for virtio-net must
  respect that; reuse the virtio-fs allocation pattern.
- **Host→guest IRQ injection** (Phase 2) is genuinely new plumbing.
- **Single shared address space**: the net tables are global (one "process"),
  which simplifies fd/socket ownership — there is no per-process socket table
  to isolate.
