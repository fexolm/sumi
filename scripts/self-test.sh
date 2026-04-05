#!/usr/bin/env bash
set -euo pipefail

SHARE_DIR=$(mktemp -d /tmp/sumi-selftest.XXXXXX)
trap 'rm -rf "$SHARE_DIR"' EXIT

cargo build -p sumi-kernel --target x86_64-unknown-none
cargo build -p sumi-vm

echo "--- kernel selftests ---"
target/debug/sumi-vm run --share "$SHARE_DIR" target/x86_64-unknown-none/debug/sumi-kernel < /dev/null
echo ""
