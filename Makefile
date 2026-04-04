KERNEL_BIN = target/x86_64-unknown-none/debug/sumi-kernel
VM_BIN     = target/debug/sumi-vm

.PHONY: build test self-test

build:
	cargo build -p sumi-kernel --target x86_64-unknown-none
	cargo build -p sumi-vm

test:
	cargo test

self-test: build
	@scripts/self-test.sh
