KERNEL_BIN = target/x86_64-unknown-none/debug/sumi-kernel
VM_BIN     = target/debug/sumi-vm

.PHONY: build test integration-test all clean

build:
	cargo build -p sumi-kernel --target x86_64-unknown-none
	cargo build -p sumi-vm

# Host-side unit tests for every crate. The kernel runs its tests under the
# host target via #[cfg(test)] (see sumi-kernel/src/main.rs).
test:
	cargo test -p sumi-abi
	cargo test -p sumi-vm
	cargo test -p sumi-kernel

# End-to-end integration tests: each binary in sumi-integration-tests/data/
# is built and executed inside sumi-vm under KVM. Requires /dev/kvm and gcc.
integration-test: build
	cargo test -p sumi-integration-tests

all: build test integration-test

clean:
	cargo clean
