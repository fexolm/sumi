# virtio-console

Status: current implementation notes, synced with the codebase on 2026-07-04.

`sumi` uses a virtio-mmio console device for guest stdin/stdout. Debug logs still
have the architecture debug console path available for early boot and panic
cases.

## Code Map

- Host backend: `sumi-vm/src/devices/virtio_console.rs`.
- Guest driver: `sumi-kernel/src/drivers/virtio/console.rs`.
- Generic virtqueue code: `sumi-kernel/src/drivers/virtio/virtqueue.rs`.
- Device registration: `sumi-vm/src/devices/mod.rs` and
  `sumi-kernel/src/kernel_main.rs`.

## Device Shape

- Device 1 in the MMIO device band is virtio-console.
- Queue 0 is receive: host stdin to guest.
- Queue 1 is transmit: guest stdout/stderr to host stdout.
- The host backend processes queues synchronously during MMIO exits.
- The console device is always registered by the VM.

## Guest Use

The kernel initializes the console after virtio-fs. The syscall layer routes
guest fd 0/1/2 through the FD table defaults:

- fd 0 reads from the receive queue;
- fd 1 and fd 2 write to the transmit queue.

`kprintln!` uses the kernel printing path and remains suitable before the
virtio-console device is fully ready.

## Limits

- stdout and stderr both go to host stdout.
- Input is blocking at the host `stdin.read` boundary.
- No multiport console support.
- No interrupt-driven console completion; queue handling is synchronous.

## Tests

Coverage is provided by read/write syscall integration tests and by the glibc
stdio tests.
